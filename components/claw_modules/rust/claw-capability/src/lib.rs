//! `claw-capability` — capability registry and dispatch, Rust-first.
//!
//! A pure-Rust reimplementation of the C `claw_cap` subsystem: capability
//! descriptors and metadata, capability groups, the lifecycle state machine,
//! global and per-session LLM visibility, capability dispatch, and the LLM
//! tool-list / catalog renderers. All state is owned by an instance
//! [`Registry`] (replacing the C `s_runtime` global). Capabilities not owned by
//! the registry can be reached through an optional [`RegistryBackend`] (the
//! seam to the legacy C registry during migration).
//!
//! # Overview
//!
//! - [`CapabilityContext`] / [`CapabilityCaller`] mirror `claw_cap_call_context_t`.
//! - [`CapabilityDescriptor`] (+ [`CapabilityKind`], [`CapabilityFlags`],
//!   [`CapabilityHandler`]) and [`CapabilityGroup`] mirror the C descriptor /
//!   group structs; the C function pointers become the handler trait.
//! - [`CapabilityState`] mirrors `claw_cap_state_t`.
//! - [`Registry`] exposes registration, lifecycle, visibility, dispatch,
//!   listing, and the `build_llm_tools_json` / `build_catalog` renderers.
//!
//! # Examples
//!
//! Register a capability and call it through the [`CapabilityInvoker`] trait:
//!
//! ```
//! use std::sync::Arc;
//!
//! use claw_capability::{
//!     CapabilityCaller, CapabilityContext, CapabilityDescriptor, CapabilityFlags,
//!     CapabilityHandler, CapabilityInvokeResult, CapabilityInvoker, CapabilityError, Registry,
//! };
//!
//! struct Echo;
//! impl CapabilityHandler for Echo {
//!     fn execute(
//!         &self,
//!         input_json: &str,
//!         _context: &CapabilityContext,
//!     ) -> Result<CapabilityInvokeResult, CapabilityError> {
//!         Ok(CapabilityInvokeResult { output: input_json.to_string(), ok: true })
//!     }
//! }
//!
//! let registry = Registry::new(None);
//! registry
//!     .register(
//!         CapabilityDescriptor::new("echo", "echo", Arc::new(Echo))
//!             .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
//!     )
//!     .expect("register echo");
//! registry.start_all().expect("start");
//!
//! let context = CapabilityContext {
//!     caller: CapabilityCaller::Agent,
//!     ..Default::default()
//! };
//! let result = registry
//!     .invoke("echo", r#"{"hello":"world"}"#, &context)
//!     .expect("echo invocation");
//! assert!(result.ok);
//! assert_eq!(result.output, r#"{"hello":"world"}"#);
//! ```

pub mod context;
pub mod descriptor;
pub mod error;
pub mod group;
pub mod invoker;
pub mod registry;
pub mod tools;

pub use context::{CapabilityCaller, CapabilityContext};
pub use descriptor::{
    CapabilityDescriptor, CapabilityFlags, CapabilityHandler, CapabilityKind, CapabilityState,
    DescriptorRuntimeInfo, DescriptorSnapshot,
};
pub use error::CapabilityError;
pub use group::{CapabilityGroup, GroupHooks, GroupInfo};
pub use invoker::{CapabilityInvokeResult, CapabilityInvoker};
pub use registry::{Registry, RegistryBackend};
