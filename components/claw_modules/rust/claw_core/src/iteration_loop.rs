//! One LLM/tool round-trip per [`IterationLoop`].
//!
//! Layer 3 only: pass chat + [`IterationTools`], LLM picks tool calls, we invoke
//! them, return which tools ran. No session, channel, or routing concepts here.
//!
//! On preemption this layer only detects the signal and ends the iteration.
//! It does not read, format, or return interrupt message content — upper layers
//! own pending input and context rebuild.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

use claw_api::{ChatError, ChatRequest, ClawApi, ClawApiError, LlmResponse};
use crate::tools::{ToolError, ToolInvocation, ToolOutput};

use claw_utils::TruncatedText;

crate::define_prefixed_id!(IterationId, "iteration-", "iteration");

/// Errors from one [`IterationLoop::run`] step.
#[derive(Clone, Debug, thiserror::Error)]
pub enum IterationLoopError {
    #[error("messages must be a JSON array")]
    MessagesNotArray,
    #[error("LLM tool-call response missing raw assistant message JSON")]
    MissingAssistantMessage,
    #[error("LLM raw assistant message JSON is not valid JSON")]
    MalformedAssistantMessage,
    #[error("LLM issued tool calls but no tools port was supplied")]
    MissingTools,
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error(transparent)]
    Tool(#[from] ToolError),
}

/// Checkpoint where preemption was detected. The iteration is terminal at this point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IterationCheckpoint {
    BeforeLlmHttp,
    InLlmHttpAbort,
    AfterLlmBeforeTool,
    BeforeTool,
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

/// Owned message batch appended by one completed step (assistant/tool round).
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

    fn as_produced_snapshot(&self) -> Option<AppendedMessages> {
        match self.0.as_array() {
            Some(items) if !items.is_empty() => Some(self.clone()),
            _ => None,
        }
    }
}

/// Tools available for one iteration: schemas for the LLM plus invoke-by-name.
pub trait IterationTools: Send + Sync {
    /// OpenAI-style tools JSON sent to the LLM, if any.
    fn schemas_json(&self) -> Option<&str>;

    /// Execute one model `tool_call`.
    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError>;
}

/// Inputs for exactly one [`IterationLoop::run`]: chat fields + optional tools.
pub struct IterationStep<'a> {
    pub iteration_id: IterationId,
    pub system_prompt: SystemPrompt<'a>,
    pub messages: ChatMessages<'a>,
    pub tools: Option<&'a dyn IterationTools>,
}

/// Terminal outcome of exactly one [`IterationLoop::run`] (completed or preempted).
#[derive(Clone, Debug)]
pub enum IterationOutcome {
    Completed(CompletedOutcome),
    Preempted(PreemptedOutcome),
}

/// One [`IterationLoop::run`] step: [`IterationOutcome`] or [`IterationLoopError`].
pub type IterationResult = Result<IterationOutcome, IterationLoopError>;

/// Successful iteration: plain-text answer or executed tool round.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletedOutcome {
    pub iteration_id: IterationId,
    pub kind: CompletedKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompletedKind {
    PlainText(PlainTextOutcome),
    Tools(ToolsOutcome),
}

/// One executed tool call (for iteration-level observers above this layer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRun {
    pub name: String,
    pub ok: bool,
}

/// The model issued tool calls and they were executed.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolsOutcome {
    /// Assistant + tool messages produced this step (iteration layer merges).
    pub appended: AppendedMessages,
    pub runs: Vec<ToolRun>,
}

/// The model returned a final plain-text answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainTextOutcome {
    pub text: String,
    pub raw_message_json: Option<String>,
}

/// Iteration ended at a preempt checkpoint.
///
/// `produced` carries assistant/tool messages already materialized this step
/// (not interrupt message content). Upper layers decide whether to merge them.
#[derive(Clone, Debug, PartialEq)]
pub struct PreemptedOutcome {
    pub iteration_id: IterationId,
    pub checkpoint: IterationCheckpoint,
    pub produced: Option<AppendedMessages>,
}

/// User interrupt surface for one in-flight iteration. No message payloads here.
///
/// Contract with [`claw_interfaces::http::ClawHttp`]:
/// - Upper layer sets `interrupt_flag` to request cooperative abort.
/// - HTTP polls the flag and returns [`claw_interfaces::http::HttpError::Aborted`]
///   without clearing it (`claw_sys` / ESP HTTP keeps the flag intact).
/// - [`IterationLoop`] consumes the flag via `swap(false)` when ending preempted.
pub trait InterruptionControl: Send + Sync {
    /// Polled at checkpoints (consume) and passed to in-flight LLM HTTP (cooperative abort).
    fn interrupt_flag(&self) -> &Arc<AtomicBool>;
}

/// One-step executor: LLM + preempt control only. Tools live on [`IterationStep`].
pub struct IterationLoop<'a> {
    pub llm: &'a ClawApi,
    pub interruption: &'a dyn InterruptionControl,
}

impl IterationLoop<'_> {
    /// Execute exactly one iteration: LLM chat → optional tool execution.
    pub fn run(&self, step: IterationStep<'_>) -> IterationResult {
        run_one_iteration(self, step)
    }
}

fn run_one_iteration(loop_: &IterationLoop<'_>, step: IterationStep<'_>) -> IterationResult {
    let iteration_id = step.iteration_id;
    let mut appended = AppendedMessages::empty();

    if let Some(outcome) = check_preempt_at_checkpoint(
        loop_.interruption,
        iteration_id,
        IterationCheckpoint::BeforeLlmHttp,
        None,
    ) {
        return Ok(IterationOutcome::Preempted(outcome));
    }

    let chat_request = ChatRequest {
        system_prompt: step.system_prompt.as_ref(),
        messages: step.messages.0,
        tools_json: step.tools.and_then(|t| t.schemas_json()),
        retry: None,
    };
    let llm_response = match loop_
        .llm
        .chat(&chat_request, loop_.interruption.interrupt_flag())
    {
        Ok(resp) => resp,
        Err(llm_err) => {
            if llm_http_preempted(loop_.interruption, &llm_err) {
                return Ok(IterationOutcome::Preempted(PreemptedOutcome {
                    iteration_id,
                    checkpoint: IterationCheckpoint::InLlmHttpAbort,
                    produced: None,
                }));
            }
            return Err(IterationLoopError::Chat(llm_err));
        }
    };

    if llm_response.tool_calls.is_empty() {
        if let Some(outcome) = check_preempt_at_checkpoint(
            loop_.interruption,
            iteration_id,
            IterationCheckpoint::AfterLlmBeforeTool,
            None,
        ) {
            return Ok(IterationOutcome::Preempted(outcome));
        }

        let text = llm_response.text.clone().unwrap_or_default();
        log::info!(
            "completion iteration={} status=done raw={}",
            iteration_id,
            TruncatedText::new(&text)
        );
        return Ok(IterationOutcome::Completed(CompletedOutcome {
            iteration_id,
            kind: CompletedKind::PlainText(PlainTextOutcome {
                text,
                raw_message_json: llm_response.raw_message_json.clone(),
            }),
        }));
    }

    if let Some(outcome) = check_preempt_at_checkpoint(
        loop_.interruption,
        iteration_id,
        IterationCheckpoint::AfterLlmBeforeTool,
        None,
    ) {
        return Ok(IterationOutcome::Preempted(outcome));
    }

    log_tool_call_names(iteration_id, &llm_response);

    let Some(tools) = step.tools else {
        return Err(IterationLoopError::MissingTools);
    };

    if let Err(err) = append_assistant_tool_calls(&mut appended.0, &llm_response) {
        return Err(err);
    }

    match run_tool_calls(
        loop_.interruption,
        tools,
        &mut appended,
        &llm_response,
        iteration_id,
    ) {
        ToolRoundResult::Completed { runs } => Ok(IterationOutcome::Completed(CompletedOutcome {
            iteration_id,
            kind: CompletedKind::Tools(ToolsOutcome { appended, runs }),
        })),
        ToolRoundResult::Preempted(outcome) => Ok(IterationOutcome::Preempted(outcome)),
        ToolRoundResult::Failed(err) => Err(err),
    }
}

enum ToolRoundResult {
    Completed { runs: Vec<ToolRun> },
    Preempted(PreemptedOutcome),
    Failed(IterationLoopError),
}

fn take_interrupt(interruption: &dyn InterruptionControl) -> bool {
    interruption
        .interrupt_flag()
        .swap(false, Ordering::AcqRel)
}

/// True when an in-flight LLM HTTP ended because of user interrupt.
fn llm_http_preempted(interruption: &dyn InterruptionControl, err: &ChatError) -> bool {
    if take_interrupt(interruption) {
        return true;
    }
    matches!(
        err,
        ChatError::Api(ClawApiError::Transport(msg)) if msg.contains("aborted")
    )
}

fn check_preempt_at_checkpoint(
    interruption: &dyn InterruptionControl,
    iteration_id: IterationId,
    checkpoint: IterationCheckpoint,
    produced: Option<AppendedMessages>,
) -> Option<PreemptedOutcome> {
    if !take_interrupt(interruption) {
        return None;
    }

    log::info!(
        "iteration_preempted iteration={} checkpoint={:?}",
        iteration_id,
        checkpoint
    );

    Some(PreemptedOutcome {
        iteration_id,
        checkpoint,
        produced,
    })
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

fn run_tool_calls(
    interruption: &dyn InterruptionControl,
    tools: &dyn IterationTools,
    appended: &mut AppendedMessages,
    response: &LlmResponse,
    iteration_id: IterationId,
) -> ToolRoundResult {
    if appended.0.as_array().is_none() {
        return ToolRoundResult::Failed(IterationLoopError::MessagesNotArray);
    }

    let mut runs: Vec<ToolRun> = Vec::with_capacity(response.tool_calls.len());

    for tc in &response.tool_calls {
        if let Some(outcome) = check_preempt_at_checkpoint(
            interruption,
            iteration_id,
            IterationCheckpoint::BeforeTool,
            appended.as_produced_snapshot(),
        ) {
            return ToolRoundResult::Preempted(outcome);
        }

        log::info!(
            "tool_call iteration={} name={} args={}",
            iteration_id,
            if tc.name.is_empty() { "(null)" } else { &tc.name },
            tc.arguments_json
        );

        let ToolOutput { output, ok } = match tools.invoke(&ToolInvocation {
            id: Some(&tc.id),
            name: &tc.name,
            arguments_json: &tc.arguments_json,
        }) {
            Ok(output) => output,
            Err(err) => return ToolRoundResult::Failed(IterationLoopError::Tool(err)),
        };

        log::info!(
            "tool_result iteration={} name={} ok={} output={}",
            iteration_id,
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

        let Some(runtime_arr) = appended.0.as_array_mut() else {
            return ToolRoundResult::Failed(IterationLoopError::MessagesNotArray);
        };
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

    ToolRoundResult::Completed { runs }
}

fn log_tool_call_names(iteration_id: IterationId, response: &LlmResponse) {
    if response.tool_calls.is_empty() {
        return;
    }
    let names: Vec<&str> = response
        .tool_calls
        .iter()
        .map(|tc| if tc.name.is_empty() { "(null)" } else { tc.name.as_str() })
        .collect();
    log::debug!(
        "llm_tool_calls iteration={} count={} names={}",
        iteration_id,
        response.tool_calls.len(),
        names.join(",")
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;
    use claw_api::ToolCall;
    use serde_json::json;

    struct FlagControl {
        interrupt: Arc<AtomicBool>,
    }

    impl FlagControl {
        fn new() -> Self {
            Self {
                interrupt: Arc::new(AtomicBool::new(false)),
            }
        }

        fn signal_interrupt(&self) {
            self.interrupt.store(true, Ordering::Release);
        }
    }

    impl InterruptionControl for FlagControl {
        fn interrupt_flag(&self) -> &Arc<AtomicBool> {
            &self.interrupt
        }
    }

    #[test]
    fn check_preempt_at_checkpoint_consumes_interrupt_flag() {
        let interruption = FlagControl::new();
        interruption.signal_interrupt();

        let outcome = check_preempt_at_checkpoint(
            &interruption,
            IterationId(1),
            IterationCheckpoint::AfterLlmBeforeTool,
            None,
        )
        .expect("preempt");

        assert_eq!(outcome.checkpoint, IterationCheckpoint::AfterLlmBeforeTool);
        assert!(outcome.produced.is_none());
        assert!(!interruption.interrupt_flag().load(Ordering::Acquire));
    }

    #[test]
    fn check_preempt_at_checkpoint_returns_produced_snapshot() {
        let interruption = FlagControl::new();
        interruption.signal_interrupt();

        let produced = AppendedMessages(json!([
            {"role": "assistant"},
            {"role": "tool", "content": "partial"}
        ]));
        let outcome = check_preempt_at_checkpoint(
            &interruption,
            IterationId(1),
            IterationCheckpoint::BeforeTool,
            produced.as_produced_snapshot(),
        )
        .expect("preempt");

        let produced = outcome.produced.expect("produced");
        let items = produced.0.as_array().expect("array");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn llm_http_preempted_accepts_transport_abort_without_flag() {
        let interruption = FlagControl::new();
        let err = ChatError::Api(ClawApiError::Transport(
            "HTTP transport error: HTTP request aborted by caller".into(),
        ));
        assert!(llm_http_preempted(&interruption, &err));
    }

    #[test]
    fn append_assistant_tool_calls_error_paths() {
        let mut messages = Value::Array(Vec::new());
        let missing_raw = LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: None,
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "files".into(),
                arguments_json: "{}".into(),
            }],
        };
        assert!(matches!(
            append_assistant_tool_calls(&mut messages, &missing_raw).unwrap_err(),
            IterationLoopError::MissingAssistantMessage
        ));

        let malformed = LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: Some("{not-json".into()),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "files".into(),
                arguments_json: "{}".into(),
            }],
        };
        assert!(matches!(
            append_assistant_tool_calls(&mut messages, &malformed).unwrap_err(),
            IterationLoopError::MalformedAssistantMessage
        ));

        let valid = LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: Some(r#"{"role":"assistant"}"#.into()),
            tool_calls: vec![],
        };
        let mut not_array = Value::Object(Default::default());
        assert!(matches!(
            append_assistant_tool_calls(&mut not_array, &valid).unwrap_err(),
            IterationLoopError::MessagesNotArray
        ));
    }

    #[test]
    fn run_tool_calls_error_and_empty_name() {
        struct OkTools;
        impl IterationTools for OkTools {
            fn schemas_json(&self) -> Option<&str> {
                None
            }

            fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    output: "done".into(),
                    ok: false,
                })
            }
        }

        let interruption = FlagControl::new();
        let tools = OkTools;
        let iteration_id = IterationId(1);
        let response = LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: None,
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: String::new(),
                arguments_json: "{}".into(),
            }],
        };

        let mut not_array = AppendedMessages(Value::Object(Default::default()));
        assert!(matches!(
            run_tool_calls(&interruption, &tools, &mut not_array, &response, iteration_id),
            ToolRoundResult::Failed(IterationLoopError::MessagesNotArray)
        ));

        let mut appended = AppendedMessages::empty();
        append_assistant_tool_calls(
            &mut appended.0,
            &LlmResponse {
                text: None,
                reasoning_content: None,
                raw_message_json: Some(r#"{"role":"assistant"}"#.into()),
                tool_calls: vec![],
            },
        )
        .expect("assistant");

        let ToolRoundResult::Completed { runs } =
            run_tool_calls(&interruption, &tools, &mut appended, &response, iteration_id)
        else {
            panic!("expected completed tool round");
        };
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].name, "(null)");
        assert!(!runs[0].ok);
        assert_eq!(appended.0[1]["is_error"], true);
    }

    #[test]
    fn log_tool_call_names_handles_empty_and_null_names() {
        log_tool_call_names(IterationId(1), &LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: None,
            tool_calls: vec![],
        });

        log_tool_call_names(
            IterationId(2),
            &LlmResponse {
                text: None,
                reasoning_content: None,
                raw_message_json: None,
                tool_calls: vec![
                    ToolCall {
                        id: "t1".into(),
                        name: String::new(),
                        arguments_json: "{}".into(),
                    },
                    ToolCall {
                        id: "t2".into(),
                        name: "files".into(),
                        arguments_json: "{}".into(),
                    },
                ],
            },
        );
    }

    #[test]
    fn iteration_id_serializes_to_prefixed_string() {
        let value = serde_json::to_value(IterationId(5)).unwrap();
        assert_eq!(value, serde_json::json!("iteration-5"));
    }

    #[test]
    fn iteration_id_deserializes_from_prefixed_string() {
        let iteration: IterationId =
            serde_json::from_value(serde_json::json!("iteration-4")).unwrap();
        assert_eq!(iteration, IterationId(4));
        assert_eq!(iteration.to_string(), "iteration-4");
    }

    #[test]
    fn append_assistant_tool_calls_accepts_valid_message() {
        let mut messages = Value::Array(Vec::new());
        let response = LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: Some(r#"{"role":"assistant","tool_calls":[]}"#.into()),
            tool_calls: vec![],
        };
        append_assistant_tool_calls(&mut messages, &response).expect("append assistant");
        assert_eq!(messages[0]["role"], "assistant");
    }
}
