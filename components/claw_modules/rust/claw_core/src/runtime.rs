//! Layer 3 adapter: one [`crate::iteration_loop::IterationLoop`] step per harness tick.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use claw_api::ClawApi;
use claw_cap::CapabilityInvoker;
use serde::{Deserialize, Serialize};

use crate::agent_spec::ContextBundle;
use crate::llm_output::ValidatedLlmOutput;
use crate::iteration_loop::{
    AppendedMessages, ChatMessages, IterationLoop, IterationLoopError, IterationLoopPhase,
    IterationResult, IterationStep, RequestControl, SystemPrompt, ToolSet,
};
use crate::observability::SpanName;
use crate::request::RequestItem;

/// Summary of model action from one iteration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ActionSummary {
    PlainText { text: String },
    ToolCalls { names: Vec<String> },
    Interrupted { user_messages: Vec<String> },
}

impl Default for ActionSummary {
    fn default() -> Self {
        Self::PlainText {
            text: String::new(),
        }
    }
}

/// Harness-level output after one Layer 3 step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessIterationOutput {
    pub action: ActionSummary,
    pub observation: Option<String>,
    pub validated: Option<ValidatedLlmOutput>,
    #[serde(skip)]
    pub appended_messages: AppendedMessages,
    pub spans: Vec<SpanName>,
}

impl Default for HarnessIterationOutput {
    fn default() -> Self {
        Self {
            action: ActionSummary::default(),
            observation: None,
            validated: None,
            appended_messages: AppendedMessages::empty(),
            spans: Vec::new(),
        }
    }
}

/// Per-instance interrupt control for [`IterationLoop`].
pub struct InstanceControl {
    phase: Mutex<IterationLoopPhase>,
    abort_flag: Arc<AtomicBool>,
    interrupts: Mutex<Vec<String>>,
}

impl InstanceControl {
    pub fn new() -> Self {
        Self {
            phase: Mutex::new(IterationLoopPhase::Idle),
            abort_flag: Arc::new(AtomicBool::new(false)),
            interrupts: Mutex::new(Vec::new()),
        }
    }

    pub fn queue_interrupt(&self, text: impl Into<String>) {
        self.interrupts.lock().unwrap().push(text.into());
    }

    pub fn phase(&self) -> IterationLoopPhase {
        *self.phase.lock().unwrap()
    }
}

impl Default for InstanceControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestControl for InstanceControl {
    fn set_phase(&self, phase: IterationLoopPhase) {
        *self.phase.lock().unwrap() = phase;
    }

    fn abort_flag(&self) -> &Arc<AtomicBool> {
        &self.abort_flag
    }

    fn take_user_interrupt_http_abort(&self, _request_id: u32) -> bool {
        false
    }

    fn dequeue_inserted_user_inputs(&self, _session_id: &str, max: usize) -> Vec<String> {
        let mut q = self.interrupts.lock().unwrap();
        let take = max.min(q.len());
        q.drain(..take).collect()
    }

    fn clear_user_interrupt_abort(&self, _request_id: u32) {}
}

/// Map [`IterationResult`] into harness output.
pub fn adapt_iteration_result(result: IterationResult) -> HarnessIterationOutput {
    match result {
        IterationResult::PlainText(outcome) => HarnessIterationOutput {
            action: ActionSummary::PlainText {
                text: outcome.text.clone(),
            },
            observation: None,
            validated: None,
            appended_messages: AppendedMessages::empty(),
            spans: vec![SpanName::LlmHttp, SpanName::Finalize],
        },
        IterationResult::Tools(outcome) => {
            let names: Vec<String> = outcome.runs.iter().map(|r| r.name.clone()).collect();
            let observation = outcome
                .runs
                .iter()
                .map(|r| format!("{}:{}", r.name, if r.ok { "ok" } else { "fail" }))
                .collect::<Vec<_>>()
                .join(", ");
            HarnessIterationOutput {
                action: ActionSummary::ToolCalls { names },
                observation: Some(observation),
                validated: None,
                appended_messages: outcome.appended,
                spans: vec![SpanName::LlmHttp, SpanName::RunTool, SpanName::Finalize],
            }
        }
        IterationResult::Interrupted(outcome) => {
            let user_messages: Vec<String> = outcome
                .appended
                .0
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            HarnessIterationOutput {
                action: ActionSummary::Interrupted { user_messages },
                observation: None,
                validated: None,
                appended_messages: outcome.appended,
                spans: vec![SpanName::LlmHttp],
            }
        }
    }
}

/// Execute one Layer 3 step.
pub fn run_iteration(
    llm: &ClawApi,
    invoker: Option<&dyn CapabilityInvoker>,
    control: &InstanceControl,
    request: &RequestItem,
    bundle: &ContextBundle,
) -> Result<HarnessIterationOutput, IterationLoopError> {
    let step = IterationStep {
        request,
        system_prompt: SystemPrompt(&bundle.system_prompt),
        messages: ChatMessages(&bundle.messages),
        tools: ToolSet::new(bundle.tools_json.as_deref()),
    };
    let loop_ = IterationLoop {
        llm,
        capability_invoker: invoker,
        control,
    };
    let mut output = adapt_iteration_result(loop_.run(step)?);
    output.spans.insert(0, SpanName::BuildContext);
    Ok(output)
}
