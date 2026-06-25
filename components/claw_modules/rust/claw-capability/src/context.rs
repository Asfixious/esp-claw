//! Per-invocation routing and identity passed to capability handlers.

/// Who initiated the capability call (mirrors `claw_cap_caller_t`).
///
/// The C enum's `CLAW_CAP_CALLER_ROOT_AGENT` is an alias for
/// `CLAW_CAP_CALLER_AGENT`; this enum keeps the single [`Agent`] variant and
/// leaves root-vs-sub refinement to the dispatch path.
///
/// [`Agent`]: CapabilityCaller::Agent
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CapabilityCaller {
    #[default]
    System,
    Agent,
    Console,
    SubAgent,
}

/// Rust view of `claw_cap_call_context_t`.
///
/// Mirrors the C call context field-for-field, except the `core` backpointer
/// (`claw_core_handle_t`), which is an FFI concern injected at the `claw_capi`
/// boundary and intentionally absent from this pure-Rust crate.
///
/// All fields are public and the type is [`Default`], so it is built by
/// zero-initializing and setting only the relevant fields — matching how
/// `claw_cap_call_from_core` fills `claw_cap_call_context_t` in C.
///
/// # Examples
///
/// ```
/// use claw_capability::{CapabilityCaller, CapabilityContext};
///
/// let context = CapabilityContext {
///     request_id: 42,
///     session_id: Some("sess-1".to_string()),
///     source_channel: Some("telegram".to_string()),
///     caller: CapabilityCaller::Agent,
///     ..Default::default()
/// };
///
/// assert_eq!(context.caller, CapabilityCaller::Agent);
/// assert!(context.target_chat_id.is_none());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityContext {
    pub request_id: u32,
    pub session_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_chat_id: Option<String>,
    pub target_channel: Option<String>,
    pub target_chat_id: Option<String>,
    pub source_capability: Option<String>,
    pub correlation_id: Option<String>,
    pub caller: CapabilityCaller,
}
