//! Frontend semantic controller: INTAKE / CLARIFY / DELEGATE / WATCH / ASK_APPROVAL / REPORT.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent::{
    AgentRegistrar, AgentRegistry, AgentRole, AgentSpec, AgentState, ContextBuildInput,
    FrontendPhase, FrontendState, OutputSchema, RoleState, RunStatus, StatePatch,
    ToolVisibility, TransitionInput, TransitionOutput, TransitionSignal,
};
use crate::llm_output::{FrontendIntakeAction, ValidatedLlmOutput};
use crate::protocol::AgentEvent;

pub struct FrontendAgentSpec;

impl FrontendAgentSpec {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FrontendAgentSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSpec for FrontendAgentSpec {
    fn role(&self) -> AgentRole {
        AgentRole::Frontend
    }

    fn state_block(&self, input: &ContextBuildInput<'_>) -> String {
        let RoleState::Frontend(frontend) = &input.state.role_state else {
            return "# STATE\n(invalid role)".to_string();
        };
        format!(
            "# STATE\ninstance={}\nrun={}\niteration={}\nphase={:?}\ntask_id={:?}\nworker={:?}",
            input.state.instance_id,
            input.state.run_id,
            input.state.iteration_id,
            frontend.phase,
            frontend.task_id,
            frontend.worker_instance_id,
        )
    }

    fn plan_block(&self, input: &ContextBuildInput<'_>) -> String {
        let header = "# PLAN\n";
        if let Some(task) = input.task {
            format!(
                "{header}task={}\ngoal={}\nstatus={:?}",
                task.task_id, task.goal, task.status
            )
        } else {
            format!("{header}(no active task)")
        }
    }

    fn conversation_messages(&self, input: &ContextBuildInput<'_>) -> Value {
        let RoleState::Frontend(frontend) = &input.state.role_state else {
            return Value::Array(Vec::new());
        };
        let messages = frontend
            .pending_user_text
            .iter()
            .map(|text| json!({"role": "user", "content": text}))
            .collect();
        Value::Array(messages)
    }

    fn visible_tools(&self, _input: &ContextBuildInput<'_>) -> ToolVisibility {
        ToolVisibility::default()
    }

    fn expected_schema(&self, input: &ContextBuildInput<'_>) -> OutputSchema {
        let name = match &input.state.role_state {
            RoleState::Frontend(frontend) => match frontend.phase {
                FrontendPhase::Intake | FrontendPhase::Clarify => "frontend_intake",
                FrontendPhase::Delegate => "frontend_delegate",
                FrontendPhase::Watch | FrontendPhase::AskApproval => "frontend_watch",
                FrontendPhase::Report | FrontendPhase::Done => "frontend_report",
            },
            _ => "frontend_idle",
        };
        OutputSchema {
            name: name.to_string(),
            description: "Frontend phase output".to_string(),
        }
    }

    fn should_call_llm(&self, state: &AgentState) -> bool {
        match &state.role_state {
            RoleState::Frontend(frontend) => matches!(
                frontend.phase,
                FrontendPhase::Intake
                    | FrontendPhase::Clarify
                    | FrontendPhase::Delegate
                    | FrontendPhase::Report
            ),
            _ => false,
        }
    }

    fn transition(&self, state: &AgentState, input: &TransitionInput) -> TransitionOutput {
        let RoleState::Frontend(frontend) = &state.role_state else {
            return TransitionOutput::default();
        };

        if let Some(signal) = &input.signal {
            return self.transition_signal(frontend, signal);
        }

        if let Some(iter) = &input.iteration {
            return self.transition_iteration(frontend, iter);
        }

        TransitionOutput::default()
    }

    fn handle_event(&self, state: &AgentState, event: &AgentEvent) -> TransitionOutput {
        let RoleState::Frontend(frontend) = &state.role_state else {
            return TransitionOutput::default();
        };
        let mut out = TransitionOutput::default();
        match (frontend.phase, event) {
            (FrontendPhase::Watch, AgentEvent::PlanProposed { steps, .. }) => {
                out.patches.push(StatePatch::SetReportDraft(format!(
                    "proposed plan: {steps:?}"
                )));
            }
            (FrontendPhase::Watch, AgentEvent::ApprovalRequested { .. }) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::AskApproval));
            }
            (FrontendPhase::Watch, AgentEvent::Blocked { reason, .. }) => {
                out.patches
                    .push(StatePatch::SetReportDraft(format!("worker blocked: {reason}")));
            }
            (FrontendPhase::Watch, AgentEvent::Done { summary, .. }) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Report));
                out.patches.push(StatePatch::SetReportDraft(summary.clone()));
            }
            _ => {}
        }
        out
    }
}

impl FrontendAgentSpec {
    fn transition_signal(
        &self,
        frontend: &FrontendState,
        signal: &TransitionSignal,
    ) -> TransitionOutput {
        let mut out = TransitionOutput::default();
        match (frontend.phase, signal) {
            (FrontendPhase::Intake, TransitionSignal::TaskUnderstood) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Delegate));
            }
            (FrontendPhase::Intake, TransitionSignal::NeedClarification) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Clarify));
            }
            (FrontendPhase::Clarify, TransitionSignal::TaskUnderstood) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Delegate));
            }
            (
                FrontendPhase::Delegate,
                TransitionSignal::TaskCreated {
                    task_id,
                    worker_id,
                },
            ) => {
                out.patches.push(StatePatch::SetTaskId(*task_id));
                out.patches.push(StatePatch::SetWorkerInstanceId(*worker_id));
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Watch));
            }
            (FrontendPhase::AskApproval, TransitionSignal::UserApproved) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Watch));
            }
            (FrontendPhase::Report, TransitionSignal::ReportSent) => {
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Done));
                out.patches.push(StatePatch::SetRunStatus(RunStatus::Done));
            }
            _ => {}
        }
        out
    }

    fn transition_iteration(
        &self,
        frontend: &FrontendState,
        iter: &crate::runtime::HarnessIterationOutput,
    ) -> TransitionOutput {
        let mut out = TransitionOutput::default();
        match frontend.phase {
            FrontendPhase::Intake | FrontendPhase::Clarify => {
                if let Some(ValidatedLlmOutput::FrontendIntake { action, .. }) = &iter.validated {
                    match action {
                        FrontendIntakeAction::TaskUnderstood => {
                            out.patches.push(StatePatch::SetFrontendPhase(
                                FrontendPhase::Delegate,
                            ));
                        }
                        FrontendIntakeAction::NeedClarification => {
                            out.patches.push(StatePatch::SetFrontendPhase(
                                FrontendPhase::Clarify,
                            ));
                        }
                    }
                } else if !frontend.pending_user_text.is_empty() {
                    out.patches
                        .push(StatePatch::SetFrontendPhase(FrontendPhase::Delegate));
                }
            }
            FrontendPhase::Delegate => {}
            FrontendPhase::Report => {
                if let Some(ValidatedLlmOutput::FrontendReport { summary }) = &iter.validated {
                    out.patches
                        .push(StatePatch::SetReportDraft(summary.clone()));
                }
                out.patches
                    .push(StatePatch::SetFrontendPhase(FrontendPhase::Done));
                out.patches.push(StatePatch::SetRunStatus(RunStatus::Done));
            }
            _ => {}
        }
        out
    }
}

/// Registers [`FrontendAgentSpec`] into a registry.
pub struct Registrar;

impl AgentRegistrar for Registrar {
    fn register(&self, registry: &mut AgentRegistry) {
        registry.register(Arc::new(FrontendAgentSpec::new()));
    }
}
