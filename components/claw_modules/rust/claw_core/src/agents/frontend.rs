//! Frontend semantic controller: INTAKE / CLARIFY / DELEGATE / WATCH / ASK_APPROVAL / REPORT.

use crate::agent_spec::{
    AgentRole, AgentSpec, AgentState, ContextBuildInput, ContextBundle, FrontendPhase,
    OutputSchema, RoleState, RunStatus, SemanticPhase, StatePatch, ToolVisibility, TransitionInput,
    TransitionOutput, TransitionSignal,
};
use crate::context::ContextAssembler;
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

    fn phase(&self, state: &AgentState) -> SemanticPhase {
        state.semantic_phase()
    }

    fn build_context(&self, input: &ContextBuildInput<'_>) -> ContextBundle {
        let schema = self.expected_schema(input);
        ContextAssembler::assemble(input, &schema, None)
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
        frontend: &crate::agent_spec::FrontendState,
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
                out.patches.push(StatePatch::SetTaskId(task_id.clone()));
                out.patches
                    .push(StatePatch::SetWorkerInstanceId(worker_id.clone()));
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
        frontend: &crate::agent_spec::FrontendState,
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
