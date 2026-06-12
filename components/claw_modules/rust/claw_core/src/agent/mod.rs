//! Layer 2 agent: contract, registry, and built-in role implementations.

mod agents;
mod patch;
mod registry;
mod role;
mod spec;
mod state;
mod transition;

pub use patch::{apply_patches, StatePatch};
pub use registry::{AgentRegistrar, AgentRegistry};
pub use role::{
    AgentRole, FrontendPhase, RunStatus, SemanticPhase, WorkerPhase,
};
pub use spec::{
    AgentSpec, ContextBuildInput, ContextBundle, OutputSchema, ToolVisibility,
};
pub use state::{
    AgentState, FrontendState, RoleState, WorkerState,
};
pub use transition::{TransitionInput, TransitionOutput, TransitionSignal};

pub use agents::{FrontendAgentSpec, WorkerAgentSpec};

/// Static registrars for built-in agents. Extend this list when adding roles.
pub fn builtin_registrars() -> &'static [&'static dyn AgentRegistrar] {
    agents::builtin_registrars()
}

/// Register built-in agent specs via [`AgentRegistrar`] callbacks.
pub fn register_builtins(registry: &mut AgentRegistry) {
    agents::register_builtins(registry);
}

/// Register a custom set of agent modules (tests or firmware overlays).
pub fn register_modules(registry: &mut AgentRegistry, registrars: &[&dyn AgentRegistrar]) {
    registry.register_all(registrars);
}
