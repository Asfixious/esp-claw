//! Per-session agent runtime.
//!
//! Each session owns one [`OrchestratorInstance`]: an independent
//! [`AgentRegistry`] (its own flattened agent graph) plus the id of its root
//! agent. Sessions are isolated — one session's agents never appear in another's
//! registry — while a single global id counter (shared at construction) keeps
//! every [`AgentId`] unique across the whole process.
//!
//! The root is built lazily on the first delivered message: that message is the
//! root's goal. Subsequent messages are appended to the existing root.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use crate::agent::base_agent::{AgentId, ApprovalDecision, ApprovalId};
use crate::agent::registry::{AgentFactory, AgentRegistry, DriveOutput, ResolveApprovalError};
use crate::agent::AgentKind;
use crate::session::SessionId;

/// The kind instantiated as a session's user-facing root agent.
const ROOT_AGENT_KIND: &str = "conversation";

/// One session's isolated agent graph and its root.
pub(crate) struct OrchestratorInstance {
    session: SessionId,
    registry: AgentRegistry,
    /// The root agent's id, set when the first message builds it.
    root: Option<AgentId>,
}

impl OrchestratorInstance {
    /// Create an empty instance for `session`. Agents are built with `factory`
    /// and draw ids from the shared `next_id` counter so they stay unique across
    /// every session.
    pub(crate) fn new(
        session: SessionId,
        factory: Arc<dyn AgentFactory>,
        next_id: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            session,
            registry: AgentRegistry::with_id_allocator(factory, next_id),
            root: None,
        }
    }

    /// Deliver a user message to this session's root.
    ///
    /// Builds the root on the first call (the message becomes its goal); appends
    /// to the existing root afterwards. The agent is left ready; call
    /// [`drive`](Self::drive) to run it.
    ///
    /// # Errors
    ///
    /// Propagates the factory's error string when the root cannot be built.
    pub(crate) fn deliver(&mut self, text: impl Into<String>) -> Result<(), String> {
        match self.root {
            Some(root) => {
                self.registry.deliver_to(root, text);
                Ok(())
            }
            None => {
                let root = self.registry.add_root_of_kind(
                    AgentKind::new(ROOT_AGENT_KIND),
                    self.session,
                    text.into(),
                )?;
                self.root = Some(root);
                Ok(())
            }
        }
    }

    /// Drive this session's agents until none is ready, returning the replies and
    /// any pending approvals surfaced for a human decision.
    pub(crate) fn drive(&mut self) -> DriveOutput {
        self.registry.run_until_idle()
    }

    /// A read-only transcript of this session's root agent (its messages and tool
    /// calls), or `None` before the first message builds the root. Subagents are
    /// removed once they finish, so only the root's own calls are visible here.
    pub(crate) fn root_transcript(&self) -> Option<serde_json::Value> {
        self.root
            .and_then(|root| self.registry.agent_transcript(root))
    }

    /// Route a human decision back to the agent waiting on `approval`.
    ///
    /// Leaves the agent ready; call [`drive`](Self::drive) afterwards to resume it.
    ///
    /// # Errors
    ///
    /// Propagates [`ResolveApprovalError`] when the agent is gone or is not
    /// awaiting this approval.
    pub(crate) fn resolve_approval(
        &mut self,
        agent: AgentId,
        approval: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ResolveApprovalError> {
        self.registry.resolve_approval(agent, approval, decision)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::agent::base_agent::{AgentCommand, AgentCommandError, TickOutcome};
    use crate::agent::{Agent, ApprovalResponder, Spawner};

    /// Echoes each delivered message back as `echo:<message>` on the next tick.
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

    /// A factory that never builds an agent — to exercise the error path.
    struct FailFactory;

    impl AgentFactory for FailFactory {
        fn create_agent(
            &self,
            _id: AgentId,
            _kind: &AgentKind,
            _goal: String,
            _spawner: Arc<dyn Spawner>,
            _approval_responder: Option<Arc<dyn ApprovalResponder>>,
        ) -> Result<Box<dyn Agent>, String> {
            Err("factory refused".into())
        }
    }

    /// Parks on an approval the first tick, then yields once the decision lands.
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
                    text: "approved".into(),
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

    fn instance(session: SessionId, factory: Arc<dyn AgentFactory>) -> OrchestratorInstance {
        OrchestratorInstance::new(session, factory, Arc::new(AtomicUsize::new(1)))
    }

    #[test]
    fn first_message_builds_root_and_echoes_the_goal() {
        let mut instance = instance(SessionId(1), Arc::new(EchoFactory));
        assert_eq!(instance.root, None);

        instance.deliver("hi").unwrap();
        assert!(instance.root.is_some());

        let output = instance.drive();
        assert_eq!(output.replies.len(), 1);
        assert_eq!(output.replies[0].text, "echo:hi");
        assert_eq!(output.replies[0].session, SessionId(1));
    }

    #[test]
    fn the_same_root_is_reused_across_messages() {
        let mut instance = instance(SessionId(1), Arc::new(EchoFactory));

        instance.deliver("first").unwrap();
        let root = instance.root;
        let _ = instance.drive();

        instance.deliver("second").unwrap();
        assert_eq!(instance.root, root, "root must be reused, not rebuilt");

        let output = instance.drive();
        assert_eq!(output.replies[0].text, "echo:second");
    }

    #[test]
    fn a_failed_root_build_propagates_and_leaves_no_root() {
        let mut instance = instance(SessionId(1), Arc::new(FailFactory));

        let result = instance.deliver("hi");
        assert_eq!(result, Err("factory refused".to_string()));
        assert_eq!(instance.root, None, "a failed build must not set a root");
    }

    #[test]
    fn an_approval_is_surfaced_then_resolved_resumes_the_root() {
        let mut instance = instance(SessionId(7), Arc::new(ApprovalFactory));

        instance.deliver("do it").unwrap();
        let output = instance.drive();
        assert!(output.replies.is_empty());
        assert_eq!(output.approvals.len(), 1);
        let approval = &output.approvals[0];
        assert_eq!(Some(approval.agent), instance.root);
        assert_eq!(approval.session, SessionId(7));

        instance
            .resolve_approval(
                approval.agent,
                approval.approval,
                ApprovalDecision::Approved,
            )
            .unwrap();
        let resumed = instance.drive();
        assert_eq!(resumed.replies.len(), 1);
        assert_eq!(resumed.replies[0].text, "approved");
    }

    #[test]
    fn instances_sharing_a_counter_allocate_distinct_root_ids() {
        let factory: Arc<dyn AgentFactory> = Arc::new(EchoFactory);
        let counter = Arc::new(AtomicUsize::new(1));
        let mut a =
            OrchestratorInstance::new(SessionId(1), Arc::clone(&factory), Arc::clone(&counter));
        let mut b = OrchestratorInstance::new(SessionId(2), factory, counter);

        a.deliver("a").unwrap();
        b.deliver("b").unwrap();

        assert_ne!(
            a.root, b.root,
            "isolated sessions must still draw unique ids from the shared counter"
        );
    }
}
