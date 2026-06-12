//! Agent roles, run status, and semantic phase enums.

use serde::{Deserialize, Serialize};

/// Agent role in the multi-agent runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentRole {
    Frontend,
    Worker,
}

/// Scheduler-visible run status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Runnable,
    Waiting,
    Done,
    Cancelled,
}

/// Frontend semantic phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrontendPhase {
    Intake,
    Clarify,
    Delegate,
    Watch,
    AskApproval,
    Report,
    Done,
}

/// Worker semantic phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerPhase {
    Plan,
    Act,
    Verify,
    Paused,
    Blocked,
    Done,
}

/// Role-specific phase wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticPhase {
    Frontend(FrontendPhase),
    Worker(WorkerPhase),
}
