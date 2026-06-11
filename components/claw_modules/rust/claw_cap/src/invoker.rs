//! Capability invoke trait and result type.

use crate::context::CapabilityContext;
use crate::error::CapabilityError;

/// Outcome of one capability invocation for the agent tool loop.
///
/// Mirrors the C contract: `output` is always model-visible text; `ok` marks
/// transport/handler failure (`is_error` on the tool message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityInvokeResult {
    pub output: String,
    pub ok: bool,
}

/// Dispatches a named capability. Rust handlers implement this directly; C
/// descriptors are adapted in `claw_capi::capability`.
pub trait CapabilityInvoker: Send + Sync {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError>;
}
