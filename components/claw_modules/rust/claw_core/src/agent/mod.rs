//! Agents driven by the orchestrator.
//!
//! [`BaseAgent`] is the command-in / outcome-out base layer. There is exactly one
//! semantic agent on top of it — [`GenericAgent`] — a flat ReAct loop with no
//! built-in FSM. What distinguishes one agent "kind" from another is pure data:
//! its [`AgentConfig`] (system prompt, tool set, skills, spawning), loaded at
//! compile time from `resources/agents/<kind>/` and resolved through an
//! [`AgentResolver`] — see the crate-internal `manifest` module.
//! [`GenericAgent`] implements
//! the unified [`Agent`] trait the scheduler drives; the trait never reaches into
//! internals.
//!
//! Spawning is a model-callable `spawn_subagent(kind, goal)` tool (in
//! [`internal_tools`]) backed by an injected [`Spawner`]; the [`AgentRegistry`]
//! owns the flattened agent graph and materializes children through an
//! [`AgentFactory`].

#[allow(clippy::module_inception)]
pub mod agent;
pub mod base_agent;
pub mod config;
mod fs_factory;
mod internal_tools;
pub mod manifest;
mod map_resolver;
pub mod registry;

pub use agent::{AgentKind, GenericAgent, GenericAgentBuildError};
pub use base_agent::{
    AgentAbortHandle, AgentCommand, AgentCommandError, AgentId, AgentRunError, AgentState,
    ApprovalDecision, ApprovalId, BaseAgent, BaseAgentBuildError, BaseAgentBuilder, CancelReason,
    TickOutcome,
};
pub use config::{AgentConfig, AgentConfigError, AgentResolver};
pub use fs_factory::FsAgentFactory;
pub use internal_tools::{ApprovalResponder, ApprovalVerdict, Spawner};
pub use map_resolver::MapAgentResolver;
pub use registry::{
    AgentFactory, AgentRegistry, ApprovalRequest, DriveOutput, ResolveApprovalError, RootReply,
};

#[doc(no_inline)]
pub use claw_api::RetryPolicy;

/// The unified contract a scheduler drives any agent through.
///
/// Object-safe so heterogeneous agents can be held as `Box<dyn Agent>`. The
/// surface mirrors [`BaseAgent`]: commands go in via [`send_command`](Agent::send_command),
/// outcomes come out via [`tick`](Agent::tick). The one multi-agent extension is
/// [`deliver_child_result`](Agent::deliver_child_result) — the channel a parent
/// receives a finished subagent's result on (a separate port rather than a new
/// [`AgentCommand`] variant, so the base command vocabulary stays untouched).
pub trait Agent: Send {
    /// This agent's stable identity.
    fn id(&self) -> AgentId;

    /// Hand the agent one command. See [`AgentCommand`] for the vocabulary and
    /// [`AgentCommandError`] for when a command is illegal in the current state.
    fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError>;

    /// Deliver a finished subagent's result back to this (parent) agent.
    ///
    /// The result re-enters as ordinary information for the model to reason over
    /// (it does not preempt or gate anything); the agent owns how it is presented.
    fn deliver_child_result(&mut self, child: AgentId, text: String, ok: bool);

    /// Advance the agent by one step and report what happened. See [`TickOutcome`].
    fn tick(&mut self) -> TickOutcome;
}

/// Shared: present a subagent's result as a provenance-tagged message and append
/// it to the agent's base memory.
///
/// Child results re-enter the conversation as information the model re-decides
/// over (no counting, no gating); both semantic agents handle them identically,
/// so the formatting lives here once.
fn append_child_result(base: &mut BaseAgent, child: AgentId, text: String, ok: bool) {
    let status = if ok { "ok" } else { "failed" };
    base.append_message(format!("[subagent {child} {status}] {text}"));
}
