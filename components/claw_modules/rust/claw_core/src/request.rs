//! Internal owned request item, port of `claw_core_request_item_t`.
//!
//! In C this carries a `claw_core_request_t view` plus the heap copies its
//! `const char *` fields point at. In Rust the owned `String`s *are* the
//! storage; the C ABI layer rebuilds a borrowed view at the boundary.

use claw_cap::CapabilityContext;

/// Owned copy of a submitted request.
#[derive(Clone, Debug, Default)]
pub struct RequestItem {
    pub request_id: u32,
    pub flags: u32,
    pub session_id: Option<String>,
    pub user_text: String,
    pub source_channel: Option<String>,
    pub source_chat_id: Option<String>,
    pub source_sender_id: Option<String>,
    pub source_message_id: Option<String>,
    pub source_cap: Option<String>,
    pub target_channel: Option<String>,
    pub target_chat_id: Option<String>,
}

impl RequestItem {
    /// Effective session id, empty string when unset (matches the C reads of
    /// `view.session_id` guarded by `[0] == '\0'`).
    pub fn session_id_str(&self) -> &str {
        self.session_id.as_deref().unwrap_or("")
    }

    /// Routing context for a capability invocation on this turn.
    pub fn capability_context(&self) -> CapabilityContext {
        CapabilityContext::agent_turn(
            self.request_id,
            self.session_id.clone(),
            self.source_channel.clone(),
            self.source_chat_id.clone(),
            self.target_channel.clone(),
            self.target_chat_id.clone(),
            self.source_cap.clone(),
        )
    }
}
