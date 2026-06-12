//! Multi-channel message routing for agent ingress and egress.
//!
//! External adapters register [`MessageChannel`] implementations on a
//! [`MessageChannelHub`]. Inbound text is buffered for the orchestrator;
//! per-session reply routes are updated on each inbound so
//! [`MessageChannelHub::send_to_session`] replies on the same channel/chat.

mod channel;
mod envelope;
mod error;
mod hub;

pub use channel::MessageChannel;
pub use envelope::{InboundMessage, OutboundMessage, ReplyRoute};
pub use error::MessageError;
pub use hub::MessageChannelHub;
