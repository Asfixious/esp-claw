//! The agent core object, port of `claw_core.c`.
//!
//! `Core` owns the mailbox ([`AgentMailbox`]), the LLM runtime, the injected
//! callbacks, and the context providers / completion observers. [`Core::start`]
//! spawns a `std::thread` running [`Core::run`], which delegates each request to
//! one-run [`crate::iteration_loop::IterationLoop`] steps orchestrated by
//! [`Core::run_one_turn`].

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use claw_interfaces::error::ESP_OK;
use claw_interfaces::event::EventPublisher;
use claw_interfaces::http::ClawHttp;

use crate::callbacks::{
    CompletionObserver, CompletionSummary, ContextProvider, GateOutcome, RequestGate, RequestStart,
    StageNote,
};
use claw_cap::CapabilityInvoker;
use crate::consts::{
    CompletionType, ResponseStatus, DEFAULT_REQUEST_Q, DEFAULT_RESPONSE_Q, DEFAULT_STACK_SIZE,
    DEFAULT_TOOL_ITERATIONS, MAX_COMPLETION_OBSERVERS, REQUEST_FLAG_SKIP_RESPONSE_QUEUE,
};
use crate::agent_mailbox::AgentMailbox;
use crate::errname::esp_err_to_name;
use crate::error::CoreError;
use crate::events::publish_out_message_if_requested;
use crate::iteration_loop::{
    AppendedMessages, ChatMessages, IterationLoop, IterationLoopError, IterationLoopPhase,
    IterationResult, IterationStep, PlainTextOutcome, SystemPrompt, ToolRun, ToolSet,
};
use crate::util::{append_tool_summary_line, obs_csv_append};
use claw_api::{ClawApi, ClawApiConfig};
use crate::request::RequestItem;
use crate::response::ResponseItem;

/// Result of bootstrapping a [`TurnState`] before the first one-run step.
enum TurnStart {
    Session(TurnState),
    GateRejected,
}

/// Cached LLM inputs for the current step (owned so [`IterationStep`] can borrow).
struct AssembledInput {
    system_prompt: String,
    messages: serde_json::Value,
    tools_json: Option<String>,
}

impl Default for AssembledInput {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            messages: serde_json::Value::Array(Vec::new()),
            tools_json: None,
        }
    }
}

/// Per-request turn state owned by [`Core`] (not [`IterationLoop`]).
struct TurnState {
    request: RequestItem,
    index: u32,
    message_tail: AppendedMessages,
    assembled: AssembledInput,
    inject_active_user: bool,
    tool_summary: String,
    obs_tool_calls_csv: String,
}

/// Rust-side construction inputs (the resolved `claw_core_config_t` plus the
/// injected dependencies). The C ABI layer builds this from the C config.
pub struct CoreConfig {
    pub instance_id: u32,
    pub system_prompt: String,
    pub runtime_config: ClawApiConfig,
    pub http: Arc<dyn ClawHttp>,
    pub capability_invoker: Option<Box<dyn CapabilityInvoker>>,
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

/// Return `value` when non-zero, otherwise `default` (the C `?:` fallbacks used
/// for the configurable queue/iteration/stack sizes).
fn nonzero_or(value: u32, default: u32) -> u32 {
    if value > 0 {
        value
    } else {
        default
    }
}

pub struct Core {
    pub mailbox: AgentMailbox,
    log_tag: String,
    pub(crate) system_prompt: String,
    pub(crate) llm: ClawApi,
    pub(crate) providers: Mutex<Vec<Box<dyn ContextProvider>>>,
    provider_capacity: usize,
    pub(crate) observers: Mutex<Vec<Box<dyn CompletionObserver>>>,
    pub(crate) capability_invoker: Option<Box<dyn CapabilityInvoker>>,
    pub(crate) request_gate: Option<Box<dyn RequestGate>>,
    pub(crate) on_request_start: Option<Box<dyn RequestStart>>,
    pub(crate) collect_stage_note: Option<Box<dyn StageNote>>,
    pub(crate) events: Option<Box<dyn EventPublisher>>,
    task: Mutex<Option<JoinHandle<()>>>,
    task_stack_size: u32,
    task_priority: u32,
    task_core: i32,
}

impl Core {
    /// `claw_core_create`.
    pub fn create(config: CoreConfig) -> Result<Arc<Core>, CoreError> {
        check_timezone();

        let llm = ClawApi::init(config.runtime_config, config.http).map_err(|e| {
            log::error!("LLM init failed: {e}");
            CoreError::Init(e)
        })?;

        let request_q = nonzero_or(config.request_queue_len, DEFAULT_REQUEST_Q) as usize;
        let response_q = nonzero_or(config.response_queue_len, DEFAULT_RESPONSE_Q) as usize;
        let max_iter = nonzero_or(config.max_tool_iterations, DEFAULT_TOOL_ITERATIONS);

        let log_tag = format!("claw_core_agent_{}", config.instance_id);
        let core = Core {
            mailbox: AgentMailbox::new(config.instance_id, request_q, response_q, max_iter),
            log_tag,
            system_prompt: config.system_prompt,
            llm,
            providers: Mutex::new(Vec::new()),
            provider_capacity: config.max_context_providers,
            observers: Mutex::new(Vec::new()),
            capability_invoker: config.capability_invoker,
            request_gate: config.request_gate,
            on_request_start: config.on_request_start,
            collect_stage_note: config.collect_stage_note,
            events: config.events,
            task: Mutex::new(None),
            task_stack_size: nonzero_or(config.task_stack_size, DEFAULT_STACK_SIZE),
            task_priority: config.task_priority,
            task_core: config.task_core,
        };
        log::info!("{} Initialized", core.log_tag);
        Ok(Arc::new(core))
    }

    /// `claw_core_add_context_provider`.
    pub fn add_context_provider(
        &self,
        provider: Box<dyn ContextProvider>,
    ) -> Result<(), CoreError> {
        if !self.mailbox.initialized.load(Ordering::Acquire) || self.mailbox.started.load(Ordering::Acquire)
        {
            return Err(CoreError::InvalidState);
        }
        let mut providers = self.providers.lock().unwrap();
        if providers.len() >= self.provider_capacity {
            return Err(CoreError::NoMem);
        }
        providers.push(provider);
        Ok(())
    }

    /// `claw_core_add_completion_observer`.
    pub fn add_completion_observer(
        &self,
        observer: Box<dyn CompletionObserver>,
    ) -> Result<(), CoreError> {
        if !self.mailbox.initialized.load(Ordering::Acquire) {
            return Err(CoreError::InvalidState);
        }
        let mut observers = self.observers.lock().unwrap();
        if observers.len() >= MAX_COMPLETION_OBSERVERS {
            return Err(CoreError::NoMem);
        }
        observers.push(observer);
        Ok(())
    }

    /// `claw_core_start`.
    pub fn start(self: &Arc<Self>) -> Result<(), CoreError> {
        if !self.mailbox.initialized.load(Ordering::Acquire) {
            return Err(CoreError::InvalidState);
        }
        if self.mailbox.started.load(Ordering::Acquire) {
            return Ok(());
        }
        self.mailbox.started.store(true, Ordering::Release);
        self.mailbox.stop_requested.store(false, Ordering::Release);

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
                Ok(())
            }
            Err(_) => {
                self.mailbox.started.store(false, Ordering::Release);
                Err(CoreError::Fail)
            }
        }
    }

    /// `claw_core_stop`.
    pub fn stop(&self, _timeout_ms: u32) -> Result<(), CoreError> {
        if !self.mailbox.initialized.load(Ordering::Acquire) {
            return Err(CoreError::InvalidState);
        }
        if !self.mailbox.started.load(Ordering::Acquire) {
            return Ok(());
        }
        self.mailbox.stop_requested.store(true, Ordering::Release);
        let _ = self.mailbox.cancel_request(0);
        // Wake the loop if it is blocked on the request queue.
        let _ = self
            .mailbox
            .request_queue
            .send_timeout(RequestItem::default(), Some(Duration::from_millis(0)));

        let handle = self.task.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        Ok(())
    }

    /// `claw_core_submit`.
    pub fn submit(&self, request: RequestItem, timeout_ms: u32) -> Result<(), CoreError> {
        self.mailbox.ingress_submit(request, timeout_ms)
    }

    /// `claw_core_receive_for` / `claw_core_receive`.
    pub fn receive_for(
        &self,
        request_id: u32,
        timeout_ms: u32,
    ) -> Result<ResponseItem, CoreError> {
        self.mailbox
            .response_receive_for(request_id, timeout_ms)
            .map_err(CoreError::Esp)
    }

    /// `claw_core_cancel_request`.
    pub fn cancel_request(&self, request_id: u32) -> Result<(), CoreError> {
        self.mailbox.cancel_request(request_id)
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

    /// Whether the loop thread is running.
    pub fn is_started(&self) -> bool {
        self.mailbox.started.load(Ordering::Acquire)
    }

    // --- worker thread ----------------------------------------------------

    /// `claw_core_agent_loop_task`.
    fn run(&self) {
        while !self.mailbox.stop_requested.load(Ordering::Acquire) {
            let Some(request) = self.mailbox.request_queue.recv_timeout(None) else {
                continue;
            };
            if self.mailbox.stop_requested.load(Ordering::Acquire) {
                break;
            }
            self.run_one_turn(request);
        }
        log::info!("{} Stopped worker task", self.log_tag);
        self.mailbox.started.store(false, Ordering::Release);
    }

    /// Drive one queued request: turn setup, repeated one-run
    /// [`IterationLoop`] steps, then response delivery.
    fn run_one_turn(&self, request: RequestItem) {
        self.mailbox
            .begin_turn(request.request_id, request.session_id_str());

        let mut response = ResponseItem {
            request_id: request.request_id,
            status: ResponseStatus::Error,
            target_channel: request.target_channel.clone(),
            target_chat_id: request.target_chat_id.clone(),
            ..Default::default()
        };

        let mut turn = match self.begin_turn_state(&request, &mut response) {
            Ok(TurnStart::Session(turn)) => turn,
            Ok(TurnStart::GateRejected) => return,
            Err(err) => {
                response.error_message = Some(err.response_message());
                self.finish_request(&request, &mut response, Some(&err));
                return;
            }
        };

        let mut turn_error: Option<CoreError> = None;

        loop {
            if let Err(err) = self.assemble_iteration_input(&mut turn) {
                turn_error = Some(err);
                break;
            }
            let step = IterationStep {
                request: &turn.request,
                system_prompt: SystemPrompt(&turn.assembled.system_prompt),
                messages: ChatMessages(&turn.assembled.messages),
                tools: ToolSet::new(turn.assembled.tools_json.as_deref()),
            };
            let iteration = IterationLoop {
                llm: &self.llm,
                capability_invoker: self.capability_invoker.as_deref(),
                control: &self.mailbox,
            };
            match iteration.run(step) {
                Ok(IterationResult::Interrupted(outcome)) => {
                    turn.message_tail.extend(&outcome.appended);
                    continue;
                }
                Ok(IterationResult::Tools(outcome)) => {
                    turn.message_tail.extend(&outcome.appended);
                    record_tool_runs(
                        &mut turn.tool_summary,
                        &mut turn.obs_tool_calls_csv,
                        &outcome.runs,
                    );
                    self.publish_stage_tool_calls(&turn.request, turn.index, &outcome.runs);
                    turn.index += 1;
                    if turn.index >= self.mailbox.max_tool_iterations {
                        turn_error = Some(CoreError::IterationLimit);
                        break;
                    }
                }
                Ok(IterationResult::PlainText(PlainTextOutcome { text, .. })) => {
                    self.publish_stage_note_for_round(&turn.request, turn.index);
                    response.status = ResponseStatus::Ok;
                    response.completion_type = CompletionType::Done;
                    response.error_message = None;
                    response.text = Some(text);
                    break;
                }
                Err(err) => {
                    turn_error = Some(err.into());
                    break;
                }
            }
        }

        if let Some(err) = turn_error.as_ref() {
            response.error_message = Some(err.response_message());
        } else if response.text.is_some() {
            self.run_completion_observers(&request, &response, &turn.obs_tool_calls_csv);
        } else if response.error_message.is_none() {
            response.error_message = Some(esp_err_to_name(ESP_OK).to_string());
        }

        self.finish_request(&request, &mut response, turn_error.as_ref());
    }

    /// Assemble the OpenAI-style inputs for one [`IterationLoop`] step.
    fn assemble_iteration_input(&self, turn: &mut TurnState) -> Result<(), CoreError> {
        turn.assembled.system_prompt = self.system_prompt.clone();
        let mut messages = serde_json::Value::Array(Vec::new());
        if turn.inject_active_user {
            messages
                .as_array_mut()
                .ok_or(CoreError::InvalidArg)?
                .push(serde_json::json!({
                    "role": "user",
                    "content": turn.request.user_text,
                }));
            turn.inject_active_user = false;
        }
        if let Some(tail) = turn.message_tail.0.as_array() {
            if !tail.is_empty() {
                messages.as_array_mut().unwrap().extend(tail.iter().cloned());
            }
        }
        turn.assembled.messages = messages;
        turn.assembled.tools_json = None;
        Ok(())
    }

    /// Gate, request-start hooks, and turn-state bootstrap.
    fn begin_turn_state(
        &self,
        request: &RequestItem,
        response: &mut ResponseItem,
    ) -> Result<TurnStart, CoreError> {
        if let Some(gate) = self.request_gate.as_deref() {
            match gate.gate(request) {
                GateOutcome::Allow => {}
                GateOutcome::Reject(message) => {
                    response.status = ResponseStatus::Ok;
                    response.text = Some(message);
                    self.finish_request(request, response, None);
                    return Ok(TurnStart::GateRejected);
                }
                GateOutcome::Error(err) => return Err(CoreError::Esp(err)),
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

        Ok(TurnStart::Session(TurnState {
            request: request.clone(),
            index: 0,
            message_tail: AppendedMessages::empty(),
            assembled: AssembledInput::default(),
            inject_active_user: true,
            tool_summary: String::new(),
            obs_tool_calls_csv: String::new(),
        }))
    }

    fn run_completion_observers(
        &self,
        request: &RequestItem,
        response: &ResponseItem,
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
            context_providers_csv: "",
            tool_calls_csv: obs_tool_calls_csv,
        };
        for observer in observers.iter() {
            observer.on_complete(&summary);
        }
    }

    fn finish_request(
        &self,
        request: &RequestItem,
        response: &mut ResponseItem,
        error: Option<&CoreError>,
    ) {
        self.mailbox.set_phase(IterationLoopPhase::Finalizing);
        let was_cancelled = self.mailbox.end_turn();
        if was_cancelled && error.is_some() && response.error_message.is_some() {
            response.error_message = Some("request cancelled".to_string());
        }

        if let Some(error) = error {
            log::error!(
                "request={} failed: {}",
                request.request_id,
                response
                    .error_message
                    .as_deref()
                    .unwrap_or_else(|| error.esp_name())
            );
        }

        if let Some(events) = self.events.as_deref() {
            publish_out_message_if_requested(events, request, response);
        }

        if request.flags & REQUEST_FLAG_SKIP_RESPONSE_QUEUE != 0 {
            return;
        }
        self.mailbox.response_push(response.clone());
    }

    fn publish_stage_note_for_round(&self, request: &RequestItem, round_index: u32) {
        #[cfg(feature = "stage_verbose")]
        use crate::events::publish_stage_text;

        let Some(collector) = self.collect_stage_note.as_deref() else {
            return;
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
            let _ = (note, round_index);
        }
    }

    fn publish_stage_tool_calls(
        &self,
        request: &RequestItem,
        round_index: u32,
        runs: &[ToolRun],
    ) {
        #[cfg(feature = "stage_verbose")]
        use crate::events::publish_stage_text;

        #[cfg(feature = "stage_verbose")]
        {
            if runs.is_empty() {
                return;
            }
            let Some(events) = self.events.as_deref() else {
                return;
            };
            let names: Vec<&str> = runs.iter().map(|r| r.name.as_str()).collect();
            let buf = format!(
                "\u{1F99E} [Round {}] Snap: {}",
                round_index + 1,
                names.join(", ")
            );
            let _ = publish_stage_text(events, request, &buf);
        }
        #[cfg(not(feature = "stage_verbose"))]
        {
            let _ = (request, round_index, runs);
        }
    }
}

fn record_tool_runs(
    tool_summary: &mut String,
    obs_tool_calls_csv: &mut String,
    runs: &[ToolRun],
) {
    for run in runs {
        if !run.name.is_empty()
            && append_tool_summary_line(tool_summary, &run.name, run.ok).is_err()
        {
            log::warn!("tool summary truncated");
        }
        obs_csv_append(obs_tool_calls_csv, &run.name, false);
    }
}

impl From<IterationLoopError> for CoreError {
    fn from(error: IterationLoopError) -> Self {
        use claw_cap::CapabilityError;
        match error {
            IterationLoopError::MessagesNotArray => CoreError::InvalidArg,
            IterationLoopError::MissingCapabilityInvoker => CoreError::InvalidState,
            IterationLoopError::MissingAssistantMessage
            | IterationLoopError::MalformedAssistantMessage => CoreError::Fail,
            IterationLoopError::Chat(chat) => CoreError::Chat(chat),
            IterationLoopError::Capability(capability) => match capability {
                CapabilityError::NoMem => CoreError::NoMem,
                CapabilityError::InvalidArg => CoreError::InvalidArg,
                CapabilityError::NotFound => CoreError::Fail,
                CapabilityError::NotAvailable | CapabilityError::NotVisible => {
                    CoreError::InvalidState
                }
                CapabilityError::Failed => CoreError::Fail,
            },
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
    use crate::consts::{ResponseStatus, REQUEST_FLAG_USER_INTERRUPT};
    use claw_api::ClawApiConfig;
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

    struct EchoCapability;
    impl CapabilityInvoker for EchoCapability {
        fn invoke(
            &self,
            capability_name: &str,
            _input_json: &str,
            _context: &claw_cap::CapabilityContext,
        ) -> Result<claw_cap::CapabilityInvokeResult, claw_cap::CapabilityError> {
            Ok(claw_cap::CapabilityInvokeResult {
                output: format!("{capability_name} done"),
                ok: true,
            })
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
            capability_invoker: Some(Box::new(EchoCapability)),
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
        core.start().unwrap();
        core.submit(user_request(1, "hi"), 1000).unwrap();
        let resp = core.receive_for(1, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.text.as_deref(), Some("all done"));
        core.stop(1000).unwrap();
    }

    #[test]
    fn tool_loop_then_final() {
        let core = Core::create(base_config(vec![TOOL_CALL_BODY.into(), FINAL_BODY.into()])).unwrap();
        core.start().unwrap();
        core.submit(user_request(7, "do work"), 1000).unwrap();
        let resp = core.receive_for(7, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.text.as_deref(), Some("all done"));
        core.stop(1000).unwrap();
    }

    #[test]
    fn iteration_limit_reached() {
        // Every chat returns a tool call, so the loop bails at max_tool_iterations.
        let mut cfg = base_config(vec![TOOL_CALL_BODY.into(), TOOL_CALL_BODY.into()]);
        cfg.max_tool_iterations = 2;
        let core = Core::create(cfg).unwrap();
        core.start().unwrap();
        core.submit(user_request(9, "loop"), 1000).unwrap();
        let resp = core.receive_for(9, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.error_message.as_deref(), Some("cap tool iteration limit reached"));
        core.stop(1000).unwrap();
    }

    #[test]
    fn user_interrupt_inserts_message() {
        // First chat blocks behaviorally by being a tool call; meanwhile we
        // submit an interrupt, which is drained before the next iteration.
        let core = Core::create(base_config(vec![TOOL_CALL_BODY.into(), FINAL_BODY.into()])).unwrap();
        core.start().unwrap();
        core.submit(user_request(11, "first"), 1000).unwrap();
        // best-effort: also queue an interrupt for the same session
        let mut interrupt = user_request(12, "second");
        interrupt.flags = REQUEST_FLAG_USER_INTERRUPT;
        let _ = core.submit(interrupt, 100);
        let resp = core.receive_for(11, 2000).unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        core.stop(1000).unwrap();
    }
}
