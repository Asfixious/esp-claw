//! [`AgentSpec`] trait and per-iteration context types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::role::AgentRole;
use super::state::AgentState;
use super::transition::{TransitionInput, TransitionOutput};
use crate::protocol::{AgentEvent, TaskContract};

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

/// Layer 2 semantic controller for one agent role.
pub trait AgentSpec: Send + Sync {
    fn role(&self) -> AgentRole;

    /// Role-specific `# STATE` block for the system prompt.
    fn state_block(&self, input: &ContextBuildInput<'_>) -> String;

    /// Role-specific `# PLAN` block for the system prompt.
    fn plan_block(&self, input: &ContextBuildInput<'_>) -> String;

    /// OpenAI-style messages for this iteration (excluding the system prompt).
    fn conversation_messages(&self, input: &ContextBuildInput<'_>) -> Value;

    fn visible_tools(&self, input: &ContextBuildInput<'_>) -> ToolVisibility;

    fn expected_schema(&self, input: &ContextBuildInput<'_>) -> OutputSchema;

    fn should_call_llm(&self, state: &AgentState) -> bool;

    fn transition(&self, state: &AgentState, input: &TransitionInput) -> TransitionOutput;

    fn handle_event(&self, state: &AgentState, event: &AgentEvent) -> TransitionOutput;
}
