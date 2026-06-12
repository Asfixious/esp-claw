//! Transition inputs/outputs between orchestrator and semantic controllers.

use serde::{Deserialize, Serialize};

use super::patch::StatePatch;
use crate::protocol::{AgentEvent, StepId, TaskId, WorkerId};

/// Input to a semantic transition.
#[derive(Clone, Debug, Default)]
pub struct TransitionInput {
    pub iteration: Option<crate::runtime::HarnessIterationOutput>,
    pub signal: Option<TransitionSignal>,
    pub requires_plan_approval: bool,
}

/// Explicit deterministic signals for tests and event-driven transitions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionSignal {
    TaskUnderstood,
    NeedClarification,
    TaskCreated { task_id: TaskId, worker_id: WorkerId },
    PlanReady { steps: Vec<StepId> },
    StepExecuted,
    VerifyPassed,
    VerifyFailed,
    NeedsApproval,
    MissingCapability { reason: String },
    UserApproved,
    WorkerDone { summary: String },
    ReportSent,
}

/// Output of a semantic transition.
#[derive(Clone, Debug, Default)]
pub struct TransitionOutput {
    pub patches: Vec<StatePatch>,
    pub events: Vec<AgentEvent>,
}
