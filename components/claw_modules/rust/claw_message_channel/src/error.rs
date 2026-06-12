//! Message channel errors.

use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum MessageError {
    #[error("channel not found: {0}")]
    ChannelNotFound(String),
    #[error("no reply route for session: {0}")]
    SessionRouteNotFound(String),
    #[error("channel send failed: {0}")]
    SendFailed(String),
}
