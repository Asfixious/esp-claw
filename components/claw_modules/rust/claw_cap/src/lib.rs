//! `claw_cap` — capability dispatch, Rust-first.
//!
//! Pure Rust: call context, invoke results, and the [`CapabilityInvoker`] trait.
//! The C capability registry (`claw_cap.c`) and `esp_err_t` execute trampolines
//! are second-class: they live in `claw_capi::capability` and adapt into this API.

pub mod context;
pub mod error;
pub mod invoker;
pub mod registry;

pub use context::{CapabilityCaller, CapabilityContext};
pub use error::CapabilityError;
pub use invoker::{CapabilityInvokeResult, CapabilityInvoker};
pub use registry::{Registry, RegistryBackend};
