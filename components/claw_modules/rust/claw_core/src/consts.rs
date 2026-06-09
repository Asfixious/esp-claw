//! Constants and small enums mirroring `claw_core.h` and `claw_core_internal.h`.

use core::ffi::c_int;

// --- request flags (claw_core.h) -----------------------------------------
pub const REQUEST_FLAG_PUBLISH_OUT_MESSAGE: u32 = 1 << 0;
pub const REQUEST_FLAG_SKIP_RESPONSE_QUEUE: u32 = 1 << 1;
pub const REQUEST_FLAG_USER_INTERRUPT: u32 = 1 << 2;
pub const REQUEST_FLAG_PUBLISH_STAGE_MESSAGE: u32 = 1 << 3;

pub const CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY: u32 = 1 << 0;

// --- internal defaults (claw_core_internal.h) ----------------------------
pub const DEFAULT_STACK_SIZE: u32 = 8 * 1024;
pub const DEFAULT_PRIORITY: u32 = 5;
pub const DEFAULT_REQUEST_Q: u32 = 4;
pub const DEFAULT_RESPONSE_Q: u32 = 4;
pub const DEFAULT_TOOL_ITERATIONS: u32 = 10;
pub const INSERT_QUEUE_LEN: usize = 4;
pub const INFLIGHT_SESSION_ID_SIZE: usize = 128;
pub const LOG_SNIPPET_LEN: usize = 96;
pub const TOOL_SUMMARY_MAX_LEN: usize = 768;
pub const OBS_CSV_MAX: usize = 384;
pub const MAX_COMPLETION_OBSERVERS: usize = 4;

/// `claw_core_response_status_t`
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseStatus {
    Ok = 0,
    Error = 1,
}

/// `claw_core_completion_type_t`
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionType {
    Done = 0,
}

/// `claw_core_agent_loop_phase_t`
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLoopPhase {
    Idle = 0,
    BeforeBuildIterationContext = 1,
    BuildingIterationContext = 2,
    BeforeLlmHttp = 3,
    InLlmHttp = 4,
    AfterLlmBeforeTool = 5,
    RunningTool = 6,
    Finalizing = 7,
}

impl AgentLoopPhase {
    pub fn as_c(self) -> c_int {
        self as c_int
    }

    /// Insertable phases mirror `claw_core_agent_loop_phase_is_insertable`:
    /// any phase that is neither Idle nor Finalizing.
    pub fn is_insertable(self) -> bool {
        !matches!(self, AgentLoopPhase::Idle | AgentLoopPhase::Finalizing)
    }
}

/// `claw_core_context_record_type_t`
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextRecordType {
    User = 1,
    AssistantFinal = 2,
    AssistantTool = 3,
    ToolResult = 4,
}

/// `claw_core_context_kind_t`
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    SystemPrompt = 0,
    Messages = 1,
    Tools = 2,
}

impl ContextKind {
    /// `claw_core_context_kind_to_string`
    pub fn as_str(self) -> &'static str {
        match self {
            ContextKind::SystemPrompt => "system_prompt",
            ContextKind::Messages => "messages",
            ContextKind::Tools => "tools",
        }
    }
}

/// `claw_core_control_abort_reason_t`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortReason {
    None,
    Cancel,
    UserInterrupt,
}
