//! `claw_core` — agent core, port of the `claw_core` C component.
//!
//! Work in progress. Modules are ported incrementally; each exports the matching
//! slice of the `claw_core.h` C ABI as `#[no_mangle] extern "C"`.

#![allow(non_camel_case_types)]

// Re-export the sibling C-ABI crates so rustc links their objects (and the
// `#[no_mangle]` `claw_paths_*` / `claw_task_*` exports) into this staticlib.
// Without a reference an unused dependency would be dropped before linking.
pub extern crate claw_paths;
pub extern crate claw_task;

pub mod cabi;
pub mod callbacks;
pub mod channel;
pub mod consts;
pub mod context;
pub mod core;
pub mod core_state;
pub mod errname;
pub mod events;
pub mod request;
pub mod response;
pub mod util;
