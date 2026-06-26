//! The agent's built-in tools and the control channels they speak through.
//!
//! Internal tools are model-callable like any other [`Tool`], but instead of
//! returning a result to the conversation they steer the *agent graph*. A tool
//! handler only has `&self`, so it cannot touch state directly — it routes
//! through one of two seams:
//! - **self-affecting** control (`end_conversation`, `request_approval`) pushes a
//!   [`ControlSignal`] onto the agent's own [`ControlSink`], drained each tick;
//! - **graph-affecting** actions (`spawn_subagent`, `respond_to_approval`) emit a
//!   [`GraphEffect`] through an [`AgentContext`]/[`GraphHost`], queued and applied
//!   by the orchestrator instance after the tick.
//!
//! Which tools an agent actually gets is a build-time knob: `spawn_subagent` only
//! when its manifest enables spawning, `respond_to_approval` only for a session
//! root.
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

/// Group label for the subagent-management tools (provenance only).
pub(crate) const SUBAGENT_TOOL_GROUP: &str = "subagents";

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

/// What becomes of a subagent once it yields a result.
///
/// A subagent runs to a result and reports it to its parent regardless; this
/// policy only decides whether it then *lingers*:
/// - [`AutoOnIdle`](Self::AutoOnIdle) (default): one-shot — the subagent is
///   removed as soon as it yields, so its id immediately becomes invalid.
/// - [`Manual`](Self::Manual): persistent — after yielding (still delivering its
///   result to the parent) the subagent stays alive and idle, so the parent can
///   observe it, hand it more work, or delete it explicitly. It is removed only on
///   an explicit delete, on its parent's removal (cascade), or at session end.
///   Terminal outcomes (end / cancel / fail) always remove it, even under
///   `Manual` — there is nothing left to re-task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminationPolicy {
    /// Remove the subagent as soon as it yields (one-shot).
    #[default]
    AutoOnIdle,
    /// Keep the subagent alive and idle after it yields, until explicitly removed.
    Manual,
}

impl TerminationPolicy {
    /// A short, stable, model-facing label (the inverse of [`parse`](Self::parse)).
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminationPolicy::AutoOnIdle => "auto",
            TerminationPolicy::Manual => "manual",
        }
    }

    /// Parse the `spawn_subagent` tool's `termination` argument; empty defaults to
    /// [`AutoOnIdle`](Self::AutoOnIdle).
    ///
    /// # Errors
    ///
    /// [`ToolError::InvokeFailed`] for any value other than `auto` / `manual`.
    fn parse(raw: &str) -> Result<Self, ToolError> {
        match raw.trim() {
            "" | "auto" => Ok(Self::AutoOnIdle),
            "manual" => Ok(Self::Manual),
            other => Err(ToolError::InvokeFailed(format!(
                "spawn_subagent 'termination' must be one of auto|manual, got '{other}'"
            ))),
        }
    }
}

// -- Graph back-channel: the one seam tools use to touch the agent graph ----
//
// Self-affecting control (end / approval) flows through the [`ControlSink`]
// above. Everything that touches *another* node of the agent graph — spawning a
// child, resolving a waiting agent's approval, (later) deleting a subtree — is a
// [`GraphEffect`] the tool *emits* through a [`GraphHost`]. The host (the
// orchestrator instance) queues the effect and applies it after the tick, at a
// borrow-safe point, so a tool never mutates the live graph mid-tick.

/// A graph / lifecycle mutation an agent requests during its tick.
///
/// Emitted via [`GraphHost::emit`] and applied by the orchestrator instance once
/// the requesting agent's tick has returned (never mid-tick). A new internal tool
/// adds a variant here rather than a new seam, keeping [`GraphHost`] stable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEffect {
    /// Create a child of `kind` for `goal`, parented to the emitter, with the
    /// given lifecycle policy. `id` was handed out by [`GraphHost::next_id`] so the
    /// requesting tool could report it to the model synchronously.
    Spawn {
        /// The id pre-allocated for the child.
        id: AgentId,
        /// Which kind (role/template) of agent to create.
        kind: AgentKind,
        /// The goal handed to the child.
        goal: String,
        /// What becomes of the child once it yields (one-shot vs. persistent).
        termination: TerminationPolicy,
    },
    /// Resolve `target`'s pending approval with the root's classified `verdict`
    /// (and optional free-text `note`).
    ResolveApproval {
        /// The agent whose pending approval is being resolved.
        target: AgentId,
        /// The root's classification of the user's reply.
        verdict: ApprovalVerdict,
        /// The user's words / reason, used when rejecting.
        note: Option<String>,
    },
    /// Remove `target` and its whole subtree. The instance honors this only when
    /// `target` is a descendant of the emitter (an agent may reap its own
    /// subagents, not arbitrary nodes).
    Delete {
        /// The subagent to remove (with everything beneath it).
        target: AgentId,
    },
}

/// A subagent's coarse lifecycle state, as observed by its parent through
/// [`AgentContext::list_subagents`] / [`AgentContext::get_subagent`].
///
/// Derived by the orchestrator instance from its own scheduling state, not asked
/// of the agent — so it reflects what the graph owner knows: queued, parked, or
/// neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Queued to run on the next drive step.
    Ready,
    /// Parked on a human decision (an approval is outstanding).
    AwaitingApproval,
    /// Alive with nothing queued (e.g. a persistent subagent that has yielded).
    Idle,
}

impl AgentStatus {
    /// A short, stable, model-facing label.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Ready => "ready",
            AgentStatus::AwaitingApproval => "awaiting_approval",
            AgentStatus::Idle => "idle",
        }
    }
}

/// An observable view of one live agent — what a parent sees when it lists or
/// watches its subagents. A read-only projection of the graph owner's state,
/// taken as of the start of the requesting agent's tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    /// The agent's id.
    pub id: AgentId,
    /// Its kind (role/template).
    pub kind: AgentKind,
    /// Its creator, or `None` for a session root.
    pub parent: Option<AgentId>,
    /// Distance from the root (root = 0).
    pub depth: u16,
    /// What becomes of it once it yields (one-shot vs. persistent).
    pub termination: TerminationPolicy,
    /// Its coarse lifecycle state.
    pub status: AgentStatus,
}

/// The services an agent's graph owner exposes to that agent's internal tools.
///
/// This is the *single* back-channel from an agent to the orchestrator: identity
/// allocation ([`next_id`](Self::next_id)) plus a deferred-effect queue
/// ([`emit`](Self::emit)). The orchestrator instance implements it; tools reach
/// it through an [`AgentContext`]. Keeping the surface to these two methods means
/// new tools extend [`GraphEffect`], not this trait.
pub trait GraphHost: Send + Sync {
    /// Allocate the next process-unique [`AgentId`] (e.g. for a child a tool is
    /// about to request), so the tool can report it before the child is built.
    fn next_id(&self) -> AgentId;

    /// Queue `effect`, requested by `requester`, to be applied after the current
    /// tick at a borrow-safe point. Never touches the live graph itself.
    fn emit(&self, requester: AgentId, effect: GraphEffect);

    /// A consistent read-only snapshot of every live agent, as of the start of the
    /// current tick. Reads (`list_subagents` / `watch_subagent`) return data to the
    /// model synchronously, so unlike [`emit`](Self::emit) this is not deferred.
    fn snapshot(&self) -> Vec<AgentSnapshot>;
}

/// The handle an agent's internal tools call to act on the agent graph.
///
/// One per agent, built at construction over the agent's own id and its
/// [`GraphHost`]. It is the ergonomic façade over [`GraphHost`]: tools call typed
/// methods ([`spawn`](Self::spawn), [`respond_to_approval`](Self::respond_to_approval))
/// and never touch [`GraphEffect`] or the queue directly. Self-affecting control
/// (`end_conversation`, `request_approval`) does *not* go through here — it stays
/// on the agent's own [`ControlSink`].
pub(crate) struct AgentContext {
    /// The owning agent's id — stamped as the `requester`/parent on every effect.
    id: AgentId,
    /// The graph owner this context emits effects to and draws ids from.
    host: Arc<dyn GraphHost>,
}

impl AgentContext {
    /// Build a context for agent `id` over its graph `host`.
    pub(crate) fn new(id: AgentId, host: Arc<dyn GraphHost>) -> Self {
        Self { id, host }
    }

    /// Request a child of `kind` for `goal` with lifecycle `termination`; returns
    /// the id assigned to the child (allocated now, built after the tick).
    pub(crate) fn spawn(
        &self,
        kind: AgentKind,
        goal: String,
        termination: TerminationPolicy,
    ) -> AgentId {
        let child = self.host.next_id();
        self.host.emit(
            self.id,
            GraphEffect::Spawn {
                id: child,
                kind,
                goal,
                termination,
            },
        );
        child
    }

    /// Report the root's `verdict` (with optional `note`) for `target`'s pending
    /// approval.
    pub(crate) fn respond_to_approval(
        &self,
        target: AgentId,
        verdict: ApprovalVerdict,
        note: Option<String>,
    ) {
        self.host.emit(
            self.id,
            GraphEffect::ResolveApproval {
                target,
                verdict,
                note,
            },
        );
    }

    /// Request removal of `target` (and its subtree). The instance ignores it
    /// unless `target` is a descendant of this agent.
    pub(crate) fn delete_subagent(&self, target: AgentId) {
        self.host.emit(self.id, GraphEffect::Delete { target });
    }

    /// Every agent in this agent's subtree (its strict descendants), sorted by
    /// `(depth, id)` for a stable, parent-before-child reading order.
    pub(crate) fn list_subagents(&self) -> Vec<AgentSnapshot> {
        let all = self.host.snapshot();
        let mut descendants: Vec<AgentSnapshot> = all
            .iter()
            .filter(|snapshot| is_strict_descendant(&all, self.id, snapshot.id))
            .cloned()
            .collect();
        descendants.sort_by_key(|snapshot| (snapshot.depth, snapshot.id.0));
        descendants
    }

    /// The snapshot of `target`, but only if it is a descendant of this agent
    /// (so an agent cannot watch a sibling, an ancestor, or itself). `None`
    /// otherwise — an unknown id or one outside this agent's subtree.
    pub(crate) fn get_subagent(&self, target: AgentId) -> Option<AgentSnapshot> {
        let all = self.host.snapshot();
        is_strict_descendant(&all, self.id, target)
            .then(|| all.into_iter().find(|snapshot| snapshot.id == target))
            .flatten()
    }
}

/// The parent of `id` in a snapshot set, or `None` if absent / a root.
fn snapshot_parent(all: &[AgentSnapshot], id: AgentId) -> Option<AgentId> {
    all.iter()
        .find(|snapshot| snapshot.id == id)
        .and_then(|snapshot| snapshot.parent)
}

/// Whether `node` is a strict descendant of `ancestor` (walking parent edges in
/// the snapshot). `false` when `node == ancestor` or the chain never reaches it.
fn is_strict_descendant(all: &[AgentSnapshot], ancestor: AgentId, node: AgentId) -> bool {
    let mut current = snapshot_parent(all, node);
    while let Some(parent) = current {
        if parent == ancestor {
            return true;
        }
        current = snapshot_parent(all, parent);
    }
    false
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

/// Build the subagent-management tool group, all routed through `context`
/// (parented to / scoped by the context's agent):
/// - `spawn_subagent` — create a child (restricted to `policy`'s allowed kinds);
/// - `list_subagents` — enumerate this agent's subtree;
/// - `watch_subagent` — snapshot one descendant;
/// - `delete_subagent` — remove one descendant (and its subtree).
pub(crate) fn subagent_tool_group(context: Arc<AgentContext>, policy: SpawnPolicy) -> ToolGroup {
    ToolGroup::new(
        SUBAGENT_TOOL_GROUP,
        [
            Tool::new(SpawnSubagentTool {
                context: Arc::clone(&context),
                policy,
            }),
            Tool::new(ListSubagentsTool {
                context: Arc::clone(&context),
            }),
            Tool::new(WatchSubagentTool {
                context: Arc::clone(&context),
            }),
            Tool::new(DeleteSubagentTool { context }),
        ],
    )
}

/// `spawn_subagent(kind, goal)` — request a child agent of `kind` to work on `goal`.
struct SpawnSubagentTool {
    context: Arc<AgentContext>,
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
            tracing::warn!(requested_kind = %kind, "spawn kind rejected by allowed_kinds");
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
        // Optional lifecycle policy: default one-shot (`auto`); `manual` keeps the
        // child alive and idle after it yields so this agent can supervise it.
        let termination =
            TerminationPolicy::parse(&string_argument(call.arguments_json, "termination")?)?;
        let child = self.context.spawn(kind, goal, termination);
        Ok(ToolOutput {
            output: format!(
                "Subagent {child} requested; its result will be reported back when it finishes."
            ),
            ok: true,
        })
    }
}

/// Render a snapshot as a compact JSON object for the model to read.
fn snapshot_json(snapshot: &AgentSnapshot) -> Value {
    serde_json::json!({
        "agent": snapshot.id.to_string(),
        "kind": snapshot.kind.as_str(),
        "parent": snapshot.parent.map(|parent| parent.to_string()),
        "depth": snapshot.depth,
        "status": snapshot.status.as_str(),
        "termination": snapshot.termination.as_str(),
    })
}

/// `list_subagents()` — enumerate this agent's subtree.
struct ListSubagentsTool {
    context: Arc<AgentContext>,
}

impl ToolHandler for ListSubagentsTool {
    tool_metadata!("list_subagents");

    fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let subagents: Vec<Value> = self
            .context
            .list_subagents()
            .iter()
            .map(snapshot_json)
            .collect();
        Ok(ToolOutput {
            output: serde_json::json!({ "subagents": subagents }).to_string(),
            ok: true,
        })
    }
}

/// `watch_subagent(agent)` — snapshot one descendant by id.
struct WatchSubagentTool {
    context: Arc<AgentContext>,
}

impl ToolHandler for WatchSubagentTool {
    tool_metadata!("watch_subagent");

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let agent = string_argument(call.arguments_json, "agent")?;
        let target = AgentId::from_wire(agent.trim()).map_err(|error| {
            ToolError::InvokeFailed(format!("invalid agent id '{agent}': {error}"))
        })?;
        match self.context.get_subagent(target) {
            Some(snapshot) => Ok(ToolOutput {
                output: snapshot_json(&snapshot).to_string(),
                ok: true,
            }),
            None => Ok(ToolOutput {
                output: format!(
                    "No subagent {target} in your subtree (unknown id, or not one of yours)."
                ),
                ok: false,
            }),
        }
    }
}

/// `delete_subagent(agent)` — remove one descendant (and its subtree) by id.
struct DeleteSubagentTool {
    context: Arc<AgentContext>,
}

impl ToolHandler for DeleteSubagentTool {
    tool_metadata!("delete_subagent");

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let agent = string_argument(call.arguments_json, "agent")?;
        let target = AgentId::from_wire(agent.trim()).map_err(|error| {
            ToolError::InvokeFailed(format!("invalid agent id '{agent}': {error}"))
        })?;
        // Authorize against the same subtree view watch uses, so the refusal is
        // immediate (the instance re-checks before actually removing).
        if self.context.get_subagent(target).is_none() {
            return Ok(ToolOutput {
                output: format!(
                    "Cannot delete {target}: it is not a subagent in your subtree."
                ),
                ok: false,
            });
        }
        self.context.delete_subagent(target);
        Ok(ToolOutput {
            output: format!("Subagent {target} and its subtree scheduled for deletion."),
            ok: true,
        })
    }
}

// -- Root-side approval resolution ------------------------------------------
//
// Only the session root talks to the user, so any agent's `request_approval`
// bubbles up to the root (done by the orchestrator instance). The root presents
// it, reads the user's free-text reply, classifies it into yes/no/other, and
// reports the verdict back with the `respond_to_approval` tool. Like
// `spawn_subagent`, this affects *another* agent, so it emits a
// [`GraphEffect::ResolveApproval`] through the agent's [`AgentContext`] for the
// instance to apply at a borrow-safe point rather than touching the graph here.

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

/// Build the approval-response tool group: a `respond_to_approval` tool that
/// reports verdicts through `context`.
pub(crate) fn respond_to_approval_tool_group(context: Arc<AgentContext>) -> ToolGroup {
    ToolGroup::new(
        APPROVAL_TOOL_GROUP,
        [Tool::new(RespondToApprovalTool { context })],
    )
}

/// `respond_to_approval(agent, verdict, note)` — the root reports its
/// classification of a user's reply to a subagent's approval request.
struct RespondToApprovalTool {
    context: Arc<AgentContext>,
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

        self.context.respond_to_approval(target, verdict, note);
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

    /// A [`GraphHost`] that records every emitted effect, hands out ascending ids
    /// from a private counter, and returns a preset snapshot.
    #[derive(Default)]
    struct RecordingHost {
        next: Mutex<usize>,
        effects: Mutex<Vec<(AgentId, GraphEffect)>>,
        snapshot: Mutex<Vec<AgentSnapshot>>,
    }

    impl GraphHost for RecordingHost {
        fn next_id(&self) -> AgentId {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            AgentId(*next)
        }
        fn emit(&self, requester: AgentId, effect: GraphEffect) {
            self.effects.lock().unwrap().push((requester, effect));
        }
        fn snapshot(&self) -> Vec<AgentSnapshot> {
            self.snapshot.lock().unwrap().clone()
        }
    }

    /// A minimal snapshot of `id` parented to `parent` for the read-path tests.
    fn snap(id: usize, parent: Option<usize>, depth: u16) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId(id),
            kind: AgentKind::new("worker"),
            parent: parent.map(AgentId),
            depth,
            termination: TerminationPolicy::Manual,
            status: AgentStatus::Idle,
        }
    }

    /// Build a host whose snapshot is `tree`.
    fn host_with_tree(tree: Vec<AgentSnapshot>) -> Arc<RecordingHost> {
        let host = RecordingHost::default();
        *host.snapshot.lock().unwrap() = tree;
        Arc::new(host)
    }

    fn tool_named(group: &ToolGroup, name: &str) -> Tool {
        group
            .tools()
            .iter()
            .find(|tool| tool.name() == name)
            .unwrap()
            .clone()
    }

    /// The kinds recorded as `Spawn` effects on `host`.
    fn spawned_kinds(host: &RecordingHost) -> Vec<AgentKind> {
        host.effects
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, effect)| match effect {
                GraphEffect::Spawn { kind, .. } => Some(kind.clone()),
                GraphEffect::ResolveApproval { .. } | GraphEffect::Delete { .. } => None,
            })
            .collect()
    }

    fn context_for(host: Arc<dyn GraphHost>, id: AgentId) -> Arc<AgentContext> {
        Arc::new(AgentContext::new(id, host))
    }

    #[test]
    fn termination_policy_parses_auto_manual_and_rejects_others() {
        assert_eq!(
            TerminationPolicy::parse("").unwrap(),
            TerminationPolicy::AutoOnIdle
        );
        assert_eq!(
            TerminationPolicy::parse("auto").unwrap(),
            TerminationPolicy::AutoOnIdle
        );
        assert_eq!(
            TerminationPolicy::parse("manual").unwrap(),
            TerminationPolicy::Manual
        );
        assert!(matches!(
            TerminationPolicy::parse("forever"),
            Err(ToolError::InvokeFailed(_))
        ));
    }

    fn spawn_tool(host: Arc<RecordingHost>, policy: SpawnPolicy) -> Tool {
        let context = context_for(host as Arc<dyn GraphHost>, AgentId(1));
        tool_named(&subagent_tool_group(context, policy), "spawn_subagent")
    }

    #[test]
    fn spawn_rejects_disallowed_kind_without_spawning() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(
            Arc::clone(&host),
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"researcher","goal":"x"}"#,
            })
            .unwrap();

        // Refused as a matched tool error (not Err), and no spawn was emitted.
        assert!(!output.ok);
        assert!(output.output.contains("not permitted"));
        assert!(output.output.contains("worker"));
        assert!(host.effects.lock().unwrap().is_empty());
    }

    #[test]
    fn spawn_allows_permitted_kind() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(
            Arc::clone(&host),
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
        assert_eq!(spawned_kinds(&host), &[AgentKind::new("worker")]);
    }

    #[test]
    fn spawn_policy_any_allows_every_kind() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(Arc::clone(&host), SpawnPolicy::Any);

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"anything","goal":"x"}"#,
            })
            .unwrap();

        assert!(output.ok);
        assert_eq!(spawned_kinds(&host), &[AgentKind::new("anything")]);
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
    fn list_subagents_returns_strict_descendants_sorted() {
        let tree = vec![
            snap(1, None, 0),
            snap(2, Some(1), 1),
            snap(3, Some(2), 2),
            snap(9, None, 0),
        ];
        let host = host_with_tree(tree);

        let ctx1 = context_for(Arc::clone(&host) as Arc<dyn GraphHost>, AgentId(1));
        let ids: Vec<usize> = ctx1.list_subagents().iter().map(|s| s.id.0).collect();
        assert_eq!(ids, vec![2, 3]);

        let ctx2 = context_for(host as Arc<dyn GraphHost>, AgentId(2));
        let ids: Vec<usize> = ctx2.list_subagents().iter().map(|s| s.id.0).collect();
        assert_eq!(ids, vec![3]);
    }

    #[test]
    fn get_subagent_authorizes_to_the_subtree() {
        let host = host_with_tree(vec![
            snap(1, None, 0),
            snap(2, Some(1), 1),
            snap(3, Some(2), 2),
            snap(9, None, 0),
        ]);
        let ctx2 = context_for(host as Arc<dyn GraphHost>, AgentId(2));
        assert!(ctx2.get_subagent(AgentId(3)).is_some(), "a descendant");
        assert!(ctx2.get_subagent(AgentId(1)).is_none(), "an ancestor");
        assert!(ctx2.get_subagent(AgentId(2)).is_none(), "itself");
        assert!(ctx2.get_subagent(AgentId(9)).is_none(), "unrelated");
    }

    #[test]
    fn delete_subagent_emits_only_for_a_descendant() {
        let host = host_with_tree(vec![
            snap(1, None, 0),
            snap(2, Some(1), 1),
            snap(3, Some(2), 2),
        ]);
        let context = context_for(Arc::clone(&host) as Arc<dyn GraphHost>, AgentId(2));
        let delete = tool_named(&subagent_tool_group(context, SpawnPolicy::Any), "delete_subagent");

        let ok = delete
            .invoke(&ToolInvocation {
                id: Some("d1"),
                name: "delete_subagent",
                arguments_json: r#"{"agent":"agent-3"}"#,
            })
            .unwrap();
        assert!(ok.ok);

        let refused = delete
            .invoke(&ToolInvocation {
                id: Some("d2"),
                name: "delete_subagent",
                arguments_json: r#"{"agent":"agent-1"}"#,
            })
            .unwrap();
        assert!(!refused.ok);

        // Only the descendant delete was emitted.
        let effects = host.effects.lock().unwrap();
        assert_eq!(
            effects.as_slice(),
            &[(
                AgentId(2),
                GraphEffect::Delete {
                    target: AgentId(3)
                }
            )]
        );
    }

    #[test]
    fn watch_subagent_reports_snapshot_or_refuses() {
        let host = host_with_tree(vec![snap(1, None, 0), snap(2, Some(1), 1)]);
        let context = context_for(host as Arc<dyn GraphHost>, AgentId(1));
        let watch = tool_named(&subagent_tool_group(context, SpawnPolicy::Any), "watch_subagent");

        let ok = watch
            .invoke(&ToolInvocation {
                id: Some("w1"),
                name: "watch_subagent",
                arguments_json: r#"{"agent":"agent-2"}"#,
            })
            .unwrap();
        assert!(ok.ok);
        assert!(ok.output.contains("agent-2"));
        assert!(ok.output.contains("manual"));

        // Watching itself (not a descendant) is refused.
        let refused = watch
            .invoke(&ToolInvocation {
                id: Some("w2"),
                name: "watch_subagent",
                arguments_json: r#"{"agent":"agent-1"}"#,
            })
            .unwrap();
        assert!(!refused.ok);
    }

    #[test]
    fn respond_to_approval_parses_target_and_verdict() {
        let host = Arc::new(RecordingHost::default());
        let context = context_for(Arc::clone(&host) as Arc<dyn GraphHost>, AgentId(1));

        let group = respond_to_approval_tool_group(context);
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

        let effects = host.effects.lock().unwrap();
        assert_eq!(
            effects.as_slice(),
            &[(
                AgentId(1),
                GraphEffect::ResolveApproval {
                    target: AgentId(7),
                    verdict: ApprovalVerdict::No,
                    note: Some("not allowed".to_string()),
                }
            )]
        );
    }

    #[test]
    fn respond_to_approval_rejects_unknown_verdict() {
        let host = Arc::new(RecordingHost::default());
        let context = context_for(host as Arc<dyn GraphHost>, AgentId(1));
        let group = respond_to_approval_tool_group(context);
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
