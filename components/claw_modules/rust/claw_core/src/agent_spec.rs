//! Layer 2 contract: role-specific semantic controllers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::{AgentEvent, SessionId, StepId, TaskContract, TaskId, WorkerId};

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
    // todo try to use enum later
    pub blocked_reason: Option<String>,
    pub paused_step: Option<StepId>,
    pub revision_note: Option<String>,
    // todo try to use message bus later
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
    pub fn frontend(instance_id: impl Into<String>, run_id: impl Into<String>, session_id: SessionId) -> Self {
        Self {
            instance_id: instance_id.into(),
            role: AgentRole::Frontend,
            run_id: run_id.into(),
            run_status: RunStatus::Runnable,
            iteration_id: 0,
            role_state: RoleState::Frontend(FrontendState::new(session_id)),
        }
    }

    pub fn worker(instance_id: impl Into<String>, run_id: impl Into<String>, task_id: TaskId) -> Self {
        Self {
            instance_id: instance_id.into(),
            role: AgentRole::Worker,
            run_id: run_id.into(),
            run_status: RunStatus::Runnable,
            iteration_id: 0,
            role_state: RoleState::Worker(WorkerState::new(task_id)),
        }
    }

    pub fn semantic_phase(&self) -> SemanticPhase {
        match &self.role_state {
            RoleState::Frontend(s) => SemanticPhase::Frontend(s.phase),
            RoleState::Worker(s) => SemanticPhase::Worker(s.phase),
        }
    }
}

/// Expected model output schema descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSchema {
    pub name: String,
    pub description: String,
}

/// Tool visibility for one iteration.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolVisibility {
    pub tools_json: Option<String>,
}

/// Inputs for context assembly.
#[derive(Clone, Debug)]
pub struct ContextBuildInput<'a> {
    pub state: &'a AgentState,
    pub task: Option<&'a TaskContract>,
    pub env_manifest: &'a str,
    pub global_policy: &'a str,
    pub memory_snapshot: &'a str,
}

/// Rendered OpenAI-style inputs for one [`crate::iteration_loop::IterationLoop`] step.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextBundle {
    pub system_prompt: String,
    pub messages: Value,
    pub tools_json: Option<String>,
}

/// Patch applied by the orchestrator after a transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StatePatch {
    SetRunStatus(RunStatus),
    SetFrontendPhase(FrontendPhase),
    SetWorkerPhase(WorkerPhase),
    SetTaskId(TaskId),
    SetWorkerInstanceId(WorkerId),
    SetPlanSteps(Vec<StepId>),
    SetCurrentStep(Option<StepId>),
    SetBlockedReason(Option<String>),
    SetPausedStep(Option<StepId>),
    PushUserText(String),
    SetReportDraft(String),
    SetRevisionNote(Option<String>),
    SetAwaitingPlanApproval(bool),
    BumpIterationId,
}

/// Input to a semantic transition.
#[derive(Clone, Debug, Default)]
pub struct TransitionInput {
    pub iteration: Option<crate::runtime::HarnessIterationOutput>,
    pub signal: Option<TransitionSignal>,
    // todo try to use message bus later
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

/// Layer 2 semantic controller for one agent role.
pub trait AgentSpec: Send + Sync {
    fn role(&self) -> AgentRole;

    fn phase(&self, state: &AgentState) -> SemanticPhase;

    fn build_context(&self, input: &ContextBuildInput<'_>) -> ContextBundle;

    fn visible_tools(&self, input: &ContextBuildInput<'_>) -> ToolVisibility;

    fn expected_schema(&self, input: &ContextBuildInput<'_>) -> OutputSchema;

    fn should_call_llm(&self, state: &AgentState) -> bool;

    fn transition(&self, state: &AgentState, input: &TransitionInput) -> TransitionOutput;

    fn handle_event(&self, state: &AgentState, event: &AgentEvent) -> TransitionOutput;
}

/// Apply [`StatePatch`] list onto [`AgentState`].
pub fn apply_patches(state: &mut AgentState, patches: &[StatePatch]) {
    for patch in patches {
        apply_patch(state, patch);
    }
}

fn apply_patch(state: &mut AgentState, patch: &StatePatch) {
    match patch {
        StatePatch::SetRunStatus(status) => state.run_status = *status,
        StatePatch::BumpIterationId => state.iteration_id += 1,
        StatePatch::SetTaskId(task_id) => {
            if let RoleState::Frontend(frontend) = &mut state.role_state {
                frontend.task_id = Some(*task_id);
            }
        }
        StatePatch::SetWorkerInstanceId(worker_id) => {
            if let RoleState::Frontend(frontend) = &mut state.role_state {
                frontend.worker_instance_id = Some(*worker_id);
            }
        }
        StatePatch::SetFrontendPhase(phase) => {
            if let RoleState::Frontend(frontend) = &mut state.role_state {
                frontend.phase = *phase;
            }
        }
        StatePatch::SetWorkerPhase(phase) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.phase = *phase;
            }
        }
        StatePatch::SetPlanSteps(steps) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.plan_steps = steps.clone();
                worker.current_step = worker.plan_steps.first().copied();
            }
        }
        StatePatch::SetCurrentStep(step) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.current_step = *step;
            }
        }
        StatePatch::SetBlockedReason(reason) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.blocked_reason = reason.clone();
            }
        }
        StatePatch::SetPausedStep(step) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.paused_step = *step;
            }
        }
        StatePatch::PushUserText(text) => {
            if let RoleState::Frontend(frontend) = &mut state.role_state {
                frontend.pending_user_text.push(text.clone());
            }
        }
        StatePatch::SetReportDraft(report) => {
            if let RoleState::Frontend(frontend) = &mut state.role_state {
                frontend.report_draft = Some(report.clone());
            }
        }
        StatePatch::SetRevisionNote(note) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.revision_note = note.clone();
            }
        }
        StatePatch::SetAwaitingPlanApproval(waiting) => {
            if let RoleState::Worker(worker) = &mut state.role_state {
                worker.awaiting_plan_approval = *waiting;
            }
        }
    }
}
