//! Worker semantic controller: PLAN / ACT / VERIFY / PAUSED / BLOCKED / DONE.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::{
    AgentRegistrar, AgentRegistry, AgentRole, AgentSpec, AgentState, ContextBuildInput,
    OutputSchema, RoleState, RunStatus, StatePatch, ToolVisibility,
    TransitionInput, TransitionOutput, TransitionSignal, WorkerPhase, WorkerState,
};
use crate::llm_output::ValidatedLlmOutput;
use crate::protocol::{AgentEvent, StepId};
use crate::runtime::{ActionSummary, HarnessIterationOutput};

pub struct WorkerAgentSpec;

impl WorkerAgentSpec {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WorkerAgentSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSpec for WorkerAgentSpec {
    fn role(&self) -> AgentRole {
        AgentRole::Worker
    }

    fn state_block(&self, input: &ContextBuildInput<'_>) -> String {
        let RoleState::Worker(worker) = &input.state.role_state else {
            return "# STATE\n(invalid role)".to_string();
        };
        format!(
            "# STATE\ninstance={}\nrun={}\niteration={}\nphase={:?}\ntask={}\nstep={:?}\nblocked={:?}",
            input.state.instance_id,
            input.state.run_id,
            input.state.iteration_id,
            worker.phase,
            worker.task_id,
            worker.current_step_id(),
            worker.blocked_reason,
        )
    }

    fn plan_block(&self, input: &ContextBuildInput<'_>) -> String {
        let header = "# PLAN\n";
        let RoleState::Worker(worker) = &input.state.role_state else {
            return format!("{header}(invalid role)");
        };
        let steps: Vec<String> = worker.plan_steps.iter().map(ToString::to_string).collect();
        let current = worker
            .current_step_id()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(none)".to_string());
        let revision = worker
            .revision_note
            .as_deref()
            .map(|n| format!("\nrevision_note={n}"))
            .unwrap_or_default();
        format!(
            "{header}task={}\ncurrent_step={current}\nsteps={steps:?}{revision}",
            worker.task_id
        )
    }

    fn conversation_messages(&self, input: &ContextBuildInput<'_>) -> Value {
        let RoleState::Worker(worker) = &input.state.role_state else {
            return Value::Array(Vec::new());
        };
        worker.message_tail.clone()
    }

    fn visible_tools(&self, input: &ContextBuildInput<'_>) -> ToolVisibility {
        let tools = match &input.state.role_state {
            RoleState::Worker(worker) => match worker.phase {
                WorkerPhase::Plan => None,
                WorkerPhase::Act | WorkerPhase::Verify => Some(
                    r#"[{"type":"function","function":{"name":"files","description":"file ops"}}]"#
                        .to_string(),
                ),
                WorkerPhase::Paused | WorkerPhase::Blocked | WorkerPhase::Done => None,
            },
            _ => None,
        };
        ToolVisibility { tools_json: tools }
    }

    fn expected_schema(&self, input: &ContextBuildInput<'_>) -> OutputSchema {
        let name = match &input.state.role_state {
            RoleState::Worker(worker) => match worker.phase {
                WorkerPhase::Plan => "worker_plan",
                WorkerPhase::Act => "worker_act",
                WorkerPhase::Verify => "worker_verify",
                _ => "worker_idle",
            },
            _ => "worker_idle",
        };
        OutputSchema {
            name: name.to_string(),
            description: "Structured worker phase output".to_string(),
        }
    }

    fn should_call_llm(&self, state: &AgentState) -> bool {
        match &state.role_state {
            RoleState::Worker(worker) => match worker.phase {
                WorkerPhase::Plan | WorkerPhase::Act | WorkerPhase::Verify => {
                    !worker.awaiting_plan_approval
                }
                WorkerPhase::Paused | WorkerPhase::Blocked | WorkerPhase::Done => false,
            },
            _ => false,
        }
    }

    fn transition(&self, state: &AgentState, input: &TransitionInput) -> TransitionOutput {
        let RoleState::Worker(worker) = &state.role_state else {
            return TransitionOutput::default();
        };

        if let Some(signal) = &input.signal {
            return self.transition_signal(worker, signal, input.requires_plan_approval);
        }

        if let Some(iter) = &input.iteration {
            return self.transition_iteration(worker, iter, input.requires_plan_approval);
        }

        TransitionOutput::default()
    }

    fn handle_event(&self, _state: &AgentState, event: &AgentEvent) -> TransitionOutput {
        let mut out = TransitionOutput::default();
        if let AgentEvent::ApprovalRequested { step_id, reason, .. } = event {
            out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Paused));
            out.patches.push(StatePatch::SetPausedStep(Some(*step_id)));
            out.events.push(event.clone());
            let _ = reason;
        }
        out
    }
}

impl WorkerAgentSpec {
    fn apply_plan_ready(
        &self,
        worker: &WorkerState,
        steps: Vec<StepId>,
        requires_plan_approval: bool,
        out: &mut TransitionOutput,
    ) {
        out.patches.push(StatePatch::SetPlanSteps(steps.clone()));
        out.events.push(AgentEvent::PlanProposed {
            task_id: worker.task_id,
            worker_instance_id: None,
            steps,
        });
        if requires_plan_approval {
            out.patches
                .push(StatePatch::SetAwaitingPlanApproval(true));
            out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Paused));
            out.patches.push(StatePatch::SetRunStatus(RunStatus::Waiting));
        } else {
            out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Act));
        }
    }

    fn transition_signal(
        &self,
        worker: &WorkerState,
        signal: &TransitionSignal,
        requires_plan_approval: bool,
    ) -> TransitionOutput {
        let mut out = TransitionOutput::default();
        match (worker.phase, signal) {
            (WorkerPhase::Plan, TransitionSignal::PlanReady { steps }) => {
                self.apply_plan_ready(worker, steps.clone(), requires_plan_approval, &mut out);
            }
            (WorkerPhase::Act, TransitionSignal::StepExecuted) => {
                out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Verify));
            }
            (WorkerPhase::Verify, TransitionSignal::VerifyFailed) => {
                out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Act));
            }
            (WorkerPhase::Verify, TransitionSignal::VerifyPassed) => {
                self.advance_or_finish(worker, &mut out, "all steps verified".to_string());
            }
            (_, TransitionSignal::NeedsApproval) => {
                out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Paused));
                out.patches
                    .push(StatePatch::SetPausedStep(worker.current_step_id()));
                out.events.push(AgentEvent::ApprovalRequested {
                    task_id: worker.task_id,
                    worker_instance_id: None,
                    step_id: worker.current_step_id().unwrap_or(StepId(0)),
                    reason: "approval required".to_string(),
                });
            }
            (_, TransitionSignal::MissingCapability { reason }) => {
                out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Blocked));
                out.patches
                    .push(StatePatch::SetBlockedReason(Some(reason.clone())));
                out.patches.push(StatePatch::SetRunStatus(RunStatus::Waiting));
                out.events.push(AgentEvent::Blocked {
                    task_id: worker.task_id,
                    worker_instance_id: None,
                    step_id: worker.current_step_id(),
                    reason: reason.clone(),
                });
            }
            _ => {}
        }
        out
    }

    fn transition_iteration(
        &self,
        worker: &WorkerState,
        iter: &HarnessIterationOutput,
        requires_plan_approval: bool,
    ) -> TransitionOutput {
        let mut out = TransitionOutput::default();
        match worker.phase {
            WorkerPhase::Plan => {
                if let Some(ValidatedLlmOutput::WorkerPlan { steps }) = &iter.validated {
                    self.apply_plan_ready(worker, steps.clone(), requires_plan_approval, &mut out);
                }
            }
            WorkerPhase::Act => {
                if matches!(iter.action, ActionSummary::ToolCalls { .. } | ActionSummary::PlainText { .. }) {
                    out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Verify));
                    if let Some(step_id) = worker.current_step_id() {
                        out.events.push(AgentEvent::Progress {
                            task_id: worker.task_id,
                            worker_instance_id: None,
                            step_id,
                            message: iter.observation.clone().unwrap_or_default(),
                        });
                    }
                }
            }
            WorkerPhase::Verify => {
                if let Some(ValidatedLlmOutput::WorkerVerify { passed, summary }) = &iter.validated {
                    if *passed {
                        self.advance_or_finish(
                            worker,
                            &mut out,
                            summary.clone().unwrap_or_else(|| "verify pass".to_string()),
                        );
                    } else {
                        out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Act));
                    }
                }
            }
            WorkerPhase::Paused | WorkerPhase::Blocked | WorkerPhase::Done => {}
        }
        out
    }

    fn advance_or_finish(&self, worker: &WorkerState, out: &mut TransitionOutput, summary: String) {
        if let Some(next) = worker.next_step() {
            out.patches.push(StatePatch::SetCurrentStep(Some(next)));
            out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Act));
        } else {
            out.patches.push(StatePatch::SetWorkerPhase(WorkerPhase::Done));
            out.patches.push(StatePatch::SetRunStatus(RunStatus::Done));
            out.events.push(AgentEvent::Done {
                task_id: worker.task_id,
                worker_instance_id: None,
                summary,
            });
        }
    }
}

/// Registers [`WorkerAgentSpec`] into a registry.
pub struct Registrar;

impl AgentRegistrar for Registrar {
    fn register(&self, registry: &mut AgentRegistry) {
        registry.register(Arc::new(WorkerAgentSpec::new()));
    }
}
