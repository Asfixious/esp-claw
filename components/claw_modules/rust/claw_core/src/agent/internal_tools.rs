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

use crate::agent::agent::AgentKind;
use crate::agent::base_agent::AgentId;
use crate::tools::{
    tool_metadata, Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput,
};

/// Group label for the agent's built-in tools (provenance only).
pub(crate) const INTERNAL_TOOL_GROUP: &str = "agent";

/// Group label for the spawn tool (provenance only).
pub(crate) const SPAWN_TOOL_GROUP: &str = "spawn";

/// Group label for the approval-response tool (provenance only).
pub(crate) const APPROVAL_TOOL_GROUP: &str = "approval";

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
    tool_metadata!("end_conversation");

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
    tool_metadata!("request_approval");

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
// The `spawn_subagent` tool delegates *creation* to an injected [`Spawner`] (the
// agent-graph owner, e.g. the registry). The model picks which `kind` of agent to
// spawn and the `goal` to hand it; the `Spawner` allocates the child's id
// synchronously and returns it (the tool reports it back to the model), but
// materializes the child later, at a borrow-safe point — it must never insert
// into the node map that owns the currently-ticking parent. The flat agent does
// not track its children locally: a child's result re-enters via
// `deliver_child_result`, so no per-agent spawn bookkeeping is needed.

/// Dependency-injection seam for creating subagents.
///
/// The agent-graph owner (an `AgentRegistry`/orchestrator) implements this so a
/// `spawn_subagent` tool can request a child without knowing how the graph is
/// stored. [`spawn`](Self::spawn) allocates and returns the child's id
/// synchronously; the implementor queues the actual creation for a borrow-safe
/// point rather than mutating the live node map mid-tick.
pub trait Spawner: Send + Sync {
    /// Request a child of `kind` for `goal`, parented to `parent`; returns the id
    /// assigned to the child.
    fn spawn(&self, parent: AgentId, kind: AgentKind, goal: String) -> AgentId;
}

/// Which kinds an agent may spawn — the resolved, runtime form of a manifest's
/// `spawn.allowed_kinds`.
///
/// The wildcard `"*"` is normalized to [`Any`](Self::Any) at resolution time, so
/// the magic string never reaches the per-call check; every other list becomes a
/// concrete [`Only`](Self::Only) allow-set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpawnPolicy {
    /// Any kind may be spawned (`allowed_kinds` contained `"*"`).
    Any,
    /// Only these specific kinds may be spawned.
    Only(Vec<AgentKind>),
}

impl SpawnPolicy {
    /// Resolve a manifest's `allowed_kinds` into a policy: a `"*"` entry anywhere
    /// means [`Any`](Self::Any); otherwise the exact set is kept.
    pub(crate) fn from_allowed_kinds(allowed_kinds: &[AgentKind]) -> Self {
        if allowed_kinds.iter().any(|kind| kind.as_str() == "*") {
            SpawnPolicy::Any
        } else {
            SpawnPolicy::Only(allowed_kinds.to_vec())
        }
    }

    /// Whether `kind` may be spawned under this policy.
    fn allows(&self, kind: &AgentKind) -> bool {
        match self {
            SpawnPolicy::Any => true,
            SpawnPolicy::Only(kinds) => kinds.iter().any(|allowed| allowed == kind),
        }
    }

    /// A short, model-facing description of what is permitted, for the rejection
    /// message handed back when a disallowed kind is requested.
    fn describe(&self) -> String {
        match self {
            SpawnPolicy::Any => "any kind".to_string(),
            SpawnPolicy::Only(kinds) if kinds.is_empty() => "(none)".to_string(),
            SpawnPolicy::Only(kinds) => kinds
                .iter()
                .map(AgentKind::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// Build the spawn tool group: a `spawn_subagent` tool that creates children via
/// `spawner`, parented to `parent`, restricted to `policy`'s allowed kinds.
pub(crate) fn spawn_tool_group(
    spawner: Arc<dyn Spawner>,
    parent: AgentId,
    policy: SpawnPolicy,
) -> ToolGroup {
    ToolGroup::new(
        SPAWN_TOOL_GROUP,
        [Tool::new(SpawnSubagentTool {
            spawner,
            parent,
            policy,
        })],
    )
}

/// `spawn_subagent(kind, goal)` — request a child agent of `kind` to work on `goal`.
struct SpawnSubagentTool {
    spawner: Arc<dyn Spawner>,
    parent: AgentId,
    /// The parent kind's `allowed_kinds`, enforced before any spawn is requested.
    policy: SpawnPolicy,
}

impl ToolHandler for SpawnSubagentTool {
    tool_metadata!("spawn_subagent");

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let kind = string_argument(call.arguments_json, "kind")?;
        if kind.trim().is_empty() {
            return Err(ToolError::InvokeFailed(
                "spawn_subagent requires a non-empty 'kind'".to_string(),
            ));
        }
        let kind = AgentKind::new(kind);

        // Enforce the manifest's `allowed_kinds`. A disallowed kind is refused with
        // a matched tool error (`ok = false`, like soft-hide gating) so the model
        // can pick a permitted kind or do the work itself — not as `Err`, which
        // would fail the whole iteration and rob the model of self-correction.
        if !self.policy.allows(&kind) {
            tracing::warn!(parent_agent = %self.parent, requested_kind = %kind, "spawn kind rejected by allowed_kinds");
            return Ok(ToolOutput {
                output: format!(
                    "spawn_subagent: kind '{kind}' is not permitted for this agent. \
                     Allowed: {}. This is a policy restriction, not a transient error: \
                     pick a permitted kind or handle the work yourself.",
                    self.policy.describe()
                ),
                ok: false,
            });
        }

        let goal = string_argument(call.arguments_json, "goal")?;
        let child = self.spawner.spawn(self.parent, kind, goal);
        Ok(ToolOutput {
            output: format!(
                "Subagent {child} requested; its result will be reported back when it finishes."
            ),
            ok: true,
        })
    }
}

// -- Root-side approval resolution ------------------------------------------
//
// Only the session root talks to the user, so any agent's `request_approval`
// bubbles up to the root (done by the registry). The root presents it, reads the
// user's free-text reply, classifies it into yes/no/other, and reports the
// verdict back with the `respond_to_approval` tool. Like `spawn_subagent`, this
// affects *another* agent, so it delegates to an injected responder that the
// registry drains at a borrow-safe point rather than touching the node map here.

/// The root's classification of a user's reply to a pending approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// A clear yes — the waiting agent is approved.
    Yes,
    /// A clear no — the waiting agent is rejected (with the note as the reason).
    No,
    /// Neither a clear yes nor no — treated as a rejection carrying the user's
    /// words, so the waiting agent can reconsider.
    Other,
}

/// Dependency-injection seam for routing a root's approval verdict back to the
/// agent that requested it.
///
/// The agent-graph owner (an `AgentRegistry`) implements this; the
/// `respond_to_approval` tool calls [`respond`](Self::respond) during the root's
/// tick, and the owner applies the decision to the `target` agent at a
/// borrow-safe point.
pub trait ApprovalResponder: Send + Sync {
    /// Record the root's `verdict` (with an optional free-text `note`) for the
    /// `target` agent's pending approval.
    fn respond(&self, target: AgentId, verdict: ApprovalVerdict, note: Option<String>);
}

/// Build the approval-response tool group: a `respond_to_approval` tool that
/// reports verdicts through `responder`.
pub(crate) fn respond_to_approval_tool_group(responder: Arc<dyn ApprovalResponder>) -> ToolGroup {
    ToolGroup::new(
        APPROVAL_TOOL_GROUP,
        [Tool::new(RespondToApprovalTool { responder })],
    )
}

/// `respond_to_approval(agent, verdict, note)` — the root reports its
/// classification of a user's reply to a subagent's approval request.
struct RespondToApprovalTool {
    responder: Arc<dyn ApprovalResponder>,
}

impl ToolHandler for RespondToApprovalTool {
    tool_metadata!("respond_to_approval");

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let agent = string_argument(call.arguments_json, "agent")?;
        let target = AgentId::from_wire(agent.trim()).map_err(|error| {
            ToolError::InvokeFailed(format!("invalid agent id '{agent}': {error}"))
        })?;

        let verdict_raw = string_argument(call.arguments_json, "verdict")?;
        let verdict = match verdict_raw.trim() {
            "yes" => ApprovalVerdict::Yes,
            "no" => ApprovalVerdict::No,
            "other" => ApprovalVerdict::Other,
            other => {
                return Err(ToolError::InvokeFailed(format!(
                    "respond_to_approval 'verdict' must be one of yes|no|other, got '{other}'"
                )))
            }
        };

        let note_raw = string_argument(call.arguments_json, "note")?;
        let note = (!note_raw.trim().is_empty()).then_some(note_raw);

        self.responder.respond(target, verdict, note);
        Ok(ToolOutput {
            output: format!("Recorded '{verdict_raw}' for {target}."),
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

    /// Records every kind it is asked to spawn; returns a fixed child id.
    struct RecordingSpawner(Mutex<Vec<AgentKind>>);
    impl Spawner for RecordingSpawner {
        fn spawn(&self, _parent: AgentId, kind: AgentKind, _goal: String) -> AgentId {
            self.0.lock().unwrap().push(kind);
            AgentId(99)
        }
    }

    fn spawn_tool(spawner: Arc<dyn Spawner>, policy: SpawnPolicy) -> Tool {
        let group = spawn_tool_group(spawner, AgentId(1), policy);
        group
            .tools()
            .iter()
            .find(|tool| tool.name() == "spawn_subagent")
            .unwrap()
            .clone()
    }

    #[test]
    fn spawn_rejects_disallowed_kind_without_spawning() {
        let spawner = Arc::new(RecordingSpawner(Mutex::new(Vec::new())));
        let tool = spawn_tool(
            Arc::clone(&spawner) as Arc<dyn Spawner>,
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"researcher","goal":"x"}"#,
            })
            .unwrap();

        // Refused as a matched tool error (not Err), and no spawn was queued.
        assert!(!output.ok);
        assert!(output.output.contains("not permitted"));
        assert!(output.output.contains("worker"));
        assert!(spawner.0.lock().unwrap().is_empty());
    }

    #[test]
    fn spawn_allows_permitted_kind() {
        let spawner = Arc::new(RecordingSpawner(Mutex::new(Vec::new())));
        let tool = spawn_tool(
            Arc::clone(&spawner) as Arc<dyn Spawner>,
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"worker","goal":"x"}"#,
            })
            .unwrap();

        assert!(output.ok);
        assert_eq!(
            spawner.0.lock().unwrap().as_slice(),
            &[AgentKind::new("worker")]
        );
    }

    #[test]
    fn spawn_policy_any_allows_every_kind() {
        let spawner = Arc::new(RecordingSpawner(Mutex::new(Vec::new())));
        let tool = spawn_tool(Arc::clone(&spawner) as Arc<dyn Spawner>, SpawnPolicy::Any);

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"anything","goal":"x"}"#,
            })
            .unwrap();

        assert!(output.ok);
        assert_eq!(
            spawner.0.lock().unwrap().as_slice(),
            &[AgentKind::new("anything")]
        );
    }

    #[test]
    fn spawn_policy_resolves_wildcard_to_any() {
        assert_eq!(
            SpawnPolicy::from_allowed_kinds(&[AgentKind::new("*")]),
            SpawnPolicy::Any
        );
        assert_eq!(
            SpawnPolicy::from_allowed_kinds(&[AgentKind::new("worker")]),
            SpawnPolicy::Only(vec![AgentKind::new("worker")])
        );
    }

    #[test]
    fn respond_to_approval_parses_target_and_verdict() {
        let recorded: Arc<Mutex<Vec<(AgentId, ApprovalVerdict, Option<String>)>>> =
            Arc::new(Mutex::new(Vec::new()));

        struct FakeResponder(Arc<Mutex<Vec<(AgentId, ApprovalVerdict, Option<String>)>>>);
        impl ApprovalResponder for FakeResponder {
            fn respond(&self, target: AgentId, verdict: ApprovalVerdict, note: Option<String>) {
                self.0.lock().unwrap().push((target, verdict, note));
            }
        }

        let group = respond_to_approval_tool_group(Arc::new(FakeResponder(Arc::clone(&recorded))));
        let tool = group
            .tools()
            .iter()
            .find(|tool| tool.name() == "respond_to_approval")
            .unwrap()
            .clone();

        tool.invoke(&ToolInvocation {
            id: Some("t1"),
            name: "respond_to_approval",
            arguments_json: r#"{"agent":"agent-7","verdict":"no","note":"not allowed"}"#,
        })
        .unwrap();

        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.as_slice(),
            &[(
                AgentId(7),
                ApprovalVerdict::No,
                Some("not allowed".to_string())
            )]
        );
    }

    #[test]
    fn respond_to_approval_rejects_unknown_verdict() {
        struct NoopResponder;
        impl ApprovalResponder for NoopResponder {
            fn respond(&self, _t: AgentId, _v: ApprovalVerdict, _n: Option<String>) {}
        }
        let group = respond_to_approval_tool_group(Arc::new(NoopResponder));
        let tool = group.tools().iter().next().unwrap().clone();

        let error = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "respond_to_approval",
                arguments_json: r#"{"agent":"agent-1","verdict":"maybe"}"#,
            })
            .unwrap_err();
        assert!(matches!(error, ToolError::InvokeFailed(_)));
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
