//! Single outbound channel adapter.

use crate::envelope::OutboundMessage;
use crate::error::MessageError;

/// Delivers [`OutboundMessage`] to one external transport (CLI, IM, web, etc.).
pub trait MessageChannel: Send + Sync {
    fn id(&self) -> &str;

    fn send(&self, msg: &OutboundMessage) -> Result<(), MessageError>;
}
