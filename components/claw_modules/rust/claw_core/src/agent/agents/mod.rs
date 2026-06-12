//! Built-in role-specific [`crate::agent::AgentSpec`] implementations.

use crate::agent::{AgentRegistrar, AgentRegistry};

mod frontend;
mod worker;

pub use frontend::FrontendAgentSpec;
pub use worker::WorkerAgentSpec;

/// Static registrars for built-in agents. Extend this list when adding roles.
pub fn builtin_registrars() -> &'static [&'static dyn AgentRegistrar] {
    &[
        &frontend::Registrar,
        &worker::Registrar,
    ]
}

/// Register built-in agent specs via [`AgentRegistrar`] callbacks.
pub fn register_builtins(registry: &mut AgentRegistry) {
    registry.register_all(builtin_registrars());
}
