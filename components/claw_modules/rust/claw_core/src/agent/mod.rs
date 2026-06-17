//! Agents driven by the orchestrator.

pub mod base_agent;
mod internal_tools;

#[cfg(test)]
mod gating_tests;

pub use base_agent::{
    AgentAbortHandle, AgentCommand, AgentCommandError, AgentId, AgentRunError, AgentState,
    ApprovalDecision, ApprovalId, BaseAgent, BaseAgentBuildError, BaseAgentBuilder, CancelReason,
    TickOutcome, ToolGatingError,
};

#[doc(no_inline)]
pub use claw_api::RetryPolicy;
