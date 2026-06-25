//! `claw_core` — runtime primitives: orchestrator shell, channels, iteration loop.
//!
//! Layer 1: [`Orchestrator`]
//! Layer 3: [`iteration_loop::IterationLoop`]

#![allow(non_camel_case_types)]

pub mod agent;
pub mod channels;
pub mod iteration_loop;
mod orchestrator;
mod orchestrator_instance;
pub mod protocol;
pub mod session;
pub mod tools;

pub use channels::{
    ChannelEgress, ChannelEgressHub, ChannelError, ChannelIngress, ChannelIngressSink,
    ChannelTransport, InboundCommand, InboundMessage, LocalChannelIngress, OutboundMessage,
    RecordingTransport, ReplyRoute,
};
pub use claw_utils::define_prefixed_id;
pub use orchestrator::{
    ChannelsEgressOnly, ChannelsUnset, FactorySet, FactoryUnset, Orchestrator, OrchestratorBuilder,
};
pub use protocol::Command;
pub use protocol::{IdParseError, IterationId, StepId, TaskId, WorkerId};
pub use session::{
    DeliverError, SessionError, SessionId, SessionMessage, SessionOut, SessionRecord, SessionStore,
};
// Skills moved to the standalone `claw-skill` crate; re-exported here so
// `claw_core::Skill*` stays the stable surface for existing callers.
pub use claw_skill::{
    FsSkillRegistry, SkillError, SkillGroup, SkillId, SkillMetadata, SkillRegistry, SkillSet,
};
pub use tools::{
    AllowedTools, Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput, ToolSet,
    ToolSetError, DEFAULT_TOOL_GROUP,
};
