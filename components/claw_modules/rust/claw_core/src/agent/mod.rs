//! Agents driven by the orchestrator.
//!
//! [`BaseAgent`] is the command-in / outcome-out base layer. The two semantic
//! agents — [`WorkerAgent`] and [`ConversationAgent`] — each *directly own* a
//! [`BaseAgent`] and implement the unified [`Agent`] trait on top of it. The
//! trait is what a (future) scheduler drives; it never reaches into an agent's
//! internals.
//!
//! The two semantic agents share one spawn mechanism (a model-callable
//! `spawn_subagent` tool whose goals are drained each tick — it lives with the
//! other built-ins in [`internal_tools`]). They differ in their FSM and tool
//! gating: [`WorkerAgent`] runs a Plan/Act/Verify FSM over a fixed tool set
//! (hard-hide), while [`ConversationAgent`] runs an Intake/Verify/Delegate/Watch/
//! Report FSM that gates tools per stage (soft-hide).

pub mod base_agent;
pub mod conversation_agent;
mod internal_tools;
pub mod worker_agent;

pub use base_agent::{
    AgentAbortHandle, AgentCommand, AgentCommandError, AgentId, AgentRunError, AgentState,
    ApprovalDecision, ApprovalId, BaseAgent, BaseAgentBuildError, BaseAgentBuilder, CancelReason,
    TickOutcome,
};
pub use conversation_agent::{
    ConversationAgent, ConversationAgentBuildError, ConversationAgentBuilder, ConvPhase,
};
pub use worker_agent::{WorkerAgent, WorkerAgentBuildError, WorkerAgentBuilder};

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
