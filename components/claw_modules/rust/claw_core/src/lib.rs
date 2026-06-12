//! `claw_core` — runtime primitives: orchestrator shell, channels, iteration loop.
//!
//! Layer 1: [`orchestrator::Orchestrator`]
//! Layer 3: [`iteration_loop::IterationLoop`]

#![allow(non_camel_case_types)]

pub mod channels;
pub mod llm_output;
pub mod iteration_loop;
pub mod memory;
pub mod observability;
pub mod orchestrator;
pub mod protocol;
pub mod request;
pub mod session;
pub mod skills;
pub mod tools;
pub mod util;

pub use orchestrator::Orchestrator;
pub use llm_output::{parse_and_validate, ValidatedLlmOutput};
pub use channels::{
    ChannelEgress, ChannelEgressHub, ChannelError, ChannelIngress, ChannelIngressSink,
    ChannelTransport, InboundCommand, InboundMessage, LocalChannelIngress, OutboundMessage,
    RecordingTransport, ReplyRoute,
};
pub use session::{
    DeliverError, SessionError, SessionMessage, SessionOut, SessionRecord, SessionStore,
};
pub use protocol::Command;
pub use skills::{
    ActiveSkillDoc, NoSkills, PromptSection, PromptSlot, SkillCatalogEntry, SkillError,
    SkillPrompt, Skills,
};
pub use iteration_loop::IterationTools;
pub use tools::{
    ToolCaller, ToolCatalog, ToolError, ToolInvocation, ToolOutput, Tools, TurnContext,
};
pub use protocol::{IdParseError, IterationId, SessionId, StepId, TaskId, WorkerId};
