//! The agent's built-in tools and the control channels they speak through.
//!
//! Internal tools are model-callable like any other [`Tool`], but instead of
//! returning a result to the conversation they steer the *agent itself*: ending
//! the conversation, requesting human approval, or spawning a subagent. A tool
//! handler only has `&self`, so it cannot touch the agent's state directly — it
//! pushes onto a shared sink (a [`ControlSink`] for end/approval, a [`SpawnSink`]
//! for spawn) that the owning agent drains each tick.
//!
//! `spawn_subagent` is shared by both semantic agents (whether it is present at
//! all is a build-time knob); the actual creation of a child agent is implemented
//! in the multi-agent phase.
//!
//! Keeping this in one place means [`IterationLoop`](crate::iteration_loop) stays
//! fully agnostic: it runs these like ordinary tools and never learns their
//! meaning.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::tools::{Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput};

/// Group label for the agent's built-in tools (provenance only).
pub(crate) const INTERNAL_TOOL_GROUP: &str = "agent";

/// Name of the built-in `end_conversation` tool.
pub(crate) const END_CONVERSATION: &str = "end_conversation";
/// Name of the built-in `request_approval` tool.
pub(crate) const REQUEST_APPROVAL: &str = "request_approval";
/// Name of the shared `spawn_subagent` tool.
pub(crate) const SPAWN_SUBAGENT: &str = "spawn_subagent";
/// Group label for the spawn tool (provenance only).
pub(crate) const SPAWN_TOOL_GROUP: &str = "spawn";

/// A signal an internal tool raises for the agent to act on next tick.
///
/// These are *internal*: they are not part of the public `AgentCommand` surface,
/// so a caller cannot forge an end-of-conversation or an approval request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ControlSignal {
    /// The agent decided it is done; carries its closing message.
    EndConversation { final_message: String },
    /// The agent wants a human decision before continuing; carries the summary
    /// shown to the approver.
    ApprovalRequested { summary: String },
}

/// The shared queue internal tools push [`ControlSignal`]s onto.
///
/// The agent owns one; each internal tool handler holds a clone. A `Mutex`
/// (not a bare cell) because [`ToolHandler`] is `Send + Sync`; contention is nil
/// in the single-driver-thread model.
pub(crate) type ControlSink = Arc<Mutex<VecDeque<ControlSignal>>>;

/// Build the agent's built-in tool group over a control sink.
pub(crate) fn internal_tool_group(sink: ControlSink) -> ToolGroup {
    ToolGroup::new(
        INTERNAL_TOOL_GROUP,
        [
            Tool::new(EndConversationTool {
                sink: Arc::clone(&sink),
            }),
            Tool::new(RequestApprovalTool { sink }),
        ],
    )
}

/// Read one string argument out of a tool call, or `""` when it is absent.
///
/// # Errors
///
/// [`ToolError::InvokeFailed`] if the arguments are present but not valid JSON —
/// a malformed call is surfaced, not swallowed.
pub(crate) fn string_argument(arguments_json: &str, key: &str) -> Result<String, ToolError> {
    if arguments_json.trim().is_empty() {
        return Ok(String::new());
    }
    let value: Value = serde_json::from_str(arguments_json).map_err(|error| {
        ToolError::InvokeFailed(format!("invalid tool arguments JSON: {error}"))
    })?;
    Ok(value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

fn push(sink: &ControlSink, signal: ControlSignal) {
    sink.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push_back(signal);
}

/// `end_conversation(final_message)` — the agent ends the task on its own terms.
struct EndConversationTool {
    sink: ControlSink,
}

impl ToolHandler for EndConversationTool {
    fn name(&self) -> &'static str {
        END_CONVERSATION
    }

    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"end_conversation","description":"Terminate the conversation as a safety or ethics circuit-breaker (e.g. an abusive, unsafe, or out-of-bounds request) — NOT for ordinary task completion, which is a normal reply. Provide the final message to show the user.","parameters":{"type":"object","properties":{"final_message":{"type":"string","description":"The closing message for the user."}},"required":["final_message"]}}}"#
    }

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let final_message = string_argument(call.arguments_json, "final_message")?;
        push(&self.sink, ControlSignal::EndConversation { final_message });
        Ok(ToolOutput {
            output: "Conversation ended.".to_string(),
            ok: true,
        })
    }
}

/// `request_approval(summary)` — the agent pauses for a human decision.
struct RequestApprovalTool {
    sink: ControlSink,
}

impl ToolHandler for RequestApprovalTool {
    fn name(&self) -> &'static str {
        REQUEST_APPROVAL
    }

    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"request_approval","description":"Request human approval before taking a consequential action. Execution pauses until a decision is returned.","parameters":{"type":"object","properties":{"summary":{"type":"string","description":"What you want approved, and why."}},"required":["summary"]}}}"#
    }

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let summary = string_argument(call.arguments_json, "summary")?;
        push(&self.sink, ControlSignal::ApprovalRequested { summary });
        Ok(ToolOutput {
            output: "Approval requested; awaiting a human decision.".to_string(),
            ok: true,
        })
    }
}

// -- Shared subagent spawning ----------------------------------------------
//
// The `spawn_subagent` tool only *records intent*: it pushes the requested goal
// onto a `SpawnSink` and returns immediately, mirroring the control tools above.
// The owning agent drains the sink each tick and turns each goal into a child.

/// Queue the `spawn_subagent` tool pushes requested goals onto; the agent drains
/// it each tick. A `Mutex` because [`ToolHandler`] is `Send + Sync`; contention
/// is nil in the single-driver-thread model.
pub(crate) type SpawnSink = Arc<Mutex<VecDeque<String>>>;

/// Build the spawn tool group over a sink.
pub(crate) fn spawn_tool_group(sink: SpawnSink) -> ToolGroup {
    ToolGroup::new(SPAWN_TOOL_GROUP, [Tool::new(SpawnSubagentTool { sink })])
}

/// Drain all pending spawn goals, leaving the sink empty.
pub(crate) fn drain_goals(sink: &SpawnSink) -> Vec<String> {
    sink.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .drain(..)
        .collect()
}

/// `spawn_subagent(goal)` — request a child agent to work on `goal`.
struct SpawnSubagentTool {
    sink: SpawnSink,
}

impl ToolHandler for SpawnSubagentTool {
    fn name(&self) -> &'static str {
        SPAWN_SUBAGENT
    }

    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"spawn_subagent","description":"Delegate a goal to a new subagent that works in parallel. Its result is reported back to you when it finishes.","parameters":{"type":"object","properties":{"goal":{"type":"string","description":"The goal to delegate to the subagent."}},"required":["goal"]}}}"#
    }

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let goal = string_argument(call.arguments_json, "goal")?;
        self.sink
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(goal);
        Ok(ToolOutput {
            output: "Subagent requested; its result will be reported back when it finishes."
                .to_string(),
            ok: true,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sink() -> ControlSink {
        Arc::new(Mutex::new(VecDeque::new()))
    }

    #[test]
    fn end_conversation_pushes_signal_with_message() {
        let sink = sink();
        let group = internal_tool_group(Arc::clone(&sink));
        let tool = group
            .tools()
            .iter()
            .find(|tool| tool.name() == "end_conversation")
            .unwrap()
            .clone();

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "end_conversation",
                arguments_json: r#"{"final_message":"all done"}"#,
            })
            .unwrap();
        assert!(output.ok);

        let signal = sink.lock().unwrap().pop_front().unwrap();
        assert_eq!(
            signal,
            ControlSignal::EndConversation {
                final_message: "all done".to_string()
            }
        );
    }

    #[test]
    fn request_approval_pushes_summary() {
        let sink = sink();
        let group = internal_tool_group(Arc::clone(&sink));
        let tool = group
            .tools()
            .iter()
            .find(|tool| tool.name() == "request_approval")
            .unwrap()
            .clone();

        tool.invoke(&ToolInvocation {
            id: Some("t1"),
            name: "request_approval",
            arguments_json: r#"{"summary":"delete prod"}"#,
        })
        .unwrap();

        let signal = sink.lock().unwrap().pop_front().unwrap();
        assert_eq!(
            signal,
            ControlSignal::ApprovalRequested {
                summary: "delete prod".to_string()
            }
        );
    }

    #[test]
    fn malformed_arguments_error_not_swallowed() {
        let sink = sink();
        let group = internal_tool_group(sink);
        let tool = group
            .tools()
            .iter()
            .find(|tool| tool.name() == "end_conversation")
            .unwrap()
            .clone();

        let error = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "end_conversation",
                arguments_json: "{not json",
            })
            .unwrap_err();
        assert!(matches!(error, ToolError::InvokeFailed(_)));
    }
}
