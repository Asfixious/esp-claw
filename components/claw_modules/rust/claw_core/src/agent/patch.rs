//! State patches applied by the orchestrator after transitions.

use serde::{Deserialize, Serialize};

use super::role::{FrontendPhase, RunStatus, WorkerPhase};
use super::state::{AgentState, RoleState};
use crate::protocol::{StepId, TaskId, WorkerId};

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
