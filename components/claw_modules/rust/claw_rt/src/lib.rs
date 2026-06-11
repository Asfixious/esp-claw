//! `claw_rt` — the firmware Rust runtime aggregator.
//!
//! This crate contains no logic of its own. It exists solely to pull every
//! migrated `claw_modules` Rust crate into a single static archive so that
//! their `#[no_mangle] extern "C"` symbols (the `claw_core_*`, `claw_memory_*`,
//! `cap_llm_inspect_*`, `claw_paths_*`, and `claw_task_*` C ABIs) are emitted
//! exactly once. Building each crate as its own staticlib instead would emit the
//! shared upstream object code (and these symbols) into several archives and the
//! final link would fail with duplicate-symbol errors.
//!
//! The ESP-IDF `claw_core` component links the resulting `libclaw_rt.a`; the
//! `claw_memory` and `cap_llm_inspect` components are header-only and depend on
//! `claw_core` for the symbols.

#![allow(unused_imports)]

// Every #[no_mangle] C-ABI symbol for the claw modules now originates in the
// single `claw_capi` crate, which depends on (and pulls in) the pure-Rust logic
// crates. Re-exporting just `claw_capi` links all of those symbols into
// libclaw_rt.a exactly once.
pub extern crate claw_capi;
