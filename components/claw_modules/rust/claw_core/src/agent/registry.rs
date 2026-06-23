//! [`AgentRegistry`] — the flattened agent graph the orchestrator owns.
//!
//! The conversation agent is the per-session **root**; any agent (root or
//! subagent) may spawn further subagents, so the *creation* relationship is a
//! forest: every node has exactly one creator (its `parent`), the root has none,
//! and no cycle can form (a spawn always makes a *new* node).
//!
//! The graph is stored **flat** — one `HashMap<AgentId, Node>` plus a per-node
//! `parent` edge — rather than by nested ownership. This is the idiomatic Rust
//! solution: ticking a node and then delivering its result to its parent would be
//! two overlapping `&mut` borrows under nested ownership, but with id indirection
//! each access borrows exactly one node at a time.
//!
//! Spawning is split across a borrow-safe boundary (see [`Spawner`]): the
//! `spawn_subagent` tool calls [`RegistrySpawner::spawn`] *during* a parent's
//! tick, which only allocates an id and queues a [`SpawnRequest`] (touching a
//! small side queue, never the node map). The registry materializes queued
//! children in [`AgentRegistry::tick_once`] *after* the tick returns, when no node
//! is borrowed.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::agent::AgentKind;
use crate::agent::base_agent::{
    AgentCommand, AgentCommandError, AgentId, ApprovalDecision, ApprovalId, TickOutcome,
};
use crate::agent::internal_tools::{ApprovalResponder, ApprovalVerdict, Spawner};
use crate::agent::Agent;
use crate::session::SessionId;

/// A queued request to create a child agent.
///
/// Produced by [`RegistrySpawner::spawn`] mid-tick and drained by the registry
/// after the requesting agent's tick, so the child is inserted into the node map
/// only at a borrow-safe point.
#[derive(Clone, Debug)]
struct SpawnRequest {
    /// The id allocated to the child (returned to the requester synchronously).
    id: AgentId,
    /// The requesting agent — the child's creator.
    parent: AgentId,
    /// Which kind (role/template) of agent to create.
    kind: AgentKind,
    /// The goal handed to the child.
    goal: String,
}

/// Creates a concrete subagent for a goal.
///
/// Injected so the registry stays free of LLM/memory wiring and unit-testable
/// with a fake factory. The factory must wire the child's spawner to the provided
/// [`Spawner`] handle so the child may itself spawn.
pub trait AgentFactory: Send + Sync {
    /// Build an agent of `kind` with id `id` already tasked with `goal`, giving it
    /// `spawner` so it can spawn its own children. Used for both spawned subagents
    /// and a session's root agent.
    ///
    /// `approval_responder` is `Some` only for a session **root**: the root is the
    /// one agent that talks to the user, so it (and only it) gets the
    /// `respond_to_approval` tool to feed user verdicts back to waiting subagents.
    /// Subagents receive `None`.
    ///
    /// # Errors
    ///
    /// Returns a human-readable error string when `kind` is unknown or the agent
    /// cannot be assembled; the registry logs it and drops the spawn.
    fn create_agent(
        &self,
        id: AgentId,
        kind: &AgentKind,
        goal: String,
        spawner: Arc<dyn Spawner>,
        approval_responder: Option<Arc<dyn ApprovalResponder>>,
    ) -> Result<Box<dyn Agent>, String>;
}

/// The registry-side [`Spawner`]: allocates ids from a shared counter and queues
/// [`SpawnRequest`]s. Cheap to clone (two `Arc`s); never touches the node map.
#[derive(Clone)]
struct RegistrySpawner {
    next_id: Arc<AtomicUsize>,
    pending: Arc<Mutex<VecDeque<SpawnRequest>>>,
}

impl Spawner for RegistrySpawner {
    fn spawn(&self, parent: AgentId, kind: AgentKind, goal: String) -> AgentId {
        let id = AgentId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(SpawnRequest {
                id,
                parent,
                kind,
                goal,
            });
        id
    }
}

/// A queued verdict from the root's `respond_to_approval` tool.
///
/// Produced by [`RegistryApprovalResponder::respond`] during the root's tick and
/// drained by [`AgentRegistry::apply_approval_responses`] afterwards, so the
/// waiting `target` agent is resolved at a borrow-safe point.
#[derive(Clone, Debug)]
struct ApprovalResponse {
    /// The agent whose pending approval the root just classified.
    target: AgentId,
    /// The root's classification of the user's reply.
    verdict: ApprovalVerdict,
    /// The user's words / reason, used when rejecting.
    note: Option<String>,
}

/// The registry-side [`ApprovalResponder`]: queues the root's verdicts. Cheap to
/// clone (one `Arc`); never touches the node map.
#[derive(Clone)]
struct RegistryApprovalResponder {
    pending: Arc<Mutex<VecDeque<ApprovalResponse>>>,
}

impl ApprovalResponder for RegistryApprovalResponder {
    fn respond(&self, target: AgentId, verdict: ApprovalVerdict, note: Option<String>) {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(ApprovalResponse {
                target,
                verdict,
                note,
            });
    }
}

/// One agent instance plus its graph edges, stored flat in the registry.
struct Node {
    agent: Box<dyn Agent>,
    /// This node's kind (role/template).
    #[allow(dead_code)]
    kind: AgentKind,
    /// The creator; `None` for a root.
    parent: Option<AgentId>,
    /// The session this whole subtree belongs to (inherited at spawn).
    session: SessionId,
    /// Distance from the root (root = 0); inherited as `parent.depth + 1`.
    #[allow(dead_code)]
    depth: u16,
}

/// A user-facing reply produced by a **root** agent, surfaced to the orchestrator
/// to route to the session's egress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootReply {
    /// The session whose root produced this reply.
    pub session: SessionId,
    /// The reply text.
    pub text: String,
    /// True when the root *ended* the conversation (via `end_conversation`),
    /// false for an ordinary yielded answer.
    pub ended: bool,
}

/// A pending human decision surfaced out of the graph.
///
/// A **subagent**'s `request_approval` never reaches here — it bubbles to the
/// session root (the only agent that talks to the user), which classifies the
/// reply and resolves it with `respond_to_approval`. Only a **root**'s own
/// approval is surfaced as an `ApprovalRequest`, to be resolved via
/// [`AgentRegistry::resolve_approval`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The session the requesting agent belongs to.
    pub session: SessionId,
    /// The agent awaiting the decision (the resolution target).
    pub agent: AgentId,
    /// The pending approval id to pass back when resolving.
    pub approval: ApprovalId,
    /// Human-readable description of what needs approving.
    pub summary: String,
}

/// Everything one drive step (or a full [`run_until_idle`](AgentRegistry::run_until_idle))
/// surfaced to the orchestrator: user-facing replies and pending approvals.
///
/// Both are surfaced to the session; an `Idle`/`Working`/parked tick contributes
/// nothing, so an empty `DriveOutput` means "nothing to route this step".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriveOutput {
    /// Replies a root produced.
    pub replies: Vec<RootReply>,
    /// Approvals any agent is waiting on.
    pub approvals: Vec<ApprovalRequest>,
}

impl DriveOutput {
    /// Fold another step's output into this one.
    fn absorb(&mut self, other: DriveOutput) {
        self.replies.extend(other.replies);
        self.approvals.extend(other.approvals);
    }

    fn replies(replies: Vec<RootReply>) -> Self {
        Self {
            replies,
            approvals: Vec::new(),
        }
    }

    fn approval(approval: ApprovalRequest) -> Self {
        Self {
            replies: Vec::new(),
            approvals: vec![approval],
        }
    }
}

/// Failure routing a human decision back to an agent via
/// [`AgentRegistry::resolve_approval`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum ResolveApprovalError {
    /// No live agent with the given id (e.g. a finished subagent was removed).
    #[error("no agent {0} to resolve approval for")]
    UnknownAgent(AgentId),
    /// The agent rejected the decision (e.g. it is not awaiting this approval).
    #[error(transparent)]
    Command(AgentCommandError),
}

/// The flattened agent graph: a flat node map keyed by [`AgentId`], a ready queue
/// driving cooperative scheduling, and the shared id counter / spawn queue the
/// [`RegistrySpawner`] writes to.
pub struct AgentRegistry {
    nodes: HashMap<AgentId, Node>,
    ready: VecDeque<AgentId>,
    next_id: Arc<AtomicUsize>,
    pending: Arc<Mutex<VecDeque<SpawnRequest>>>,
    factory: Arc<dyn AgentFactory>,
    /// The root agent of each session — where subagent approvals bubble to.
    roots: HashMap<SessionId, AgentId>,
    /// The pending approval id for each agent currently parked on a decision,
    /// so a root can resolve it by agent id alone (it never sees the approval id).
    parked_approvals: HashMap<AgentId, ApprovalId>,
    /// Verdicts queued by the root's `respond_to_approval` tool, drained after the
    /// tick that produced them.
    approval_pending: Arc<Mutex<VecDeque<ApprovalResponse>>>,
}

impl AgentRegistry {
    /// Create an empty registry that builds agents with `factory`, using a private
    /// id counter.
    ///
    /// Use [`with_id_allocator`](Self::with_id_allocator) instead when several
    /// registries (e.g. one per session) must hand out globally-unique ids.
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        // Start at 1 so `agent-1` is the first id (0 reads like "unset").
        Self::with_id_allocator(factory, Arc::new(AtomicUsize::new(1)))
    }

    /// Create an empty registry that builds agents with `factory` and draws ids
    /// from the shared `next_id` counter.
    ///
    /// The orchestrator passes one global counter to every per-session registry so
    /// that [`AgentId`]s are unique across the whole process, not just within a
    /// session.
    pub fn with_id_allocator(factory: Arc<dyn AgentFactory>, next_id: Arc<AtomicUsize>) -> Self {
        Self {
            nodes: HashMap::new(),
            ready: VecDeque::new(),
            next_id,
            pending: Arc::new(Mutex::new(VecDeque::new())),
            factory,
            roots: HashMap::new(),
            parked_approvals: HashMap::new(),
            approval_pending: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// A [`Spawner`] handle bound to this registry's shared counter and queue.
    ///
    /// Hand it to a root agent at build time; the factory hands the same kind of
    /// handle to each child, so any agent can spawn.
    pub fn spawner(&self) -> Arc<dyn Spawner> {
        Arc::new(RegistrySpawner {
            next_id: Arc::clone(&self.next_id),
            pending: Arc::clone(&self.pending),
        })
    }

    /// An [`ApprovalResponder`] handle bound to this registry's verdict queue.
    ///
    /// Hand it only to a session's root agent (it gets the `respond_to_approval`
    /// tool); the verdicts it queues are applied in
    /// [`apply_approval_responses`](Self::apply_approval_responses).
    pub fn approval_responder(&self) -> Arc<dyn ApprovalResponder> {
        Arc::new(RegistryApprovalResponder {
            pending: Arc::clone(&self.approval_pending),
        })
    }

    /// Allocate a fresh id, e.g. for a root agent the caller is about to build.
    pub fn alloc_id(&self) -> AgentId {
        AgentId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The number of live agents in the graph.
    pub fn agent_count(&self) -> usize {
        self.nodes.len()
    }

    /// True while any agent has work queued.
    pub fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// A read-only transcript snapshot of the live agent `id`, when it keeps one
    /// (for inspection tools / CLIs). `None` if there is no such agent or it has
    /// no transcript.
    pub fn agent_transcript(&self, id: AgentId) -> Option<serde_json::Value> {
        self.nodes.get(&id).and_then(|node| node.agent.transcript())
    }

    /// Register a `root` agent of `kind` (no parent) bound to `session`, and mark
    /// it ready. The root's own [`AgentId`] (from [`alloc_id`](Self::alloc_id)) is
    /// used as its key.
    pub fn add_root(
        &mut self,
        root: Box<dyn Agent>,
        kind: AgentKind,
        session: SessionId,
    ) -> AgentId {
        let id = root.id();
        self.nodes.insert(
            id,
            Node {
                agent: root,
                kind,
                parent: None,
                session,
                depth: 0,
            },
        );
        self.roots.insert(session, id);
        self.enqueue(id);
        id
    }

    /// Build a root agent of `kind` (already tasked with `goal`) via the factory,
    /// register it for `session`, and mark it ready. Returns its [`AgentId`].
    ///
    /// This is the root counterpart to spawned subagents: the registry owns the
    /// factory, so it builds the root itself rather than taking a pre-built agent.
    ///
    /// # Errors
    ///
    /// Propagates the factory's error string when `kind` is unknown or the agent
    /// cannot be assembled.
    pub fn add_root_of_kind(
        &mut self,
        kind: AgentKind,
        session: SessionId,
        goal: String,
    ) -> Result<AgentId, String> {
        let id = self.alloc_id();
        let spawner = self.spawner();
        let responder = self.approval_responder();
        let agent = self
            .factory
            .create_agent(id, &kind, goal, spawner, Some(responder))?;
        Ok(self.add_root(agent, kind, session))
    }

    /// Append a user message to the agent `id` and mark it ready. Returns `false`
    /// if no such agent exists.
    pub fn deliver_to(&mut self, id: AgentId, message: impl Into<String>) -> bool {
        let Some(node) = self.nodes.get_mut(&id) else {
            return false;
        };
        // `AppendMessage` is accepted in every state, so this cannot fail.
        let _ = node
            .agent
            .send_command(AgentCommand::AppendMessage(message.into()));
        self.enqueue(id);
        true
    }

    /// Tick one ready agent and apply the consequences: materialize any children
    /// it spawned this tick, then route its outcome (a subagent result bubbles to
    /// its parent; a root reply or a pending approval is surfaced). Returns what
    /// to route this step (empty when nothing was ready).
    pub fn tick_once(&mut self) -> DriveOutput {
        let Some(id) = self.ready.pop_front() else {
            return DriveOutput::default();
        };

        // Tick in its own scope so the node borrow ends before we mutate the map.
        let outcome = match self.nodes.get_mut(&id) {
            Some(node) => node.agent.tick(),
            // The node was removed since being queued (e.g. a finished child).
            None => return DriveOutput::default(),
        };

        // Borrow-safe: the tool calls only wrote to side queues, not to `nodes`.
        self.materialize_pending();
        self.apply_approval_responses();
        self.route_outcome(id, outcome)
    }

    /// Drive the graph until no agent is ready, collecting every reply and
    /// pending approval. An agent parked on an approval is *not* ready, so the
    /// loop terminates with that approval surfaced for a human decision.
    pub fn run_until_idle(&mut self) -> DriveOutput {
        let mut output = DriveOutput::default();
        loop {
            // A verdict may have been queued externally (a root tool call lands
            // inside `tick_once`, but tests/callers can queue one between drives);
            // applying it here re-enqueues the resolved agent so the loop runs it.
            self.apply_approval_responses();
            if !self.has_ready() {
                break;
            }
            output.absorb(self.tick_once());
        }
        output
    }

    /// Route a human decision back to the agent waiting on `approval`, then mark
    /// it ready so the next [`run_until_idle`](Self::run_until_idle) resumes it.
    ///
    /// # Errors
    ///
    /// [`ResolveApprovalError::UnknownAgent`] if no live agent has `agent` (e.g. a
    /// finished subagent), or [`ResolveApprovalError::Command`] if the agent is not
    /// awaiting this approval.
    pub fn resolve_approval(
        &mut self,
        agent: AgentId,
        approval: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveApprovalError> {
        let node = self
            .nodes
            .get_mut(&agent)
            .ok_or(ResolveApprovalError::UnknownAgent(agent))?;
        node.agent
            .send_command(AgentCommand::ApprovalResult {
                id: approval,
                decision,
            })
            .map_err(ResolveApprovalError::Command)?;
        self.enqueue(agent);
        Ok(())
    }

    /// Record `agent` as parked on `approval`, then either bubble its request to
    /// the session root (a subagent) or surface it to the orchestrator (a root).
    fn park_approval(
        &mut self,
        agent: AgentId,
        approval: ApprovalId,
        summary: String,
    ) -> DriveOutput {
        let Some((session, is_root)) = self
            .nodes
            .get(&agent)
            .map(|node| (node.session, node.parent.is_none()))
        else {
            return DriveOutput::default();
        };
        self.parked_approvals.insert(agent, approval);

        if is_root {
            // The root talks to the user directly; surface it for a human reply.
            return DriveOutput::approval(ApprovalRequest {
                session,
                agent,
                approval,
                summary,
            });
        }

        // A subagent: hand the request to the session root to classify, tagged
        // with the requester's id so the root can address it in
        // `respond_to_approval`.
        match self.roots.get(&session).copied() {
            Some(root_id) => {
                self.deliver_to(
                    root_id,
                    format!("[approval request from {agent}] {summary}"),
                );
            }
            None => log::warn!("no root for session {session}; cannot route approval from {agent}"),
        }
        DriveOutput::default()
    }

    /// Apply verdicts the root queued via `respond_to_approval`: look up each
    /// target's parked approval id and resolve it. `yes` approves; `no` and
    /// `other` reject, carrying the note (the user's words) as the reason.
    fn apply_approval_responses(&mut self) {
        let responses: Vec<ApprovalResponse> = self
            .approval_pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain(..)
            .collect();
        for response in responses {
            let Some(approval) = self.parked_approvals.remove(&response.target) else {
                log::warn!(
                    "respond_to_approval for {} but no approval is parked",
                    response.target
                );
                continue;
            };
            let decision = match response.verdict {
                ApprovalVerdict::Yes => ApprovalDecision::Approved,
                ApprovalVerdict::No | ApprovalVerdict::Other => ApprovalDecision::Rejected(
                    response.note.unwrap_or_else(|| "rejected".to_string()),
                ),
            };
            if let Err(error) = self.resolve_approval(response.target, approval, decision) {
                log::warn!(
                    "failed to resolve approval for {}: {error}",
                    response.target
                );
            }
        }
    }

    /// Insert children queued during the last tick. Each inherits its parent's
    /// session and `depth + 1`; a request whose parent has vanished is dropped.
    fn materialize_pending(&mut self) {
        let requests: Vec<SpawnRequest> = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain(..)
            .collect();
        for request in requests {
            let Some(parent) = self.nodes.get(&request.parent) else {
                log::warn!(
                    "spawn parent {} is gone; dropping child {}",
                    request.parent,
                    request.id
                );
                continue;
            };
            let session = parent.session;
            let depth = parent.depth.saturating_add(1);
            let agent = match self.factory.create_agent(
                request.id,
                &request.kind,
                request.goal,
                self.spawner(),
                // Subagents never classify approvals — only the root does.
                None,
            ) {
                Ok(agent) => agent,
                Err(error) => {
                    log::warn!(
                        "failed to create subagent {} of kind {}: {error}; dropping spawn",
                        request.id,
                        request.kind
                    );
                    continue;
                }
            };
            self.nodes.insert(
                request.id,
                Node {
                    agent,
                    kind: request.kind,
                    parent: Some(request.parent),
                    session,
                    depth,
                },
            );
            self.enqueue(request.id);
        }
    }

    /// Map a tick outcome to graph actions.
    fn route_outcome(&mut self, id: AgentId, outcome: TickOutcome) -> DriveOutput {
        match outcome {
            // Still working: requeue for another step.
            TickOutcome::Working => {
                self.enqueue(id);
                DriveOutput::default()
            }
            // Waiting for input: leave it parked (no longer ready).
            TickOutcome::Idle => DriveOutput::default(),
            // Parked on a human decision. A subagent can't talk to the user, so
            // its request bubbles to the session root for classification; only a
            // root's own approval is surfaced to the orchestrator. Either way the
            // agent stays un-enqueued (parked) until resolved.
            TickOutcome::AwaitingApproval {
                id: approval,
                summary,
            } => self.park_approval(id, approval, summary),
            // A finished answer: bubble to parent or surface to the user.
            TickOutcome::Yielded { text } => {
                DriveOutput::replies(self.route_result(id, text, true, false))
            }
            TickOutcome::Ended { final_message } => {
                DriveOutput::replies(self.route_result(id, final_message, true, true))
            }
            TickOutcome::Cancelled { reason } => DriveOutput::replies(self.route_result(
                id,
                format!("[cancelled: {reason:?}]"),
                false,
                true,
            )),
            TickOutcome::Failed(error) => DriveOutput::replies(self.route_result(
                id,
                format!("[failed: {error:?}]"),
                false,
                true,
            )),
        }
    }

    /// Deliver a finished agent's `text` to its parent (a subagent, removed after
    /// delivery) or surface it as a [`RootReply`] (a root, kept alive for the next
    /// message). `ended` marks an `end_conversation` close for roots.
    fn route_result(&mut self, id: AgentId, text: String, ok: bool, ended: bool) -> Vec<RootReply> {
        let Some((parent, session)) = self.nodes.get(&id).map(|node| (node.parent, node.session))
        else {
            return Vec::new();
        };
        match parent {
            Some(parent_id) => {
                if let Some(parent_node) = self.nodes.get_mut(&parent_id) {
                    parent_node.agent.deliver_child_result(id, text, ok);
                    self.enqueue(parent_id);
                }
                // A subagent is one-shot: it is removed once its result is in.
                self.nodes.remove(&id);
                Vec::new()
            }
            // A root stays alive (the session persists); cleanup is the
            // orchestrator's job when it deletes the session.
            None => vec![RootReply {
                session,
                text,
                ended,
            }],
        }
    }

    /// Mark `id` ready, avoiding duplicate queue entries.
    fn enqueue(&mut self, id: AgentId) {
        if !self.ready.contains(&id) {
            self.ready.push_back(id);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::agent::base_agent::AgentCommandError;

    // -- A scripted fake agent (no LLM/memory) to exercise the registry --------

    /// The kind every test child is spawned as.
    const TEST_KIND: &str = "worker";

    /// Shared, inspectable record of `(agent, command)` pairs every fake agent
    /// received via `send_command`.
    type CommandLog = Arc<Mutex<Vec<(AgentId, AgentCommand)>>>;
    /// Shared, inspectable record of `(child, text, ok)` results delivered to
    /// parents.
    type DeliveredLog = Arc<Mutex<Vec<(AgentId, String, bool)>>>;

    /// One scripted action a [`FakeAgent`] performs on a `tick`.
    enum Step {
        /// Return `Working` (more to do).
        Work,
        /// Call the injected spawner with `goal`, then return `Working`.
        Spawn(String),
        /// Return `AwaitingApproval { id: approval-1, summary }`.
        AwaitApproval(String),
        /// Call the injected approval responder for `target`, then return
        /// `Working` (models a root resolving a subagent's request).
        Respond(AgentId, ApprovalVerdict, Option<String>),
        /// Return `Yielded { text }`.
        Yield(String),
    }

    /// A test agent that replays scripted [`Step`]s, records every command it
    /// receives, and records delivered child results — all into shared vecs.
    struct FakeAgent {
        id: AgentId,
        steps: VecDeque<Step>,
        spawner: Option<Arc<dyn Spawner>>,
        responder: Option<Arc<dyn ApprovalResponder>>,
        delivered: DeliveredLog,
        received: CommandLog,
    }

    impl FakeAgent {
        /// A minimally-wired agent that only replays `steps`.
        fn scripted(id: AgentId, steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                id,
                steps: steps.into_iter().collect(),
                spawner: None,
                responder: None,
                delivered: Arc::new(Mutex::new(Vec::new())),
                received: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Agent for FakeAgent {
        fn id(&self) -> AgentId {
            self.id
        }

        fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError> {
            self.received.lock().unwrap().push((self.id, command));
            Ok(())
        }

        fn deliver_child_result(&mut self, child: AgentId, text: String, ok: bool) {
            self.delivered.lock().unwrap().push((child, text, ok));
        }

        fn tick(&mut self) -> TickOutcome {
            match self.steps.pop_front() {
                Some(Step::Work) => TickOutcome::Working,
                Some(Step::Spawn(goal)) => {
                    if let Some(spawner) = &self.spawner {
                        spawner.spawn(self.id, AgentKind::new(TEST_KIND), goal);
                    }
                    TickOutcome::Working
                }
                Some(Step::AwaitApproval(summary)) => TickOutcome::AwaitingApproval {
                    id: ApprovalId(1),
                    summary,
                },
                Some(Step::Respond(target, verdict, note)) => {
                    if let Some(responder) = &self.responder {
                        responder.respond(target, verdict, note);
                    }
                    TickOutcome::Working
                }
                Some(Step::Yield(text)) => TickOutcome::Yielded { text },
                None => TickOutcome::Idle,
            }
        }
    }

    /// A factory whose children yield `done:<goal>`. Goal prefixes drive behavior:
    /// `deep:` first spawns one grandchild (subagent->subagent spawning);
    /// `approve:` first parks on an approval (subagent approval bubbling).
    struct FakeFactory {
        delivered: DeliveredLog,
        received: CommandLog,
    }

    impl AgentFactory for FakeFactory {
        fn create_agent(
            &self,
            id: AgentId,
            _kind: &AgentKind,
            goal: String,
            spawner: Arc<dyn Spawner>,
            _approval_responder: Option<Arc<dyn ApprovalResponder>>,
        ) -> Result<Box<dyn Agent>, String> {
            let mut steps = VecDeque::new();
            if let Some(rest) = goal.strip_prefix("deep:") {
                steps.push_back(Step::Spawn(rest.to_string()));
            }
            if let Some(rest) = goal.strip_prefix("approve:") {
                steps.push_back(Step::AwaitApproval(rest.to_string()));
            }
            steps.push_back(Step::Yield(format!("done:{goal}")));
            Ok(Box::new(FakeAgent {
                id,
                steps,
                spawner: Some(spawner),
                responder: None,
                delivered: Arc::clone(&self.delivered),
                received: Arc::clone(&self.received),
            }))
        }
    }

    /// Build a registry over a fake factory, returning the shared delivered-result
    /// and received-command logs for inspection.
    fn registry_with_factory() -> (AgentRegistry, DeliveredLog, CommandLog) {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(FakeFactory {
            delivered: Arc::clone(&delivered),
            received: Arc::clone(&received),
        });
        (AgentRegistry::new(factory), delivered, received)
    }

    #[test]
    fn root_spawns_child_and_receives_its_result() {
        let (mut registry, _factory_delivered, _received) = registry_with_factory();
        let root_id = registry.alloc_id();
        let root_delivered = Arc::new(Mutex::new(Vec::new()));
        let root = FakeAgent {
            spawner: Some(registry.spawner()),
            delivered: Arc::clone(&root_delivered),
            ..FakeAgent::scripted(
                root_id,
                [
                    Step::Spawn("subtask".into()),
                    Step::Work,
                    Step::Yield("final answer".into()),
                ],
            )
        };
        registry.add_root(Box::new(root), AgentKind::new("conversation"), SessionId(1));

        let output = registry.run_until_idle();

        // The root produced exactly one user-facing reply and no approvals.
        assert_eq!(
            output.replies,
            vec![RootReply {
                session: SessionId(1),
                text: "final answer".into(),
                ended: false,
            }]
        );
        assert!(output.approvals.is_empty());
        // The child's result bubbled back to the root.
        let delivered = root_delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].1, "done:subtask");
        assert!(delivered[0].2);
        // The one-shot child was removed; only the root remains.
        assert_eq!(registry.agent_count(), 1);
    }

    #[test]
    fn subagent_can_itself_spawn_a_grandchild() {
        let (mut registry, factory_delivered, _received) = registry_with_factory();
        let root_id = registry.alloc_id();
        let root_delivered = Arc::new(Mutex::new(Vec::new()));
        let root = FakeAgent {
            spawner: Some(registry.spawner()),
            delivered: Arc::clone(&root_delivered),
            // Spawn a child that will itself spawn a grandchild ("deep:").
            ..FakeAgent::scripted(
                root_id,
                [Step::Spawn("deep:leaf".into()), Step::Yield("ok".into())],
            )
        };
        registry.add_root(Box::new(root), AgentKind::new("conversation"), SessionId(2));

        let output = registry.run_until_idle();

        assert_eq!(output.replies.len(), 1);
        assert_eq!(output.replies[0].session, SessionId(2));
        // The grandchild's result was delivered to the child (recorded in the
        // factory-shared sink); the child's result was delivered to the root.
        assert!(
            factory_delivered
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text, _)| text == "done:leaf"),
            "grandchild result should reach its parent (the child)"
        );
        assert!(
            root_delivered
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text, _)| text == "done:deep:leaf"),
            "child result should reach the root"
        );
        // All subagents are one-shot and removed; only the root remains.
        assert_eq!(registry.agent_count(), 1);
    }

    #[test]
    fn root_approval_is_surfaced_then_resolved_resumes_the_agent() {
        let (mut registry, _delivered, _received) = registry_with_factory();
        let root_id = registry.alloc_id();
        let root = FakeAgent::scripted(
            root_id,
            [
                Step::AwaitApproval("delete prod?".into()),
                Step::Yield("done".into()),
            ],
        );
        registry.add_root(Box::new(root), AgentKind::new("conversation"), SessionId(3));

        // First drive parks on the approval: no reply, one surfaced request.
        let output = registry.run_until_idle();
        assert!(output.replies.is_empty());
        assert_eq!(
            output.approvals,
            vec![ApprovalRequest {
                session: SessionId(3),
                agent: root_id,
                approval: ApprovalId(1),
                summary: "delete prod?".into(),
            }]
        );

        // Resolving re-enqueues the agent; the next drive produces the reply.
        registry
            .resolve_approval(root_id, ApprovalId(1), ApprovalDecision::Approved)
            .expect("resolve approval");
        let output = registry.run_until_idle();
        assert_eq!(output.replies.len(), 1);
        assert_eq!(output.replies[0].text, "done");
    }

    #[test]
    fn resolve_approval_for_unknown_agent_errors() {
        let (mut registry, _delivered, _received) = registry_with_factory();
        let result =
            registry.resolve_approval(AgentId(999), ApprovalId(1), ApprovalDecision::Approved);
        assert!(matches!(result, Err(ResolveApprovalError::UnknownAgent(_))));
    }

    #[test]
    fn spawner_allocates_unique_ascending_ids() {
        let (registry, _delivered, _received) = registry_with_factory();
        let spawner = registry.spawner();
        let a = spawner.spawn(AgentId(1), AgentKind::new(TEST_KIND), "x".into());
        let b = spawner.spawn(AgentId(1), AgentKind::new(TEST_KIND), "y".into());
        let c = registry.alloc_id();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert!(a.0 < b.0 && b.0 < c.0);
    }

    /// Build a root that lets the subagent park, then resolves it with `verdict`.
    /// The child is spawned directly so its id is deterministic (`agent-2`).
    fn drive_subagent_approval(
        verdict: ApprovalVerdict,
        note: Option<String>,
    ) -> (CommandLog, DriveOutput) {
        let (mut registry, _delivered, received) = registry_with_factory();
        let root_id = registry.alloc_id();
        let root = FakeAgent {
            spawner: Some(registry.spawner()),
            responder: Some(registry.approval_responder()),
            received: Arc::clone(&received),
            ..FakeAgent::scripted(
                root_id,
                [
                    // One Work tick lets the child park and bubble its request first.
                    Step::Work,
                    Step::Respond(AgentId(2), verdict, note),
                    Step::Yield("root-done".into()),
                ],
            )
        };
        registry.add_root(Box::new(root), AgentKind::new("conversation"), SessionId(5));

        let child_id = registry.spawner().spawn(
            root_id,
            AgentKind::new(TEST_KIND),
            "approve:delete prod".into(),
        );
        assert_eq!(
            child_id,
            AgentId(2),
            "child id must be deterministic for the script"
        );

        let output = registry.run_until_idle();
        (received, output)
    }

    #[test]
    fn subagent_approval_bubbles_to_root_not_to_the_orchestrator() {
        let (received, output) = drive_subagent_approval(ApprovalVerdict::Yes, None);

        // The subagent's request was *not* surfaced to the orchestrator...
        assert!(
            output.approvals.is_empty(),
            "a subagent approval must bubble to the root, not surface"
        );
        // ...it was delivered to the root as a provenance-tagged message.
        let received = received.lock().unwrap();
        assert!(
            received.iter().any(|(agent, command)| *agent == AgentId(1)
                && *command
                    == AgentCommand::AppendMessage(
                        "[approval request from agent-2] delete prod".into()
                    )),
            "root should receive the bubbled approval request: {received:?}"
        );
        // The root still produced its own reply afterwards.
        assert_eq!(output.replies.len(), 1);
        assert_eq!(output.replies[0].text, "root-done");
    }

    #[test]
    fn root_yes_verdict_approves_the_waiting_subagent() {
        let (received, _output) = drive_subagent_approval(ApprovalVerdict::Yes, None);
        let received = received.lock().unwrap();
        assert!(
            received.iter().any(|(agent, command)| *agent == AgentId(2)
                && *command
                    == AgentCommand::ApprovalResult {
                        id: ApprovalId(1),
                        decision: ApprovalDecision::Approved,
                    }),
            "subagent should be approved: {received:?}"
        );
    }

    #[test]
    fn root_no_and_other_verdicts_reject_with_the_user_note() {
        for verdict in [ApprovalVerdict::No, ApprovalVerdict::Other] {
            let (received, _output) = drive_subagent_approval(verdict, Some("not allowed".into()));
            let received = received.lock().unwrap();
            assert!(
                received.iter().any(|(agent, command)| *agent == AgentId(2)
                    && *command
                        == AgentCommand::ApprovalResult {
                            id: ApprovalId(1),
                            decision: ApprovalDecision::Rejected("not allowed".into()),
                        }),
                "verdict {verdict:?} should reject with the note: {received:?}"
            );
        }
    }
}
