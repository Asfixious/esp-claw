//! Event Router foundations.
//!
//! RPC, workflow, event, and component runtime foundations.
//!
//! The implemented [`rpc`] module provides one task-local typed registry with
//! both compile-time Method calls and runtime-addressed writer/reader calls.
//! Future workflow and runtime layers remain separate modules inside this crate
//! until they require an independently reusable dependency boundary.

pub mod rpc;
