//! Internal owned response item, port of `claw_core_response_item_t` /
//! `claw_core_response_t`.

use crate::consts::{CompletionType, ResponseStatus};

/// Owned response produced by the agent loop and delivered to receivers.
#[derive(Clone, Debug)]
pub struct ResponseItem {
    pub request_id: u32,
    pub status: ResponseStatus,
    pub completion_type: CompletionType,
    pub target_channel: Option<String>,
    pub target_chat_id: Option<String>,
    pub text: Option<String>,
    pub error_message: Option<String>,
}

impl Default for ResponseItem {
    fn default() -> Self {
        ResponseItem {
            request_id: 0,
            status: ResponseStatus::Ok,
            completion_type: CompletionType::Done,
            target_channel: None,
            target_chat_id: None,
            text: None,
            error_message: None,
        }
    }
}

impl ResponseItem {
    /// Text used for an outbound message: the final text on success, otherwise
    /// the error message (mirrors `build_agent_out_message_event`).
    pub fn outbound_text(&self) -> Option<&str> {
        let t = match self.status {
            ResponseStatus::Ok => self.text.as_deref(),
            ResponseStatus::Error => self.error_message.as_deref(),
        };
        t.filter(|s| !s.is_empty())
    }
}
