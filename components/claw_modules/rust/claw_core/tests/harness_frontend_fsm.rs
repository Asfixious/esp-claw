//! Frontend semantic FSM unit tests.

use claw_core::agent_spec::{
    apply_patches, AgentSpec, AgentState, FrontendPhase, RoleState, RunStatus, TransitionInput,
    TransitionSignal,
};
use claw_core::agents::FrontendAgentSpec;
use claw_core::protocol::{AgentEvent, SessionId, StepId, TaskId, WorkerId};

const SESS: SessionId = SessionId(1);
const TASK: TaskId = TaskId(1);

#[test]
fn frontend_intake_to_delegate() {
    let spec = FrontendAgentSpec::new();
    let state = AgentState::frontend("fe-1", "run-1", SESS);
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::TaskUnderstood),
            ..Default::default()
        },
    );
    let mut next = state;
    apply_patches(&mut next, &out.patches);
    if let RoleState::Frontend(frontend) = &next.role_state {
        assert_eq!(frontend.phase, FrontendPhase::Delegate);
    } else {
        panic!("expected frontend");
    }
}

#[test]
fn frontend_delegate_to_watch_on_task_created() {
    let spec = FrontendAgentSpec::new();
    let mut state = AgentState::frontend("fe-1", "run-1", SESS);
    apply_patches(
        &mut state,
        &[claw_core::agent_spec::StatePatch::SetFrontendPhase(
            FrontendPhase::Delegate,
        )],
    );
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::TaskCreated {
                task_id: TASK,
                worker_id: WorkerId::for_task(TASK),
            }),
            ..Default::default()
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Frontend(frontend) = &state.role_state {
        assert_eq!(frontend.phase, FrontendPhase::Watch);
        assert_eq!(frontend.task_id, Some(TASK));
        assert_eq!(frontend.worker_instance_id, Some(WorkerId::for_task(TASK)));
    } else {
        panic!("expected frontend");
    }
}

#[test]
fn frontend_watch_done_moves_to_report() {
    let spec = FrontendAgentSpec::new();
    let mut state = AgentState::frontend("fe-1", "run-1", SESS);
    apply_patches(
        &mut state,
        &[
            claw_core::agent_spec::StatePatch::SetFrontendPhase(FrontendPhase::Watch),
            claw_core::agent_spec::StatePatch::SetTaskId(TASK),
        ],
    );
    let out = spec.handle_event(
        &state,
        &AgentEvent::Done {
            task_id: TASK,
            worker_instance_id: Some(WorkerId(1)),
            summary: "finished".into(),
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Frontend(frontend) = &state.role_state {
        assert_eq!(frontend.phase, FrontendPhase::Report);
        assert_eq!(frontend.report_draft.as_deref(), Some("finished"));
    } else {
        panic!("expected frontend");
    }
}

#[test]
fn frontend_approval_requested_moves_to_ask_approval() {
    let spec = FrontendAgentSpec::new();
    let mut state = AgentState::frontend("fe-1", "run-1", SESS);
    apply_patches(
        &mut state,
        &[
            claw_core::agent_spec::StatePatch::SetFrontendPhase(FrontendPhase::Watch),
            claw_core::agent_spec::StatePatch::SetTaskId(TASK),
        ],
    );
    let out = spec.handle_event(
        &state,
        &AgentEvent::ApprovalRequested {
            task_id: TASK,
            worker_instance_id: Some(WorkerId(1)),
            step_id: StepId(2),
            reason: "dangerous".into(),
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Frontend(frontend) = &state.role_state {
        assert_eq!(frontend.phase, FrontendPhase::AskApproval);
    } else {
        panic!("expected frontend");
    }
}

#[test]
fn frontend_report_to_done() {
    let spec = FrontendAgentSpec::new();
    let mut state = AgentState::frontend("fe-1", "run-1", SESS);
    apply_patches(
        &mut state,
        &[claw_core::agent_spec::StatePatch::SetFrontendPhase(
            FrontendPhase::Report,
        )],
    );
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::ReportSent),
            ..Default::default()
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Frontend(frontend) = &state.role_state {
        assert_eq!(frontend.phase, FrontendPhase::Done);
    }
    assert_eq!(state.run_status, RunStatus::Done);
}
