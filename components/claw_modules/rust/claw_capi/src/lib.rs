//! `claw_capi` — the single C ABI layer between the pure-Rust claw modules and
//! the firmware's C callers.
//!
//! Every `#[no_mangle] extern "C"` export, `#[repr(C)]` ABI struct, C
//! function-pointer typedef, C-callback-to-Rust-trait adapter, and
//! `Result -> esp_err_t` mapping lives in this crate. The logic crates
//! (`claw_core`, `claw_memory`, `claw_paths`, `claw_task`) are pure Rust and
//! never reference the C ABI; this crate depends on them and wraps their
//! idiomatic Rust APIs.

#![allow(non_camel_case_types)]
// Several C-callback adapters are only instantiated by the espidf create paths;
// off-target they exist for their trait impls but are otherwise unused.
#![cfg_attr(not(target_os = "espidf"), allow(dead_code))]

pub mod abi_types;
pub mod cap_skills;
pub mod capability;
#[cfg(feature = "legacy-core")]
pub mod core_abi;
pub mod errmap;
pub mod orchestrator_abi;

pub use cap_skills::CLegacySkills as CapSkills;
pub mod ffi;
#[cfg(feature = "legacy-core")]
pub mod llm;
#[cfg(feature = "legacy-core")]
pub mod memory_abi;
pub mod memory_cap;
pub mod paths;
pub mod task;
