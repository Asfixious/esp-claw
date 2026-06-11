//! `claw_memory` — long-term + session memory, a faithful Rust port of the
//! `claw_memory` ESP-IDF component. This crate is pure Rust: it exposes the
//! memory logic as idiomatic Rust APIs. The `claw_memory.h` C ABI
//! (`#[no_mangle] extern "C"` functions, the by-value `claw_memory_item_t`
//! struct, the context-provider statics, and the capability registration) lives
//! in the `claw_capi` crate, which depends on and wraps these APIs. The crate is
//! an `rlib`.

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

pub mod api;
pub mod cap;
pub mod consts;
pub mod error;
pub mod extract;
pub mod item;
pub mod lightweight;
pub mod profile;
pub mod session;
mod state;
mod storage;
pub mod util;
