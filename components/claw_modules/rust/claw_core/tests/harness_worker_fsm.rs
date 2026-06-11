//! Worker semantic FSM unit tests.

use claw_core::agent_spec::{
    apply_patches, AgentRole, AgentSpec, AgentState, RoleState, RunStatus, TransitionInput,
    TransitionSignal, WorkerPhase,
};
use claw_core::agents::WorkerAgentSpec;
use claw_core::protocol::{AgentEvent, StepId, TaskId};

const TASK: TaskId = TaskId(1);
use claw_core::llm_output::ValidatedLlmOutput;
use claw_core::runtime::{ActionSummary, HarnessIterationOutput};

#[test]
fn worker_plan_to_act_via_signal() {
    let spec = WorkerAgentSpec::new();
    let state = AgentState::worker("w1", "run-1", TASK);
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::PlanReady {
                steps: vec![StepId(1), StepId(2)],
            }),
            ..Default::default()
        },
    );
    let mut next = state;
    apply_patches(&mut next, &out.patches);
    assert_eq!(spec.role(), AgentRole::Worker);
    if let RoleState::Worker(worker) = &next.role_state {
        assert_eq!(worker.phase, WorkerPhase::Act);
        assert_eq!(worker.plan_steps.len(), 2);
    } else {
        panic!("expected worker state");
    }
}

#[test]
fn worker_verify_pass_advances_or_done() {
    let spec = WorkerAgentSpec::new();
    let mut state = AgentState::worker("w1", "run-1", TASK);
    apply_patches(
        &mut state,
        &[
            claw_core::agent_spec::StatePatch::SetPlanSteps(vec![StepId(1), StepId(2)]),
            claw_core::agent_spec::StatePatch::SetWorkerPhase(WorkerPhase::Verify),
        ],
    );
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::VerifyPassed),
            ..Default::default()
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Worker(worker) = &state.role_state {
        assert_eq!(worker.phase, WorkerPhase::Act);
        assert_eq!(worker.current_step_id(), Some(StepId(2)));
    } else {
        panic!("expected worker");
    }

    apply_patches(
        &mut state,
        &[claw_core::agent_spec::StatePatch::SetWorkerPhase(WorkerPhase::Verify)],
    );
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::VerifyPassed),
            ..Default::default()
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Worker(worker) = &state.role_state {
        assert_eq!(worker.phase, WorkerPhase::Done);
    }
    assert_eq!(state.run_status, RunStatus::Done);
}

#[test]
fn worker_verify_fail_returns_to_act() {
    let spec = WorkerAgentSpec::new();
    let mut state = AgentState::worker("w1", "run-1", TASK);
    apply_patches(
        &mut state,
        &[claw_core::agent_spec::StatePatch::SetWorkerPhase(WorkerPhase::Verify)],
    );
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::VerifyFailed),
            ..Default::default()
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Worker(worker) = &state.role_state {
        assert_eq!(worker.phase, WorkerPhase::Act);
    } else {
        panic!("expected worker");
    }
}

#[test]
fn worker_blocked_emits_event() {
    let spec = WorkerAgentSpec::new();
    let state = AgentState::worker("w1", "run-1", TASK);
    let out = spec.transition(
        &state,
        &TransitionInput {
            signal: Some(TransitionSignal::MissingCapability {
                reason: "no gpio".into(),
            }),
            ..Default::default()
        },
    );
    assert!(matches!(out.events.first(), Some(AgentEvent::Blocked { .. })));
}

#[test]
fn worker_iteration_verify_text_fail() {
    let spec = WorkerAgentSpec::new();
    let mut state = AgentState::worker("w1", "run-1", TASK);
    apply_patches(
        &mut state,
        &[claw_core::agent_spec::StatePatch::SetWorkerPhase(WorkerPhase::Verify)],
    );
    let out = spec.transition(
        &state,
        &TransitionInput {
            iteration: Some(HarnessIterationOutput {
                action: ActionSummary::PlainText {
                    text: r#"{"action":"verify_failed"}"#.into(),
                },
                validated: Some(ValidatedLlmOutput::WorkerVerify {
                    passed: false,
                    summary: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    apply_patches(&mut state, &out.patches);
    if let RoleState::Worker(worker) = &state.role_state {
        assert_eq!(worker.phase, WorkerPhase::Act);
    } else {
        panic!("expected worker");
    }
}
