//! `claw_core` — agent core, port of the `claw_core` C component.
//!
//! Pure Rust: the iteration loop, context building, request/response plumbing,
//! and the LLM runtime live here as idiomatic Rust APIs. The C ABI (`claw_core.h`
//! exports, `#[repr(C)]` structs, and C-callback adapters) lives separately in
//! the `claw_capi` crate, which depends on and wraps these APIs.

#![allow(non_camel_case_types)]

pub mod callbacks;
pub mod channel;
pub mod consts;
pub mod core;
pub mod agent_mailbox;
pub mod iteration_loop;
pub mod errname;
pub mod error;
pub mod events;
pub mod request;
pub mod response;
pub mod util;
