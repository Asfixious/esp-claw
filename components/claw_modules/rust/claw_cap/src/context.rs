//! Per-invocation routing and identity passed to capability handlers.

/// Who initiated the capability call (mirrors `claw_cap_caller_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CapabilityCaller {
    #[default]
    System,
    Agent,
    Console,
    SubAgent,
}

/// Rust view of `claw_cap_call_context_t` routing fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityContext {
    pub request_id: u32,
    pub session_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_chat_id: Option<String>,
    pub target_channel: Option<String>,
    pub target_chat_id: Option<String>,
    pub source_capability: Option<String>,
    pub caller: CapabilityCaller,
}

/// Minimal context for one LLM tool invocation (Layer 3 only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolContext {
    pub turn_id: u32,
    pub session_id: Option<String>,
}

impl ToolContext {
    pub fn session_id_str(&self) -> &str {
        self.session_id.as_deref().unwrap_or("")
    }

    /// Adapter for the legacy C capability surface.
    pub fn to_capability_context(&self) -> CapabilityContext {
        CapabilityContext {
            request_id: self.turn_id,
            session_id: self.session_id.clone(),
            caller: CapabilityCaller::Agent,
            ..Default::default()
        }
    }
}

impl CapabilityContext {
    /// Build an agent-originated context from a submitted turn (root/sub-agent
    /// caller refinement happens in the C backend via `cap_user_ctx`).
    pub fn agent_turn(
        request_id: u32,
        session_id: Option<String>,
        source_channel: Option<String>,
        source_chat_id: Option<String>,
        target_channel: Option<String>,
        target_chat_id: Option<String>,
        source_capability: Option<String>,
    ) -> Self {
        CapabilityContext {
            request_id,
            session_id,
            source_channel,
            source_chat_id,
            target_channel,
            target_chat_id,
            source_capability,
            caller: CapabilityCaller::Agent,
        }
    }
}
