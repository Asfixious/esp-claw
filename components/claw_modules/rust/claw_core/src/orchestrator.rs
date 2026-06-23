//! Layer 1 orchestrator: session registry, ingress, egress reply routing.
//!
//! Inbound logic lives in [`Orchestrator::on_user_message`] and
//! [`Orchestrator::on_command`].

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use crate::agent::base_agent::{AgentId, ApprovalDecision, ApprovalId};
use crate::agent::registry::{AgentFactory, DriveOutput, ResolveApprovalError};
use crate::channels::{
    ChannelEgress, ChannelEgressHub, ChannelIngressSink, InboundCommand, InboundMessage,
};
use crate::orchestrator_instance::OrchestratorInstance;
use crate::protocol::Command;
use crate::session::{
    DeliverError, SessionError, SessionId, SessionMessage, SessionOut, SessionRoutes, SessionStore,
};

/// The first [`crate::agent::AgentId`] handed out (0 reads like "unset").
const FIRST_AGENT_ID: usize = 1;

/// Failure delivering a human approval decision via
/// [`Orchestrator::resolve_approval`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum ApprovalError {
    /// No live agent graph for the session (never started, or already deleted).
    #[error("session {0} has no live agent graph")]
    UnknownSession(SessionId),
    /// The agent rejected the decision (gone, or not awaiting this approval).
    #[error(transparent)]
    Resolve(#[from] ResolveApprovalError),
}

pub struct Orchestrator {
    egress: Arc<dyn ChannelEgress>,
    /// Builds agents for every session's registry. Required at construction
    /// (enforced by the builder typestate): the orchestrator owns no LLM client
    /// of its own — the factory holds whatever an agent needs to run.
    factory: Arc<dyn AgentFactory>,
    /// Global agent-id counter shared by every per-session registry so ids are
    /// unique across the whole process, not merely within one session.
    next_agent_id: Arc<AtomicUsize>,
    /// One isolated agent graph per session. Each instance is behind its own
    /// lock so driving one session does not serialize the others; the outer lock
    /// guards only insert/lookup/remove on the map.
    instances: Mutex<HashMap<SessionId, Arc<Mutex<OrchestratorInstance>>>>,
    sessions: SessionStore,
    routes: SessionRoutes,
}

impl Orchestrator {
    // -----------------------------------------------------------------------
    // Inbound callbacks — edit these
    // -----------------------------------------------------------------------

    fn on_user_message(&self, session_id: SessionId, msg: &SessionMessage) {
        // Take only the session's own lock for the deliver+drive; the global map
        // lock is released first so a slow LLM call in one session does not block
        // ingress/drive for the others (see `instance_for`).
        let instance = self.instance_for(session_id);
        let output = {
            let mut instance = instance.lock().unwrap_or_else(|poison| poison.into_inner());
            if let Err(error) = instance.deliver(msg.text.clone()) {
                log::warn!("failed to build/deliver root for {session_id}: {error}");
                return;
            }
            instance.drive()
        };

        self.surface_output(output);
    }

    /// Get the session's agent graph, creating it on first use.
    ///
    /// Holds the global `instances` map lock only long enough to look up or insert
    /// the per-session [`OrchestratorInstance`], then hands back a clone of its
    /// `Arc<Mutex<_>>` so callers lock the session — not the whole map — while
    /// driving it.
    fn instance_for(&self, session_id: SessionId) -> Arc<Mutex<OrchestratorInstance>> {
        let mut instances = self
            .instances
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Arc::clone(instances.entry(session_id).or_insert_with(|| {
            Arc::new(Mutex::new(OrchestratorInstance::new(
                session_id,
                Arc::clone(&self.factory),
                Arc::clone(&self.next_agent_id),
            )))
        }))
    }

    /// Route a [`DriveOutput`] to the session's egress: replies as messages, and
    /// each pending approval as a visible message tagged with its agent/approval
    /// id (so a human can answer it via [`resolve_approval`](Self::resolve_approval)).
    fn surface_output(&self, output: DriveOutput) {
        for reply in output.replies {
            if let Err(error) = self.send_message(reply.session, reply.text) {
                log::warn!("failed to send reply for {}: {error}", reply.session);
            }
        }
        for approval in output.approvals {
            // There is no inbound approval-response binding yet; surface the
            // request so it is visible and resolvable out of band rather than
            // dropped. The id tags let a caller target the exact agent.
            log::info!(
                "approval requested: session={} agent={} approval={}",
                approval.session,
                approval.agent,
                approval.approval
            );
            let text = format!(
                "[approval needed · {} · {}] {}",
                approval.agent, approval.approval, approval.summary
            );
            if let Err(error) = self.send_message(approval.session, text) {
                log::warn!(
                    "failed to surface approval for {}: {error}",
                    approval.session
                );
            }
        }
    }

    /// Deliver a human decision for a pending approval and resume the session.
    ///
    /// Routes the decision to the exact agent within `session`, drives the
    /// session's graph, and surfaces any resulting replies/approvals.
    ///
    /// # Errors
    ///
    /// [`ApprovalError::UnknownSession`] when the session has no live agent graph,
    /// or [`ApprovalError::Resolve`] when the agent is gone or not awaiting this
    /// approval.
    pub fn resolve_approval(
        &self,
        session_id: SessionId,
        agent: AgentId,
        approval: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ApprovalError> {
        let instance = {
            let instances = self
                .instances
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            instances
                .get(&session_id)
                .map(Arc::clone)
                .ok_or(ApprovalError::UnknownSession(session_id))?
        };
        let output = {
            let mut instance = instance.lock().unwrap_or_else(|poison| poison.into_inner());
            instance.resolve_approval(agent, approval, decision)?;
            instance.drive()
        };
        self.surface_output(output);
        Ok(())
    }

    /// A read-only snapshot of `session_id`'s root-agent transcript — its messages
    /// and `tool_calls` — for inspection tools / CLIs. `None` if the session has no
    /// live root yet. Subagents are ephemeral, so only the root's own calls show.
    pub fn root_transcript(&self, session_id: SessionId) -> Option<serde_json::Value> {
        let instance = {
            let instances = self
                .instances
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            instances.get(&session_id).map(Arc::clone)?
        };
        let instance = instance.lock().unwrap_or_else(|poison| poison.into_inner());
        instance.root_transcript()
    }

    fn on_command(&self, session_id: SessionId, cmd: &Command) {
        // Not wired yet (intentionally a no-op, not a panic): the `Command`
        // protocol still uses the legacy task/run model and must be reconciled
        // with the session/agent/approval model before it can drive the graph.
        // Until then, acknowledge in the log and drop rather than aborting.
        log::warn!("inbound command for {session_id} ignored (not implemented yet): {cmd:?}");
    }

    fn send_message(
        &self,
        session_id: SessionId,
        text: impl Into<String>,
    ) -> Result<(), DeliverError> {
        let reply_route = self
            .routes
            .get(session_id)
            .ok_or(DeliverError::NoReplyRoute(session_id))?;
        SessionOut::new(self.egress.as_ref(), &reply_route)
            .send_message(text)
            .map_err(Into::into)
    }

    fn deliver_user_message(&self, msg: InboundMessage) -> Result<(), DeliverError> {
        if msg.session_id.trim().is_empty() {
            return Err(DeliverError::MissingSessionId);
        }
        let session_id = SessionId::from_wire(&msg.session_id)
            .map_err(|_| DeliverError::InvalidSessionId(msg.session_id.clone()))?;
        if !self.sessions.contains(session_id) {
            return Err(DeliverError::SessionNotFound(session_id));
        }

        self.routes.update_from_inbound(session_id, &msg);
        let session_msg = SessionMessage::from_inbound(&msg);
        self.on_user_message(session_id, &session_msg);
        Ok(())
    }

    fn deliver_command(&self, inbound: InboundCommand) -> Result<(), DeliverError> {
        let session_id = inbound.session_id;
        if !self.sessions.contains(session_id) {
            return Err(DeliverError::SessionNotFound(session_id));
        }
        if self.routes.get(session_id).is_none() {
            return Err(DeliverError::NoReplyRoute(session_id));
        }
        self.on_command(session_id, &inbound.command);
        Ok(())
    }

    pub fn session_create(&self) -> SessionId {
        self.sessions.create().id
    }

    pub fn session_delete(&self, session_id: SessionId) -> Result<(), SessionError> {
        self.sessions.delete(session_id)?;
        self.routes.remove(session_id);
        // Drop the session's agent graph so a deleted session leaves no live
        // agents behind.
        self.instances
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&session_id);
        Ok(())
    }

    pub fn builder() -> OrchestratorBuilder<ChannelsUnset, FactoryUnset> {
        OrchestratorBuilder {
            channels: ChannelsUnset,
            factory: FactoryUnset,
        }
    }
}

impl ChannelIngressSink for Orchestrator {
    fn push_user_message(&self, msg: InboundMessage) {
        if let Err(err) = self.deliver_user_message(msg) {
            log::warn!("ingress user message deliver failed: {err}");
        }
    }

    fn push_command(&self, command: InboundCommand) {
        if let Err(err) = self.deliver_command(command) {
            log::warn!("ingress command deliver failed: {err}");
        }
    }
}

impl ChannelIngressSink for Arc<Orchestrator> {
    fn push_user_message(&self, msg: InboundMessage) {
        (**self).push_user_message(msg);
    }

    fn push_command(&self, command: InboundCommand) {
        (**self).push_command(command);
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

pub struct OrchestratorBuilder<Channels, Factory> {
    channels: Channels,
    factory: Factory,
}

impl<Channels> OrchestratorBuilder<Channels, FactoryUnset> {
    /// Inject the [`AgentFactory`] used to build every session's agents.
    ///
    /// Required: [`build`](OrchestratorBuilder::build) is only callable once a
    /// factory is set, so the orchestrator can always materialize a root agent
    /// for a live session.
    pub fn with_agent_factory(
        self,
        factory: Arc<dyn AgentFactory>,
    ) -> OrchestratorBuilder<Channels, FactorySet> {
        OrchestratorBuilder {
            channels: self.channels,
            factory: FactorySet { factory },
        }
    }
}

pub struct ChannelsUnset;
pub struct ChannelsBuiltin;
pub struct ChannelsEgressOnly {
    egress: Arc<dyn ChannelEgress>,
}
pub struct FactoryUnset;
pub struct FactorySet {
    factory: Arc<dyn AgentFactory>,
}

mod sealed {
    use super::*;

    pub trait SealedChannelsReady {
        fn materialize(self) -> Arc<dyn ChannelEgress>;
    }

    impl SealedChannelsReady for ChannelsBuiltin {
        fn materialize(self) -> Arc<dyn ChannelEgress> {
            Arc::new(ChannelEgressHub::new())
        }
    }

    impl SealedChannelsReady for ChannelsEgressOnly {
        fn materialize(self) -> Arc<dyn ChannelEgress> {
            self.egress
        }
    }
}

trait ChannelsReady: sealed::SealedChannelsReady {}
impl ChannelsReady for ChannelsBuiltin {}
impl ChannelsReady for ChannelsEgressOnly {}

impl<Factory> OrchestratorBuilder<ChannelsUnset, Factory> {
    pub fn with_builtin_channels(self) -> OrchestratorBuilder<ChannelsBuiltin, Factory> {
        OrchestratorBuilder {
            channels: ChannelsBuiltin,
            factory: self.factory,
        }
    }

    pub fn config_egress(
        self,
        egress: Arc<dyn ChannelEgress>,
    ) -> OrchestratorBuilder<ChannelsEgressOnly, Factory> {
        OrchestratorBuilder {
            channels: ChannelsEgressOnly { egress },
            factory: self.factory,
        }
    }
}

impl<C> OrchestratorBuilder<C, FactorySet>
where
    C: ChannelsReady,
{
    pub fn build(self) -> Arc<Orchestrator> {
        Arc::new(Orchestrator {
            egress: self.channels.materialize(),
            factory: self.factory.factory,
            next_agent_id: Arc::new(AtomicUsize::new(FIRST_AGENT_ID)),
            instances: Mutex::new(HashMap::new()),
            sessions: SessionStore::new(),
            routes: SessionRoutes::new(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::agent::base_agent::{AgentCommand, AgentCommandError, AgentId, TickOutcome};
    use crate::agent::registry::AgentFactory;
    use crate::agent::{Agent, AgentKind, ApprovalResponder, Spawner};
    use crate::channels::{ChannelTransport, RecordingTransport};

    // -- A fake factory + agent that echoes each delivered message --------------

    /// A scripted agent: every message it receives is echoed back as
    /// `echo:<message>` on the next tick, then it goes idle.
    struct EchoAgent {
        id: AgentId,
        pending: VecDeque<String>,
    }

    impl Agent for EchoAgent {
        fn id(&self) -> AgentId {
            self.id
        }

        fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError> {
            if let AgentCommand::AppendMessage(message) = command {
                self.pending.push_back(message);
            }
            Ok(())
        }

        fn deliver_child_result(&mut self, _child: AgentId, _text: String, _ok: bool) {}

        fn tick(&mut self) -> TickOutcome {
            match self.pending.pop_front() {
                Some(message) => TickOutcome::Yielded {
                    text: format!("echo:{message}"),
                },
                None => TickOutcome::Idle,
            }
        }
    }

    /// Builds [`EchoAgent`]s seeded with the goal as their first message.
    struct EchoFactory;

    impl AgentFactory for EchoFactory {
        fn create_agent(
            &self,
            id: AgentId,
            _kind: &AgentKind,
            goal: String,
            _spawner: Arc<dyn Spawner>,
            _approval_responder: Option<Arc<dyn ApprovalResponder>>,
        ) -> Result<Box<dyn Agent>, String> {
            Ok(Box::new(EchoAgent {
                id,
                pending: VecDeque::from([goal]),
            }))
        }
    }

    /// An agent that parks on an approval the first time it ticks, then yields
    /// once the decision arrives — to exercise the approval round-trip.
    struct ApprovalAgent {
        id: AgentId,
        asked: bool,
        approved: bool,
        done: bool,
    }

    impl Agent for ApprovalAgent {
        fn id(&self) -> AgentId {
            self.id
        }

        fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError> {
            if matches!(command, AgentCommand::ApprovalResult { .. }) {
                self.approved = true;
            }
            Ok(())
        }

        fn deliver_child_result(&mut self, _child: AgentId, _text: String, _ok: bool) {}

        fn tick(&mut self) -> TickOutcome {
            if !self.asked {
                self.asked = true;
                return TickOutcome::AwaitingApproval {
                    id: ApprovalId(1),
                    summary: "ok?".into(),
                };
            }
            if self.approved && !self.done {
                self.done = true;
                return TickOutcome::Yielded {
                    text: "approved-done".into(),
                };
            }
            TickOutcome::Idle
        }
    }

    struct ApprovalFactory;

    impl AgentFactory for ApprovalFactory {
        fn create_agent(
            &self,
            id: AgentId,
            _kind: &AgentKind,
            _goal: String,
            _spawner: Arc<dyn Spawner>,
            _approval_responder: Option<Arc<dyn ApprovalResponder>>,
        ) -> Result<Box<dyn Agent>, String> {
            Ok(Box::new(ApprovalAgent {
                id,
                asked: false,
                approved: false,
                done: false,
            }))
        }
    }

    fn user_msg(session_id: SessionId, text: &str) -> InboundMessage {
        InboundMessage {
            message_id: "m1".into(),
            channel: "qq".into(),
            chat_id: "chat-a".into(),
            sender_id: None,
            session_id: session_id.to_wire(),
            text: text.into(),
        }
    }

    fn orchestrator_with_factory(
        factory: Arc<dyn AgentFactory>,
    ) -> (Arc<Orchestrator>, Arc<RecordingTransport>) {
        let transport = RecordingTransport::new("qq");
        let egress = Arc::new(ChannelEgressHub::new());
        let as_transport: Arc<dyn ChannelTransport> = Arc::clone(&transport) as Arc<_>;
        egress.register(as_transport);

        let orch = Orchestrator::builder()
            .config_egress(egress as Arc<dyn ChannelEgress>)
            .with_agent_factory(factory)
            .build();
        (orch, transport)
    }

    #[test]
    fn user_message_drives_root_and_replies() {
        let (orch, transport) = orchestrator_with_factory(Arc::new(EchoFactory));
        let session = orch.session_create();

        orch.push_user_message(user_msg(session, "hi"));

        let sent = transport.drain_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "echo:hi");
    }

    #[test]
    fn second_message_reuses_the_same_session_root() {
        let (orch, transport) = orchestrator_with_factory(Arc::new(EchoFactory));
        let session = orch.session_create();

        orch.push_user_message(user_msg(session, "first"));
        orch.push_user_message(user_msg(session, "second"));

        let sent = transport.drain_sent();
        assert_eq!(
            sent.iter().map(|m| m.text.clone()).collect::<Vec<_>>(),
            vec!["echo:first".to_string(), "echo:second".to_string()]
        );
        // Exactly one instance was created for the session.
        assert_eq!(orch.instances.lock().unwrap().len(), 1);
    }

    #[test]
    fn approval_is_surfaced_then_resolved_resumes_the_session() {
        let (orch, transport) = orchestrator_with_factory(Arc::new(ApprovalFactory));
        let session = orch.session_create();

        // First message parks the root on an approval, surfaced as a message.
        orch.push_user_message(user_msg(session, "do it"));
        let surfaced = transport.drain_sent();
        assert_eq!(surfaced.len(), 1);
        assert!(
            surfaced[0].text.contains("[approval needed"),
            "expected an approval surface, got: {}",
            surfaced[0].text
        );

        // The root is the first agent allocated (id 1). Resolving resumes it.
        orch.resolve_approval(
            session,
            AgentId(1),
            ApprovalId(1),
            ApprovalDecision::Approved,
        )
        .expect("resolve approval");
        let resumed = transport.drain_sent();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].text, "approved-done");
    }

    #[test]
    fn resolve_approval_for_unknown_session_errors() {
        let (orch, _transport) = orchestrator_with_factory(Arc::new(ApprovalFactory));
        let result = orch.resolve_approval(
            SessionId(42),
            AgentId(1),
            ApprovalId(1),
            ApprovalDecision::Approved,
        );
        assert!(matches!(result, Err(ApprovalError::UnknownSession(_))));
    }

    #[test]
    fn two_sessions_have_independent_graphs() {
        let (orch, transport) = orchestrator_with_factory(Arc::new(EchoFactory));
        let s1 = orch.session_create();
        let s2 = orch.session_create();

        orch.push_user_message(user_msg(s1, "one"));
        orch.push_user_message(user_msg(s2, "two"));

        // One isolated instance per session.
        assert_eq!(orch.instances.lock().unwrap().len(), 2);
        let texts: Vec<String> = transport.drain_sent().into_iter().map(|m| m.text).collect();
        assert!(texts.contains(&"echo:one".to_string()));
        assert!(texts.contains(&"echo:two".to_string()));
    }

    #[test]
    fn builds_with_builtin_channels_and_factory() {
        let _ = Orchestrator::builder()
            .with_builtin_channels()
            .with_agent_factory(Arc::new(EchoFactory))
            .build();
    }
}
