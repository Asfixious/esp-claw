//! The outbound event type and the `EventPublisher` injection trait.
//!
//! `claw_core` builds [`ClawEvent`]s and hands them to an [`EventPublisher`].
//! The espidf wiring converts a [`ClawEvent`] into the C `claw_event_t` and
//! forwards to `claw_event_router_publish`; host tests record events instead.

use crate::error::EspErr;

/// Mirror of `claw_session_policy_t` from `claw_session_mgr.h`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPolicy {
    Chat = 0,
    Trigger = 1,
    Global = 2,
    Ephemeral = 3,
    NoSave = 4,
}

/// Rust-owned representation of the fields `claw_core` populates on a
/// `claw_event_t`. String fields are truncated to the C buffer sizes at the
/// publish boundary, not here.
#[derive(Clone, Debug)]
pub struct ClawEvent {
    pub event_id: String,
    pub source_cap: String,
    pub event_type: String,
    pub source_channel: String,
    pub chat_id: String,
    pub message_id: String,
    pub correlation_id: String,
    pub content_type: String,
    pub timestamp_ms: i64,
    pub session_policy: SessionPolicy,
    pub text: Option<String>,
    pub payload_json: Option<String>,
}

impl Default for ClawEvent {
    fn default() -> Self {
        ClawEvent {
            event_id: String::new(),
            source_cap: String::new(),
            event_type: String::new(),
            source_channel: String::new(),
            chat_id: String::new(),
            message_id: String::new(),
            correlation_id: String::new(),
            content_type: String::new(),
            timestamp_ms: 0,
            session_policy: SessionPolicy::Chat,
            text: None,
            payload_json: None,
        }
    }
}

/// Injection point for publishing events to the (still-C) event router.
pub trait EventPublisher: Send + Sync {
    /// Equivalent to `claw_event_router_publish(&event)`.
    fn publish(&self, event: &ClawEvent) -> EspErr;
}
