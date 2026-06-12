//! Per-role and per-instance agent state.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::role::{AgentRole, FrontendPhase, RunStatus, SemanticPhase, WorkerPhase};
use crate::protocol::{SessionId, StepId, TaskId, WorkerId};

/// Frontend-owned state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrontendState {
    pub phase: FrontendPhase,
    pub session_id: SessionId,
    pub pending_user_text: Vec<String>,
    pub task_id: Option<TaskId>,
    pub worker_instance_id: Option<WorkerId>,
    /// Draft text for the user-facing REPORT phase (worker summary, status, or LLM polish).
    pub report_draft: Option<String>,
}

impl FrontendState {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            phase: FrontendPhase::Intake,
            session_id,
            pending_user_text: Vec::new(),
            task_id: None,
            worker_instance_id: None,
            report_draft: None,
        }
    }
}

/// Worker-owned state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerState {
    pub phase: WorkerPhase,
    pub task_id: TaskId,
    pub current_step: Option<StepId>,
    pub plan_steps: Vec<StepId>,
    pub blocked_reason: Option<String>,
    pub paused_step: Option<StepId>,
    pub revision_note: Option<String>,
    // todo
    pub awaiting_plan_approval: bool,
    pub message_tail: Value,
}

impl WorkerState {
    pub fn new(task_id: TaskId) -> Self {
        Self {
            phase: WorkerPhase::Plan,
            task_id,
            current_step: None,
            plan_steps: Vec::new(),
            blocked_reason: None,
            paused_step: None,
            revision_note: None,
            awaiting_plan_approval: false,
            message_tail: Value::Array(Vec::new()),
        }
    }

    pub fn current_step_id(&self) -> Option<StepId> {
        self.current_step
    }

    pub fn current_step_position(&self) -> Option<usize> {
        let current = self.current_step?;
        self.plan_steps.iter().position(|step| *step == current)
    }

    pub fn next_step(&self) -> Option<StepId> {
        let pos = self.current_step_position()?;
        self.plan_steps.get(pos + 1).copied()
    }
}

/// Role-specific sub-state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RoleState {
    Frontend(FrontendState),
    Worker(WorkerState),
}

impl RoleState {
    pub fn frontend(session_id: SessionId) -> Self {
        Self::Frontend(FrontendState::new(session_id))
    }

    pub fn worker(task_id: TaskId) -> Self {
        Self::Worker(WorkerState::new(task_id))
    }

    pub fn role(&self) -> AgentRole {
        match self {
            Self::Frontend(_) => AgentRole::Frontend,
            Self::Worker(_) => AgentRole::Worker,
        }
    }

    pub fn semantic_phase(&self) -> SemanticPhase {
        match self {
            Self::Frontend(s) => SemanticPhase::Frontend(s.phase),
            Self::Worker(s) => SemanticPhase::Worker(s.phase),
        }
    }

    pub fn as_frontend(&self) -> Option<&FrontendState> {
        match self {
            Self::Frontend(s) => Some(s),
            Self::Worker(_) => None,
        }
    }

    pub fn as_worker(&self) -> Option<&WorkerState> {
        match self {
            Self::Frontend(_) => None,
            Self::Worker(s) => Some(s),
        }
    }
}

/// Full per-instance agent state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub instance_id: String,
    pub role: AgentRole,
    pub run_id: String,
    pub run_status: RunStatus,
    pub iteration_id: u32,
    pub role_state: RoleState,
}

impl AgentState {
    pub fn new(
        instance_id: impl Into<String>,
        run_id: impl Into<String>,
        role_state: RoleState,
    ) -> Self {
        let role = role_state.role();
        Self {
            instance_id: instance_id.into(),
            role,
            run_id: run_id.into(),
            run_status: RunStatus::Runnable,
            iteration_id: 0,
            role_state,
        }
    }

    pub fn frontend(instance_id: impl Into<String>, run_id: impl Into<String>, session_id: SessionId) -> Self {
        Self::new(instance_id, run_id, RoleState::frontend(session_id))
    }

    pub fn worker(instance_id: impl Into<String>, run_id: impl Into<String>, task_id: TaskId) -> Self {
        Self::new(instance_id, run_id, RoleState::worker(task_id))
    }

    pub fn semantic_phase(&self) -> SemanticPhase {
        self.role_state.semantic_phase()
    }
}
