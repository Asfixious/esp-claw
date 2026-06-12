//! `claw_core` — multi-agent runtime built around [`iteration_loop`] (Layer 3).
//!
//! Layer 1: [`orchestrator::RunOrchestrator`]
//! Layer 2: [`agent`]
//! Layer 3: [`iteration_loop::IterationLoop`]

#![allow(non_camel_case_types)]

pub mod agent;
pub mod context;
pub mod llm_output;
pub mod iteration_loop;
pub mod memory;
pub mod observability;
pub mod orchestrator;
pub mod protocol;
pub mod request;
pub mod runtime;
pub mod util;

pub use agent::{
    apply_patches, register_builtins, AgentRegistrar, AgentRegistry, AgentRole, AgentSpec,
    AgentState, ContextBuildInput, ContextBundle, FrontendAgentSpec, FrontendPhase, OutputSchema,
    RunStatus, SemanticPhase, StatePatch, TransitionInput, TransitionOutput, TransitionSignal,
    WorkerAgentSpec, WorkerPhase,
};
pub use orchestrator::{AgentInstance, OrchestratorConfig, RunOrchestrator};
pub use llm_output::{parse_and_validate, ValidatedLlmOutput};
pub use claw_message_channel::{
    InboundMessage, MessageChannel, MessageChannelHub, MessageError, OutboundMessage, ReplyRoute,
};
pub use protocol::{IdParseError, SessionId, StepId, TaskId, TurnId, WorkerId};
pub use runtime::{HarnessIterationOutput, InstanceControl, run_iteration};
