//! Tool port: model-facing catalog and invocation for one orchestrator iteration.

use thiserror::Error;

/// Who initiated a tool call (maps to `claw_cap_caller_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ToolCaller {
    #[default]
    System,
    RootAgent,
    Console,
    SubAgent,
}

/// Per-iteration routing identity for tools and skills.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnContext {
    pub iteration_id: u32,
    pub session_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_chat_id: Option<String>,
    pub target_channel: Option<String>,
    pub target_chat_id: Option<String>,
    pub source_cap: Option<String>,
    pub caller: ToolCaller,
}

/// Model-facing tool schemas for one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCatalog {
    pub llm_json: Option<String>,
}

/// One model tool_call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolInvocation<'a> {
    pub id: Option<&'a str>,
    pub name: &'a str,
    pub arguments_json: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub output: String,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("tool invoke failed: {0}")]
    InvokeFailed(String),
    #[error("out of memory")]
    NoMem,
}

pub trait Tools: Send + Sync {
    fn catalog(&self, ctx: &TurnContext) -> ToolCatalog;

    fn invoke(
        &self,
        call: &ToolInvocation<'_>,
        ctx: &TurnContext,
    ) -> Result<ToolOutput, ToolError>;
}

