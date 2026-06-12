//! `claw_cap` — capability dispatch, Rust-first.
//!
//! Pure Rust: call context, invoke results, and the [`CapabilityInvoker`] trait.
//! The C capability registry (`claw_cap.c`) and `esp_err_t` execute trampolines
//! are second-class: they live in `claw_capi::capability` and adapt into this API.

pub mod c_bridge;
pub mod channel_adapters;
pub mod context;
pub mod error;
pub mod invoker;
pub mod registry;
pub mod skills_adapter;
pub mod tools_adapter;

pub use context::{CapabilityCaller, CapabilityContext, ToolContext};
pub use error::CapabilityError;
pub use invoker::{CapabilityInvokeResult, CapabilityInvoker};
pub use registry::{Registry, RegistryBackend};
pub use c_bridge::{capability_to_turn, turn_to_capability_context};
pub use channel_adapters::{CapChannelIngress, CapChannelTransport};
pub use skills_adapter::{EmptySkills, StaticSkills};
pub use tools_adapter::{
    EmptyToolCatalog, InvokerTools, InvokerToolsDyn, StaticToolCatalog, ToolCatalogSource,
};

/// C-backed tools port alias: object-safe [`Tools`] over [`CapabilityInvoker`].
pub type CapTools = InvokerToolsDyn;
