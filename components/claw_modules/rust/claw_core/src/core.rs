//! The agent core object and loop, port of `claw_core.c` and
//! `claw_core_agent_loop.c`.
//!
//! `Core` owns the plumbing ([`CoreState`]), the LLM runtime, the injected
//! callbacks, and the context providers / completion observers. [`Core::start`]
//! spawns a `std::thread` running [`Core::run`] (the C `claw_core_agent_loop_task`,
//! which used a `claw_task` FreeRTOS task; the external `claw_task` C ABI is kept
//! separately for other consumers).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_ERR_NO_MEM, ESP_ERR_NOT_SUPPORTED,
    ESP_FAIL, ESP_OK,
};
use claw_interfaces::event::EventPublisher;
use claw_interfaces::http::ClawHttp;

use crate::callbacks::{
    CapCaller, CompletionObserver, CompletionSummary, ContextProvider, GateOutcome, PersistContext,
    RequestGate, RequestStart, StageNote,
};
use crate::consts::{
    AgentLoopPhase, CompletionType, ResponseStatus, DEFAULT_REQUEST_Q, DEFAULT_RESPONSE_Q,
    DEFAULT_STACK_SIZE, DEFAULT_TOOL_ITERATIONS, INSERT_QUEUE_LEN, MAX_COMPLETION_OBSERVERS,
    REQUEST_FLAG_SKIP_RESPONSE_QUEUE,
};
use crate::context::{
    append_assistant_tool_calls, append_tool_results_messages, append_user_message,
    build_context_failure_trace, build_iteration_context, cached_contexts_have_messages,
    collect_request_start_only_contexts, log_context_persist_failure,
    persist_context_final_if_configured, persist_context_tool_round_if_configured,
    persist_context_user_messages_if_configured,
};
use crate::core_state::CoreState;
use crate::errname::esp_err_to_name;
use crate::events::publish_out_message_if_requested;
#[cfg(feature = "stage_verbose")]
use crate::events::publish_stage_text;
use claw_api::{ChatError, ChatRequest, ClawApi, ClawApiConfig, InitError, LlmResponse};
use crate::request::RequestItem;
use crate::response::ResponseItem;
use crate::util::obs_csv_append;

/// Rust-side construction inputs (the resolved `claw_core_config_t` plus the
/// injected dependencies). The C ABI layer builds this from the C config.
pub struct CoreConfig {
    pub instance_id: u32,
    pub system_prompt: String,
    pub runtime_config: ClawApiConfig,
    pub http: Arc<dyn ClawHttp>,
    pub call_cap: Option<Box<dyn CapCaller>>,
    pub persist_context: Option<Box<dyn PersistContext>>,
    pub request_gate: Option<Box<dyn RequestGate>>,
    pub on_request_start: Option<Box<dyn RequestStart>>,
    pub collect_stage_note: Option<Box<dyn StageNote>>,
    pub events: Option<Box<dyn EventPublisher>>,
    pub max_tool_iterations: u32,
    pub request_queue_len: u32,
    pub response_queue_len: u32,
    pub max_context_providers: usize,
    /// Worker-task stack size in bytes. The C build placed this stack in PSRAM
    /// via `xTaskCreatePinnedToCoreWithCaps`; the Rust worker is a `std::thread`
    /// (pthread), so we apply the same size + PSRAM caps through `esp_pthread`.
    pub task_stack_size: u32,
    pub task_priority: u32,
    pub task_core: i32,
}

/// Maps a [`claw_api::errors::InitError`] to the `esp_err_t` this component
/// reports across its C ABI (`claw-api` itself is platform-agnostic).
fn init_error_code(err: &InitError) -> EspErr {
    match err {
        InitError::MissingApiKey
        | InitError::MissingModel
        | InitError::MissingBaseUrl
        | InitError::MissingBackendType => ESP_ERR_INVALID_ARG,
        InitError::UnknownBackend => ESP_ERR_NOT_SUPPORTED,
    }
}

/// Maps a [`claw_api::errors::ChatError`] to the `esp_err_t` this component
/// reports across its C ABI.
fn chat_error_code(err: &ChatError) -> EspErr {
    match err {
        ChatError::ToolsUnsupported => ESP_ERR_NOT_SUPPORTED,
        ChatError::InvalidToolsJson => ESP_ERR_INVALID_ARG,
        ChatError::Api(_) => ESP_FAIL,
    }
}

/// Distinguishes a normal tool-loop completion (run the success/persist/observer
/// block) from a `goto finish_request` (skip it). Mirrors the C control flow.
enum TurnOutcome {
    Completed,
    Finished(EspErr),
}

pub struct Core {
    pub state: CoreState,
    log_tag: String,
    system_prompt: String,
    llm: ClawApi,
    providers: Mutex<Vec<Box<dyn ContextProvider>>>,
    provider_capacity: usize,
    observers: Mutex<Vec<Box<dyn CompletionObserver>>>,
    call_cap: Option<Box<dyn CapCaller>>,
    persist_context: Option<Box<dyn PersistContext>>,
    request_gate: Option<Box<dyn RequestGate>>,
    on_request_start: Option<Box<dyn RequestStart>>,
    collect_stage_note: Option<Box<dyn StageNote>>,
    events: Option<Box<dyn EventPublisher>>,
    task: Mutex<Option<JoinHandle<()>>>,
    task_stack_size: u32,
    task_priority: u32,
    task_core: i32,
}

impl Core {
    /// `claw_core_create`.
    pub fn create(config: CoreConfig) -> Result<Arc<Core>, EspErr> {
        check_timezone();

        let llm = ClawApi::init(config.runtime_config, config.http).map_err(|e| {
            log::error!("LLM init failed: {e}");
            init_error_code(&e)
        })?;

        let request_q = if config.request_queue_len > 0 {
            config.request_queue_len
        } else {
            DEFAULT_REQUEST_Q
        } as usize;
        let response_q = if config.response_queue_len > 0 {
            config.response_queue_len
        } else {
            DEFAULT_RESPONSE_Q
        } as usize;
        let max_iter = if config.max_tool_iterations > 0 {
            config.max_tool_iterations
        } else {
            DEFAULT_TOOL_ITERATIONS
        };

        let log_tag = format!("claw_core_agent_{}", config.instance_id);
        let core = Core {
            state: CoreState::new(config.instance_id, request_q, response_q, max_iter),
            log_tag,
            system_prompt: config.system_prompt,
            llm,
            providers: Mutex::new(Vec::new()),
            provider_capacity: config.max_context_providers,
            observers: Mutex::new(Vec::new()),
            call_cap: config.call_cap,
            persist_context: config.persist_context,
            request_gate: config.request_gate,
            on_request_start: config.on_request_start,
            collect_stage_note: config.collect_stage_note,
            events: config.events,
            task: Mutex::new(None),
            task_stack_size: if config.task_stack_size > 0 {
                config.task_stack_size
            } else {
                DEFAULT_STACK_SIZE
            },
            task_priority: config.task_priority,
            task_core: config.task_core,
        };
        log::info!("{} Initialized", core.log_tag);
        Ok(Arc::new(core))
    }

    /// `claw_core_add_context_provider`.
    pub fn add_context_provider(&self, provider: Box<dyn ContextProvider>) -> EspErr {
        if !self.state.initialized.load(Ordering::Acquire) || self.state.started.load(Ordering::Acquire)
        {
            return ESP_ERR_INVALID_STATE;
        }
        let mut providers = self.providers.lock().unwrap();
        if providers.len() >= self.provider_capacity {
            return ESP_ERR_NO_MEM;
        }
        providers.push(provider);
        ESP_OK
    }

    /// `claw_core_add_completion_observer`.
    pub fn add_completion_observer(&self, observer: Box<dyn CompletionObserver>) -> EspErr {
        if !self.state.initialized.load(Ordering::Acquire) {
            return ESP_ERR_INVALID_STATE;
        }
        let mut observers = self.observers.lock().unwrap();
        if observers.len() >= MAX_COMPLETION_OBSERVERS {
            return ESP_ERR_NO_MEM;
        }
        observers.push(observer);
        ESP_OK
    }

    /// `claw_core_start`.
    pub fn start(self: &Arc<Self>) -> EspErr {
        if !self.state.initialized.load(Ordering::Acquire) {
            return ESP_ERR_INVALID_STATE;
        }
        if self.state.started.load(Ordering::Acquire) {
            return ESP_OK;
        }
        self.state.started.store(true, Ordering::Release);
        self.state.stop_requested.store(false, Ordering::Release);

        let core = Arc::clone(self);

        // The C agent task ran on a PSRAM-backed FreeRTOS stack
        // (`xTaskCreatePinnedToCoreWithCaps` + `MALLOC_CAP_SPIRAM`). A bare
        // `std::thread` uses the small default pthread stack in internal RAM and
        // overflows, so spawn through the shared helper that applies the stack
        // size + PSRAM caps via `esp_pthread`.
        let spawn = claw_sys::thread::spawn_worker(
            &self.log_tag,
            self.task_stack_size as usize,
            self.task_priority,
            self.task_core,
            move || core.run(),
        );
        match spawn {
            Ok(handle) => {
                *self.task.lock().unwrap() = Some(handle);
                log::info!("{} Started worker task", self.log_tag);
                ESP_OK
            }
            Err(_) => {
                self.state.started.store(false, Ordering::Release);
                ESP_FAIL
            }
        }
    }

    /// `claw_core_stop`.
    pub fn stop(&self, _timeout_ms: u32) -> EspErr {
        if !self.state.initialized.load(Ordering::Acquire) {
            return ESP_ERR_INVALID_STATE;
        }
        if !self.state.started.load(Ordering::Acquire) {
            return ESP_OK;
        }
        self.state.stop_requested.store(true, Ordering::Release);
        let _ = self.state.cancel_request(0);
        // Wake the loop if it is blocked on the request queue.
        let _ = self
            .state
            .request_queue
            .send_timeout(RequestItem::default(), Some(Duration::from_millis(0)));

        let handle = self.task.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        ESP_OK
    }

    /// `claw_core_submit`.
    pub fn submit(&self, request: RequestItem, timeout_ms: u32) -> EspErr {
        self.state.ingress_submit(request, timeout_ms)
    }

    /// `claw_core_receive_for` / `claw_core_receive`.
    pub fn receive_for(&self, request_id: u32, timeout_ms: u32) -> Result<ResponseItem, EspErr> {
        self.state.response_receive_for(request_id, timeout_ms)
    }

    /// `claw_core_cancel_request`.
    pub fn cancel_request(&self, request_id: u32) -> EspErr {
        self.state.cancel_request(request_id)
    }

    /// `claw_core_llm_infer_media` — run a one-shot media inference on this
    /// core's LLM runtime (used by `cap_llm_inspect`). Unlike the agent loop
    /// there is no per-request abort, so a fresh, un-aborted flag is passed.
    pub fn infer_media(
        &self,
        request: &claw_api::MediaRequest,
    ) -> Result<String, claw_api::InferMediaError> {
        let abort = std::sync::atomic::AtomicBool::new(false);
        self.llm.infer_media(request, &abort)
    }

    /// `claw_core_get_agent_loop_phase`.
    pub fn get_agent_loop_phase(&self) -> AgentLoopPhase {
        self.state.get_phase()
    }

    /// Whether the loop thread is running.
    pub fn is_started(&self) -> bool {
        self.state.started.load(Ordering::Acquire)
    }

    // --- agent loop -------------------------------------------------------

    /// `claw_core_agent_loop_task`.
    fn run(&self) {
        while !self.state.stop_requested.load(Ordering::Acquire) {
            let request = match self.state.request_queue.recv_timeout(None) {
                Some(r) => r,
                None => continue,
            };
            if self.state.stop_requested.load(Ordering::Acquire) {
                break;
            }
            self.run_one_turn(request);
        }
        log::info!("{} Stopped worker task", self.log_tag);
        self.state.started.store(false, Ordering::Release);
    }

    fn run_one_turn(&self, request: RequestItem) {
        self.state
            .begin_turn(request.request_id, request.session_id_str());

        let mut response = ResponseItem {
            request_id: request.request_id,
            status: ResponseStatus::Error,
            completion_type: CompletionType::Done,
            target_channel: request.target_channel.clone(),
            target_chat_id: request.target_chat_id.clone(),
            text: None,
            error_message: None,
        };

        let mut tool_summary = String::new();
        let mut obs_providers_csv = String::new();
        let mut obs_tool_calls_csv = String::new();
        let mut last_raw_message_json: Option<String> = None;

        let outcome = self.run_turn_body(
            &request,
            &mut response,
            &mut tool_summary,
            &mut obs_providers_csv,
            &mut obs_tool_calls_csv,
            &mut last_raw_message_json,
        );

        let err = match outcome {
            TurnOutcome::Completed => {
                // Success block: only reached on a normal tool-loop break.
                if response.text.is_some() {
                    response.status = ResponseStatus::Ok;
                    let persist_err = persist_context_final_if_configured(
                        self.persist_context.as_deref(),
                        &request,
                        last_raw_message_json.as_deref(),
                        response.text.as_deref(),
                    );
                    log_context_persist_failure(&request, "persist_context_final", persist_err);
                    self.run_completion_observers(
                        &request,
                        &response,
                        &obs_providers_csv,
                        &obs_tool_calls_csv,
                    );
                } else if response.error_message.is_none() {
                    response.error_message = Some(esp_err_to_name(ESP_OK).to_string());
                }
                ESP_OK
            }
            TurnOutcome::Finished(err) => err,
        };

        self.finish_request(&request, &mut response, &tool_summary, err);
    }

    /// The tool loop and its setup, returning how to finalize. `response`,
    /// `tool_summary`, the obs CSVs and `last_raw_message_json` are filled in.
    fn run_turn_body(
        &self,
        request: &RequestItem,
        response: &mut ResponseItem,
        tool_summary: &mut String,
        obs_providers_csv: &mut String,
        obs_tool_calls_csv: &mut String,
        last_raw_message_json: &mut Option<String>,
    ) -> TurnOutcome {
        if let Some(gate) = self.request_gate.as_deref() {
            match gate.gate(request) {
                GateOutcome::Allow => {}
                GateOutcome::Reject(message) => {
                    response.status = ResponseStatus::Ok;
                    response.text = Some(message);
                    return TurnOutcome::Finished(ESP_OK);
                }
                GateOutcome::Error(err) => {
                    response.error_message = Some(esp_err_to_name(err).to_string());
                    return TurnOutcome::Finished(err);
                }
            }
        }

        if let Some(start) = self.on_request_start.as_deref() {
            let err = start.on_start(request);
            if err != ESP_OK {
                log::warn!(
                    "request_start request={} failed: {}",
                    request.request_id,
                    esp_err_to_name(err)
                );
            }
        }

        let persist = self.persist_context.as_deref();
        let original_user_persisted = {
            let texts = [request.user_text.as_str()];
            match persist_context_user_messages_if_configured(persist, request, &texts) {
                Ok(persisted) => persisted,
                Err(err) => {
                    log_context_persist_failure(request, "persist_context_user", err);
                    false
                }
            }
        };

        let mut runtime_messages = Value::Array(Vec::new());

        let providers = self.providers.lock().unwrap();
        let providers: &[Box<dyn ContextProvider>] = &providers;

        let request_start_contexts = match collect_request_start_only_contexts(providers, request) {
            Ok(contexts) => contexts,
            Err(err) => {
                response.error_message = Some(esp_err_to_name(err).to_string());
                return TurnOutcome::Finished(err);
            }
        };

        let mut inject_active_user = true;
        if original_user_persisted && cached_contexts_have_messages(&request_start_contexts) {
            inject_active_user = false;
        }

        let mut iteration: u32 = 0;
        loop {
            self.state
                .set_phase(AgentLoopPhase::BeforeBuildIterationContext);
            match self.handle_pending_user_interrupts(
                request,
                "before_build_iteration_context",
                &mut runtime_messages,
            ) {
                Err(err) => {
                    response.error_message = Some(esp_err_to_name(err).to_string());
                    return TurnOutcome::Finished(err);
                }
                Ok(true) => continue,
                Ok(false) => {}
            }

            self.state
                .set_phase(AgentLoopPhase::BuildingIterationContext);
            let built = match build_iteration_context(
                &self.system_prompt,
                providers,
                request,
                &runtime_messages,
                &request_start_contexts,
                inject_active_user,
                obs_providers_csv,
            ) {
                Ok(built) => built,
                Err(err) => {
                    response.error_message = Some(esp_err_to_name(err).to_string());
                    return TurnOutcome::Finished(err);
                }
            };

            self.state.set_phase(AgentLoopPhase::BeforeLlmHttp);
            match self.handle_pending_user_interrupts(
                request,
                "before_llm_http",
                &mut runtime_messages,
            ) {
                Err(err) => {
                    response.error_message = Some(esp_err_to_name(err).to_string());
                    return TurnOutcome::Finished(err);
                }
                Ok(true) => continue,
                Ok(false) => {}
            }

            self.state.set_phase(AgentLoopPhase::InLlmHttp);
            let chat_request = ChatRequest {
                system_prompt: &built.system_prompt,
                messages: &built.messages,
                tools_json: built.tools_json.as_deref(),
            };
            let llm_response = match self.llm.chat(&chat_request, &self.state.abort_flag) {
                Ok(resp) => resp,
                Err(llm_err) => {
                    if self
                        .state
                        .take_user_interrupt_http_abort(request.request_id)
                    {
                        match self.handle_pending_user_interrupts(
                            request,
                            "in_llm_http_abort",
                            &mut runtime_messages,
                        ) {
                            Err(err) => {
                                response.error_message = Some(esp_err_to_name(err).to_string());
                                return TurnOutcome::Finished(err);
                            }
                            Ok(true) => continue,
                            Ok(false) => {}
                        }
                    }
                    response.error_message = Some(llm_err.to_string());
                    return TurnOutcome::Finished(chat_error_code(&llm_err));
                }
            };

            *last_raw_message_json = llm_response.raw_message_json.clone();

            if llm_response.tool_calls.is_empty() {
                self.state.set_phase(AgentLoopPhase::Finalizing);
                self.publish_stage_note_for_round(request, iteration);
                self.finish_from_plain_text(request.request_id, &llm_response, response);
                return TurnOutcome::Completed;
            }

            self.state.set_phase(AgentLoopPhase::AfterLlmBeforeTool);
            match self.handle_pending_user_interrupts(
                request,
                "after_llm_before_tool",
                &mut runtime_messages,
            ) {
                Err(err) => {
                    response.error_message = Some(esp_err_to_name(err).to_string());
                    return TurnOutcome::Finished(err);
                }
                Ok(true) => continue,
                Ok(false) => {}
            }

            self.state.set_phase(AgentLoopPhase::RunningTool);
            self.log_tool_call_names(request.request_id, &llm_response);
            self.publish_stage_tool_calls(request, &llm_response, iteration);
            for tc in &llm_response.tool_calls {
                obs_csv_append(obs_tool_calls_csv, &tc.name, false);
            }

            let assistant_tool_message_json = llm_response.raw_message_json.clone();
            let err = append_assistant_tool_calls(&mut runtime_messages, &llm_response);
            if err != ESP_OK {
                response.error_message = Some(esp_err_to_name(err).to_string());
                return TurnOutcome::Finished(err);
            }

            let call_cap = match self.call_cap.as_deref() {
                Some(cap) => cap,
                None => {
                    response.error_message =
                        Some(esp_err_to_name(ESP_ERR_INVALID_STATE).to_string());
                    return TurnOutcome::Finished(ESP_ERR_INVALID_STATE);
                }
            };
            let tool_results_json = match append_tool_results_messages(
                call_cap,
                &mut runtime_messages,
                &llm_response,
                request,
                tool_summary,
            ) {
                Ok(json) => json,
                Err(err) => {
                    response.error_message = Some(esp_err_to_name(err).to_string());
                    return TurnOutcome::Finished(err);
                }
            };

            if !tool_results_json.is_empty() {
                if let Some(raw) = assistant_tool_message_json.as_deref() {
                    let persist_err = persist_context_tool_round_if_configured(
                        persist,
                        request,
                        raw,
                        &tool_results_json,
                    );
                    if persist_err != ESP_OK {
                        log::warn!(
                            "persist_context_tool_round failed for request={} iteration={}: {}",
                            request.request_id,
                            iteration,
                            esp_err_to_name(persist_err)
                        );
                    }
                }
            }

            iteration += 1;
            if iteration >= self.state.max_tool_iterations {
                response.error_message = Some("cap tool iteration limit reached".to_string());
                return TurnOutcome::Finished(ESP_ERR_INVALID_STATE);
            }
        }
    }

    /// `claw_core_finish_from_plain_text`.
    fn finish_from_plain_text(
        &self,
        request_id: u32,
        llm_response: &LlmResponse,
        response: &mut ResponseItem,
    ) {
        let text = llm_response.text.clone().unwrap_or_default();
        response.completion_type = CompletionType::Done;
        response.error_message = None;
        log::info!(
            "completion request={} status=done raw={}",
            request_id,
            crate::util::truncate_for_log(&text)
        );
        response.text = Some(text);
    }

    /// `handle_pending_user_interrupts`. Returns `Ok(true)` when inputs were
    /// drained (the loop should `continue`).
    fn handle_pending_user_interrupts(
        &self,
        request: &RequestItem,
        timing_point: &str,
        runtime_messages: &mut Value,
    ) -> Result<bool, EspErr> {
        let texts = self
            .state
            .dequeue_inserted_user_inputs(request.session_id_str(), INSERT_QUEUE_LEN);
        if texts.is_empty() {
            return Ok(false);
        }
        self.state.clear_user_interrupt_abort(request.request_id);

        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let persisted = match persist_context_user_messages_if_configured(
            self.persist_context.as_deref(),
            request,
            &refs,
        ) {
            Ok(persisted) => persisted,
            Err(err) => {
                log_context_persist_failure(request, "persist_context_user_interrupt", err);
                false
            }
        };
        log::info!(
            "user_interrupt_triggered request={} timing={} count={} persisted={}",
            request.request_id,
            timing_point,
            texts.len(),
            persisted
        );

        for text in &texts {
            let err = append_user_message(runtime_messages, text);
            if err != ESP_OK {
                return Err(err);
            }
        }
        Ok(true)
    }

    fn run_completion_observers(
        &self,
        request: &RequestItem,
        response: &ResponseItem,
        obs_providers_csv: &str,
        obs_tool_calls_csv: &str,
    ) {
        let observers = self.observers.lock().unwrap();
        if observers.is_empty() {
            return;
        }
        let session = request.session_id_str();
        let summary = CompletionSummary {
            request_id: request.request_id,
            session_id: if session.is_empty() { None } else { Some(session) },
            final_text: response.text.as_deref(),
            context_providers_csv: obs_providers_csv,
            tool_calls_csv: obs_tool_calls_csv,
        };
        for observer in observers.iter() {
            observer.on_complete(&summary);
        }
    }

    /// The shared tail of every turn (the C `finish_request:` block).
    fn finish_request(
        &self,
        request: &RequestItem,
        response: &mut ResponseItem,
        tool_summary: &str,
        err: EspErr,
    ) {
        self.state.set_phase(AgentLoopPhase::Finalizing);
        let was_cancelled = self.state.end_turn();
        if was_cancelled && err != ESP_OK && response.error_message.is_some() {
            response.error_message = Some("request cancelled".to_string());
        }

        if err != ESP_OK {
            log::error!(
                "request={} failed: {}",
                request.request_id,
                response
                    .error_message
                    .as_deref()
                    .unwrap_or_else(|| esp_err_to_name(err))
            );
            if self.persist_context.is_some()
                && !request.session_id_str().is_empty()
                && !request.user_text.is_empty()
            {
                let trace = build_context_failure_trace(response.error_message.as_deref(), tool_summary);
                let persist_err = persist_context_final_if_configured(
                    self.persist_context.as_deref(),
                    request,
                    None,
                    Some(&trace),
                );
                log_context_persist_failure(request, "persist_context_failure_note", persist_err);
            }
        }

        if let Some(events) = self.events.as_deref() {
            publish_out_message_if_requested(events, request, response);
        }

        if request.flags & REQUEST_FLAG_SKIP_RESPONSE_QUEUE != 0 {
            return;
        }
        self.state.response_push(response.clone());
    }

    /// `claw_core_log_tool_call_names`.
    fn log_tool_call_names(&self, request_id: u32, response: &LlmResponse) {
        if response.tool_calls.is_empty() {
            return;
        }
        let names: Vec<&str> = response
            .tool_calls
            .iter()
            .map(|tc| if tc.name.is_empty() { "(null)" } else { tc.name.as_str() })
            .collect();
        log::debug!(
            "llm_tool_calls request={} count={} names={}",
            request_id,
            response.tool_calls.len(),
            names.join(",")
        );
    }

    /// `claw_core_publish_stage_note_for_round`. The stage-note callback is
    /// always invoked (for its side effects); publishing is verbose-only.
    fn publish_stage_note_for_round(&self, request: &RequestItem, round_index: u32) {
        let collector = match self.collect_stage_note.as_deref() {
            Some(c) => c,
            None => return,
        };
        let note = match collector.collect(request) {
            Ok(note) => note,
            Err(err) => {
                log::warn!("stage_note callback failed: {}", esp_err_to_name(err));
                return;
            }
        };
        #[cfg(feature = "stage_verbose")]
        if let Some(events) = self.events.as_deref() {
            if let Some(text) = note.as_deref().filter(|s| !s.is_empty()) {
                let buf = format!("\u{1F99E} [Round {}] {}", round_index + 1, text);
                let _ = publish_stage_text(events, request, &buf);
            }
        }
        #[cfg(not(feature = "stage_verbose"))]
        {
            let _ = note;
            let _ = round_index;
        }
    }

    /// `claw_core_publish_stage_tool_calls` (verbose-only).
    fn publish_stage_tool_calls(
        &self,
        request: &RequestItem,
        response: &LlmResponse,
        iteration: u32,
    ) {
        #[cfg(feature = "stage_verbose")]
        {
            if response.tool_calls.is_empty() {
                return;
            }
            let events = match self.events.as_deref() {
                Some(e) => e,
                None => return,
            };
            let mut buf = format!("\u{1F99E} [Round {}] Snap: ", iteration + 1);
            for (i, tc) in response.tool_calls.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                let name = if tc.name.is_empty() { "?" } else { tc.name.as_str() };
                if tc.arguments_json.is_empty() {
                    buf.push_str(name);
                } else {
                    let args: String = tc.arguments_json.chars().take(40).collect();
                    let ellipsis = if tc.arguments_json.len() > 40 { "..." } else { "" };
                    buf.push_str(&format!("{name}({args}{ellipsis})"));
                }
            }
            let _ = publish_stage_text(events, request, &buf);
        }
        #[cfg(not(feature = "stage_verbose"))]
        {
            let _ = (request, response, iteration);
        }
    }
}

/// `claw_core_check_timezone`.
fn check_timezone() {
    let tz = std::env::var("TZ").unwrap_or_default();
    if tz.is_empty() {
        log::warn!("Timezone is not configured; time-related responses may use an unexpected timezone");
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::ProviderOutcome;
    use crate::consts::{ContextKind, REQUEST_FLAG_USER_INTERRUPT};
    use claw_api::ClawApiConfig;
    use claw_interfaces::error::ESP_OK;
    use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;

    /// HTTP mock that returns canned bodies in order.
    struct ScriptedHttp {
        bodies: StdMutex<VecDeque<String>>,
    }
    impl ClawHttp for ScriptedHttp {
        fn post_json(
            &self,
            _request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            let body = self
                .bodies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"choices":[{"message":{"role":"assistant","content":"fallback"}}]}"#.into());
            Ok(HttpResponse { status_code: 200, body })
        }
    }

    struct EchoCap;
    impl CapCaller for EchoCap {
        fn call_cap(&self, cap_name: &str, _input: &str, _req: &RequestItem) -> (EspErr, Option<String>) {
            (ESP_OK, Some(format!("{cap_name} done")))
        }
    }

    struct StaticProvider {
        name: String,
        flags: u32,
        kind: ContextKind,
        content: String,
    }
    impl ContextProvider for StaticProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn flags(&self) -> u32 {
            self.flags
        }
        fn collect(&self, _request: &RequestItem) -> ProviderOutcome {
            ProviderOutcome::Provided {
                kind: self.kind,
                content: self.content.clone(),
            }
        }
    }

    fn base_config(bodies: Vec<String>) -> CoreConfig {
        CoreConfig {
            instance_id: 1,
            system_prompt: "You are a test agent.".into(),
            runtime_config: ClawApiConfig {
                api_key: Some("sk-test".into()),
                backend_type: "openai_compatible".into(),
                model: Some("gpt-test".into()),
                base_url: Some("https://example.invalid".into()),
                supports_tools: true,
                ..Default::default()
            },
            http: Arc::new(ScriptedHttp {
                bodies: StdMutex::new(bodies.into_iter().collect()),
            }),
            call_cap: Some(Box::new(EchoCap)),
            persist_context: None,
            request_gate: None,
            on_request_start: None,
            collect_stage_note: None,
            events: None,
            max_tool_iterations: 5,
            request_queue_len: 4,
            response_queue_len: 4,
            max_context_providers: 4,
            task_stack_size: 0,
            task_priority: 0,
            task_core: 0,
        }
    }

    fn user_request(id: u32, text: &str) -> RequestItem {
        RequestItem {
            request_id: id,
            user_text: text.into(),
            session_id: Some("s1".into()),
            source_channel: Some("cli".into()),
            source_chat_id: Some("c1".into()),
            ..Default::default()
        }
    }

    const TOOL_CALL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"t1","function":{"name":"files","arguments":"{}"}}]}}]}"#;
    const FINAL_BODY: &str =
        r#"{"choices":[{"message":{"role":"assistant","content":"all done"}}]}"#;

    #[test]
    fn single_turn_plain_text() {
        let core = Core::create(base_config(vec![FINAL_BODY.into()])).unwrap();
        assert_eq!(core.start(), ESP_OK);
        assert_eq!(core.submit(user_request(1, "hi"), 1000), ESP_OK);
        let resp = core.receive_for(1, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.text.as_deref(), Some("all done"));
        assert_eq!(core.stop(1000), ESP_OK);
    }

    #[test]
    fn tool_loop_then_final() {
        let core = Core::create(base_config(vec![TOOL_CALL_BODY.into(), FINAL_BODY.into()])).unwrap();
        assert_eq!(core.start(), ESP_OK);
        assert_eq!(core.submit(user_request(7, "do work"), 1000), ESP_OK);
        let resp = core.receive_for(7, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.text.as_deref(), Some("all done"));
        assert_eq!(core.stop(1000), ESP_OK);
    }

    #[test]
    fn iteration_limit_reached() {
        // Every chat returns a tool call, so the loop bails at max_tool_iterations.
        let mut cfg = base_config(vec![TOOL_CALL_BODY.into(), TOOL_CALL_BODY.into()]);
        cfg.max_tool_iterations = 2;
        let core = Core::create(cfg).unwrap();
        assert_eq!(core.start(), ESP_OK);
        assert_eq!(core.submit(user_request(9, "loop"), 1000), ESP_OK);
        let resp = core.receive_for(9, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.error_message.as_deref(), Some("cap tool iteration limit reached"));
        assert_eq!(core.stop(1000), ESP_OK);
    }

    #[test]
    fn provider_injects_system_prompt() {
        let core = Core::create(base_config(vec![FINAL_BODY.into()])).unwrap();
        assert_eq!(
            core.add_context_provider(Box::new(StaticProvider {
                name: "skills".into(),
                flags: 0,
                kind: ContextKind::SystemPrompt,
                content: "extra instructions".into(),
            })),
            ESP_OK
        );
        assert_eq!(core.start(), ESP_OK);
        assert_eq!(core.submit(user_request(3, "hi"), 1000), ESP_OK);
        let resp = core.receive_for(3, 2000).unwrap();
        assert_eq!(resp.text.as_deref(), Some("all done"));
        assert_eq!(core.stop(1000), ESP_OK);
    }

    #[test]
    fn user_interrupt_inserts_message() {
        // First chat blocks behaviorally by being a tool call; meanwhile we
        // submit an interrupt, which is drained before the next iteration.
        let core = Core::create(base_config(vec![TOOL_CALL_BODY.into(), FINAL_BODY.into()])).unwrap();
        assert_eq!(core.start(), ESP_OK);
        assert_eq!(core.submit(user_request(11, "first"), 1000), ESP_OK);
        // best-effort: also queue an interrupt for the same session
        let mut interrupt = user_request(12, "second");
        interrupt.flags = REQUEST_FLAG_USER_INTERRUPT;
        let _ = core.submit(interrupt, 100);
        let resp = core.receive_for(11, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(core.stop(1000), ESP_OK);
    }
}
