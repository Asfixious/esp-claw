//! `claw_core` — multi-agent runtime built around [`iteration_loop`] (Layer 3).
//!
//! Layer 1: [`orchestrator::RunOrchestrator`]
//! Layer 2: [`agent_spec::AgentSpec`] + [`agents`]
//! Layer 3: [`iteration_loop::IterationLoop`]

#![allow(non_camel_case_types)]

pub mod agent_spec;
pub mod agents;
pub mod context;
pub mod llm_output;
pub mod iteration_loop;
pub mod memory;
pub mod observability;
pub mod orchestrator;
pub mod protocol;
pub mod agent_registry;
pub mod request;
pub mod runtime;
pub mod util;

pub use agent_spec::{
    apply_patches, AgentRole, AgentSpec, AgentState, ContextBuildInput, ContextBundle,
    FrontendPhase, OutputSchema, RunStatus, SemanticPhase, StatePatch, TransitionInput,
    TransitionOutput, TransitionSignal, WorkerPhase,
};
pub use agents::{FrontendAgentSpec, WorkerAgentSpec};
pub use orchestrator::{AgentInstance, OrchestratorConfig, RunOrchestrator};
pub use llm_output::{parse_and_validate, ValidatedLlmOutput};
pub use protocol::{IdParseError, SessionId, StepId, TaskId, WorkerId};
pub use agent_registry::AgentRegistry;
pub use runtime::{HarnessIterationOutput, InstanceControl, run_iteration};
