//! `claw_core` — runtime primitives: orchestrator shell, channels, iteration loop.
//!
//! Layer 1: [`orchestrator::Orchestrator`]
//! Layer 3: [`iteration_loop::IterationLoop`]

#![allow(non_camel_case_types)]

pub mod agent;
pub mod channels;
pub mod iteration_loop;
pub mod observability;
pub mod orchestrator;
pub mod protocol;
pub mod request;
pub mod session;
pub mod skills;
pub mod tools;

pub use claw_utils::define_prefixed_id;
pub use orchestrator::Orchestrator;
pub use channels::{
    ChannelEgress, ChannelEgressHub, ChannelError, ChannelIngress, ChannelIngressSink,
    ChannelTransport, InboundCommand, InboundMessage, LocalChannelIngress, OutboundMessage,
    RecordingTransport, ReplyRoute,
};
pub use session::{
    DeliverError, SessionError, SessionId, SessionMessage, SessionOut, SessionRecord, SessionStore,
};
pub use protocol::Command;
pub use skills::{
    FsSkillRegistry, ManageMode, SkillError, SkillGroup, SkillId, SkillMetadata, SkillRegistry,
    SkillSet,
};
pub use tools::{
    AllowedTools, Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput, ToolSet,
    ToolSetError, DEFAULT_TOOL_GROUP,
};
pub use protocol::{IdParseError, IterationId, StepId, TaskId, WorkerId};
