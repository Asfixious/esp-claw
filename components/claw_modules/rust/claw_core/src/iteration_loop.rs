//! One LLM/tool round-trip per [`IterationLoop`].
//!
//! [`IterationLoop`] is stateless and models exactly **one** iteration step:
//! OpenAI-style chat inputs in, LLM call, optional tool dispatch out. Turn
//! bookkeeping, context assembly, observers, and stage publishing live above
//! this layer.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;

use claw_api::{ChatError, ChatRequest, ClawApi, LlmResponse};
use claw_cap::{CapabilityError, CapabilityInvokeResult, CapabilityInvoker};
use crate::request::RequestItem;

/// Errors from one [`IterationLoop::run`] step.
#[derive(Debug, thiserror::Error)]
pub enum IterationLoopError {
    #[error("messages must be a JSON array")]
    MessagesNotArray,
    #[error("tool calls require a configured capability invoker")]
    MissingCapabilityInvoker,
    #[error("LLM tool-call response missing raw assistant message JSON")]
    MissingAssistantMessage,
    #[error("LLM raw assistant message JSON is not valid JSON")]
    MalformedAssistantMessage,
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error(transparent)]
    Capability(#[from] CapabilityError),
}

/// Internal iteration phase for interrupt gating and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IterationLoopPhase {
    Idle,
    BeforeLlmHttp,
    InLlmHttp,
    AfterLlmBeforeTool,
    RunningTool,
    Finalizing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptDrainPoint {
    BeforeLlmHttp,
    InLlmHttpAbort,
    AfterLlmBeforeTool,
}

pub(crate) fn phase_is_insertable(phase: IterationLoopPhase) -> bool {
    !matches!(
        phase,
        IterationLoopPhase::Idle | IterationLoopPhase::Finalizing
    )
}

/// Borrowed OpenAI-style system prompt for one LLM call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemPrompt<'a>(pub &'a str);

impl AsRef<str> for SystemPrompt<'_> {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// Borrowed OpenAI-style `messages` array for one LLM call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatMessages<'a>(pub &'a Value);

/// Owned message batch appended by one step (tool round or user interrupt).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AppendedMessages(pub Value);

impl AppendedMessages {
    pub fn empty() -> Self {
        Self(Value::Array(Vec::new()))
    }

    pub fn extend(&mut self, other: &AppendedMessages) {
        if let (Some(dest), Some(src)) = (self.0.as_array_mut(), other.0.as_array()) {
            dest.extend(src.iter().cloned());
        }
    }
}

/// Borrowed model-facing tool schemas for one iteration step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ToolSet<'a> {
    tools_json: Option<&'a str>,
}

impl<'a> ToolSet<'a> {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn new(tools_json: Option<&'a str>) -> Self {
        Self { tools_json }
    }

    pub fn as_json(self) -> Option<&'a str> {
        self.tools_json
    }
}

/// Inputs for exactly one [`IterationLoop::run`]: borrowed OpenAI chat request
/// fields plus [`RequestItem`] for tool dispatch and interrupt handling.
pub struct IterationStep<'a> {
    pub request: &'a RequestItem,
    pub system_prompt: SystemPrompt<'a>,
    pub messages: ChatMessages<'a>,
    pub tools: ToolSet<'a>,
}

/// Outcome of exactly one [`IterationLoop::run`].
#[derive(Clone, Debug, PartialEq)]
pub enum IterationResult {
    Tools(ToolsOutcome),
    PlainText(PlainTextOutcome),
    Interrupted(InterruptedOutcome),
}

/// One executed tool call (for turn-level observers above this layer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRun {
    pub name: String,
    pub ok: bool,
}

/// The model issued tool calls.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolsOutcome {
    /// Assistant + tool messages produced this step (turn layer merges).
    pub appended: AppendedMessages,
    pub runs: Vec<ToolRun>,
}

/// The model returned a final plain-text answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainTextOutcome {
    pub text: String,
    pub raw_message_json: Option<String>,
}

/// User interrupts were drained; merge `appended` then re-assemble.
#[derive(Clone, Debug, PartialEq)]
pub struct InterruptedOutcome {
    pub appended: AppendedMessages,
}

/// Per-request interrupt, cancel, and phase surface used during one iteration.
pub trait RequestControl: Send + Sync {
    fn set_phase(&self, phase: IterationLoopPhase);
    fn abort_flag(&self) -> &Arc<AtomicBool>;
    fn take_user_interrupt_http_abort(&self, request_id: u32) -> bool;
    fn dequeue_inserted_user_inputs(&self, session_id: &str, max: usize) -> Vec<String>;
    fn clear_user_interrupt_abort(&self, request_id: u32);
}

/// One-step executor; holds only what a single iteration needs (no [`crate::core::Core`]).
pub struct IterationLoop<'a> {
    pub llm: &'a ClawApi,
    pub capability_invoker: Option<&'a dyn CapabilityInvoker>,
    pub control: &'a dyn RequestControl,
}

impl IterationLoop<'_> {
    /// Execute exactly one iteration: LLM chat → optional tool execution.
    pub fn run(&self, step: IterationStep<'_>) -> Result<IterationResult, IterationLoopError> {
        run_one_iteration(self, step)
    }
}

fn run_one_iteration(
    loop_: &IterationLoop<'_>,
    step: IterationStep<'_>,
) -> Result<IterationResult, IterationLoopError> {
    let request = step.request;
    let mut appended = AppendedMessages::empty();

    loop_.control.set_phase(IterationLoopPhase::BeforeLlmHttp);
    if drain_user_interrupts(
        loop_.control,
        request,
        InterruptDrainPoint::BeforeLlmHttp,
        &mut appended,
    )? {
        return Ok(IterationResult::Interrupted(InterruptedOutcome { appended }));
    }

    loop_.control.set_phase(IterationLoopPhase::InLlmHttp);
    let chat_request = ChatRequest {
        system_prompt: step.system_prompt.as_ref(),
        messages: step.messages.0,
        tools_json: step.tools.as_json(),
    };
    let llm_response = match loop_.llm.chat(&chat_request, loop_.control.abort_flag()) {
        Ok(resp) => resp,
        Err(llm_err) => {
            if loop_
                .control
                .take_user_interrupt_http_abort(request.request_id)
                && drain_user_interrupts(
                    loop_.control,
                    request,
                    InterruptDrainPoint::InLlmHttpAbort,
                    &mut appended,
                )?
            {
                return Ok(IterationResult::Interrupted(InterruptedOutcome { appended }));
            }
            return Err(IterationLoopError::Chat(llm_err));
        }
    };

    if llm_response.tool_calls.is_empty() {
        loop_.control.set_phase(IterationLoopPhase::Finalizing);
        let text = llm_response.text.clone().unwrap_or_default();
        log::info!(
            "completion request={} status=done raw={}",
            request.request_id,
            crate::util::truncate_for_log(&text)
        );
        return Ok(IterationResult::PlainText(PlainTextOutcome {
            text,
            raw_message_json: llm_response.raw_message_json.clone(),
        }));
    }

    loop_
        .control
        .set_phase(IterationLoopPhase::AfterLlmBeforeTool);
    if drain_user_interrupts(
        loop_.control,
        request,
        InterruptDrainPoint::AfterLlmBeforeTool,
        &mut appended,
    )? {
        return Ok(IterationResult::Interrupted(InterruptedOutcome { appended }));
    }

    loop_.control.set_phase(IterationLoopPhase::RunningTool);
    log_tool_call_names(request.request_id, &llm_response);

    append_assistant_tool_calls(&mut appended.0, &llm_response)?;

    let Some(invoker) = loop_.capability_invoker else {
        return Err(IterationLoopError::MissingCapabilityInvoker);
    };
    let context = request.capability_context();
    let runs = append_tool_results_messages(
        invoker,
        &context,
        &mut appended.0,
        &llm_response,
        request,
    )?;

    Ok(IterationResult::Tools(ToolsOutcome { appended, runs }))
}

fn drain_user_interrupts(
    control: &dyn RequestControl,
    request: &RequestItem,
    timing: InterruptDrainPoint,
    appended: &mut AppendedMessages,
) -> Result<bool, IterationLoopError> {
    use crate::consts::INSERT_QUEUE_LEN;

    let texts = control.dequeue_inserted_user_inputs(request.session_id_str(), INSERT_QUEUE_LEN);
    if texts.is_empty() {
        return Ok(false);
    }
    control.clear_user_interrupt_abort(request.request_id);

    log::info!(
        "user_interrupt_triggered request={} timing={:?} count={}",
        request.request_id,
        timing,
        texts.len()
    );

    for text in &texts {
        append_user_message(&mut appended.0, text)?;
    }
    Ok(true)
}

fn append_user_message(messages: &mut Value, text: &str) -> Result<(), IterationLoopError> {
    let Some(arr) = messages.as_array_mut() else {
        return Err(IterationLoopError::MessagesNotArray);
    };
    arr.push(serde_json::json!({ "role": "user", "content": text }));
    Ok(())
}

fn append_assistant_tool_calls(
    messages: &mut Value,
    response: &LlmResponse,
) -> Result<(), IterationLoopError> {
    let Some(raw) = response.raw_message_json.as_deref().filter(|s| !s.is_empty()) else {
        return Err(IterationLoopError::MissingAssistantMessage);
    };
    let Ok(assistant) = serde_json::from_str::<Value>(raw) else {
        return Err(IterationLoopError::MalformedAssistantMessage);
    };
    match messages.as_array_mut() {
        Some(a) => {
            a.push(assistant);
            Ok(())
        }
        None => Err(IterationLoopError::MessagesNotArray),
    }
}

fn append_tool_results_messages(
    invoker: &dyn CapabilityInvoker,
    context: &claw_cap::CapabilityContext,
    runtime_messages: &mut Value,
    response: &LlmResponse,
    request: &RequestItem,
) -> Result<Vec<ToolRun>, IterationLoopError> {
    let Some(runtime_arr) = runtime_messages.as_array_mut() else {
        return Err(IterationLoopError::MessagesNotArray);
    };
    let mut runs: Vec<ToolRun> = Vec::with_capacity(response.tool_calls.len());

    for tc in &response.tool_calls {
        log::info!(
            "tool_call request={} name={} args={}",
            request.request_id,
            if tc.name.is_empty() { "(null)" } else { &tc.name },
            tc.arguments_json
        );

        let CapabilityInvokeResult { output, ok } =
            invoker.invoke(&tc.name, &tc.arguments_json, context)?;

        log::info!(
            "tool_result request={} name={} ok={} output={}",
            request.request_id,
            if tc.name.is_empty() { "(null)" } else { &tc.name },
            ok,
            output
        );

        let tool_message = serde_json::json!({
            "role": "tool",
            "tool_call_id": tc.id,
            "content": output,
            "is_error": !ok,
        });

        runtime_arr.push(tool_message);
        runs.push(ToolRun {
            name: if tc.name.is_empty() {
                "(null)".to_string()
            } else {
                tc.name.clone()
            },
            ok,
        });
    }

    Ok(runs)
}

fn log_tool_call_names(request_id: u32, response: &LlmResponse) {
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
