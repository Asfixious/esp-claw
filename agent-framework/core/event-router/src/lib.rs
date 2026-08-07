//! Event Router foundations.
//!
//! RPC, workflow, event, and component runtime foundations.
//!
//! The implemented [`rpc`] module provides a task-local registry with typed
//! fixed-layout Zerocopy messages and a raw binary escape hatch. Future workflow
//! and runtime layers remain separate modules inside this crate until they
//! require an independently reusable dependency boundary.

pub mod rpc;
