//! Owned inbound/outbound message envelopes.

/// Reply routing snapshot for one agent session (updated on each inbound).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyRoute {
    pub channel: String,
    pub chat_id: String,
    pub sender_id: Option<String>,
    pub reply_to_message_id: Option<String>,
}

impl ReplyRoute {
    pub fn from_inbound(msg: &InboundMessage) -> Self {
        Self {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            sender_id: msg.sender_id.clone(),
            reply_to_message_id: Some(msg.message_id.clone()),
        }
    }
}

/// User or IM message submitted from an external channel adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    pub message_id: String,
    /// Routing key matching a registered [`super::channel::MessageChannel`] id.
    pub channel: String,
    pub chat_id: String,
    pub sender_id: Option<String>,
    /// Agent session resolved by the channel adapter before submit.
    pub session_id: String,
    pub text: String,
}

/// Agent reply routed back through a channel adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub text: String,
    pub reply_to_message_id: Option<String>,
}
