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
///
/// [`Registry`](crate::Registry) is the in-process implementation, but callers
/// depend on this trait so the backend (Rust handlers vs. the C registry) can be
/// swapped without touching the agent tool loop.
///
/// # Examples
///
/// ```
/// use claw_capability::{
///     CapabilityContext, CapabilityError, CapabilityInvokeResult, CapabilityInvoker,
/// };
///
/// /// An invoker that always answers with a fixed greeting.
/// struct GreetingInvoker;
///
/// impl CapabilityInvoker for GreetingInvoker {
///     fn invoke(
///         &self,
///         capability_name: &str,
///         _input_json: &str,
///         _context: &CapabilityContext,
///     ) -> Result<CapabilityInvokeResult, CapabilityError> {
///         match capability_name {
///             "greet" => Ok(CapabilityInvokeResult { output: "hi".to_string(), ok: true }),
///             _ => Err(CapabilityError::NotFound),
///         }
///     }
/// }
///
/// let invoker = GreetingInvoker;
/// let result = invoker
///     .invoke("greet", "{}", &CapabilityContext::default())
///     .expect("greet");
/// assert_eq!(result.output, "hi");
/// ```
pub trait CapabilityInvoker: Send + Sync {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError>;
}
