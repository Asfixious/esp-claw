//! Event Router foundations.
//!
//! RPC, workflow, event, and component runtime foundations.
//!
//! The implemented [`rpc`] module provides a task-local registry with typed
//! Postcard messages and a raw binary escape hatch. Future workflow and runtime
//! layers remain separate modules inside this crate until they require an
//! independently reusable dependency boundary.

pub mod rpc;
