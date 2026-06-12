//! [`AgentSpec`] registration by role.

use std::collections::HashMap;
use std::sync::Arc;

use super::{AgentRole, AgentSpec};

/// Registers one [`AgentSpec`] implementation into a registry.
pub trait AgentRegistrar: Send + Sync {
    fn register(&self, registry: &mut AgentRegistry);
}

/// Maps each [`AgentRole`] to its semantic controller implementation.
#[derive(Default)]
pub struct AgentRegistry {
    specs: HashMap<AgentRole, Arc<dyn AgentSpec>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry with all agents registered via [`crate::agent::register_builtins`].
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        crate::agent::register_builtins(&mut registry);
        registry
    }

    pub fn register(&mut self, spec: Arc<dyn AgentSpec>) {
        self.specs.insert(spec.role(), spec);
    }

    pub fn register_all(&mut self, registrars: &[&dyn AgentRegistrar]) {
        for registrar in registrars {
            registrar.register(self);
        }
    }

    pub fn get(&self, role: AgentRole) -> Option<Arc<dyn AgentSpec>> {
        self.specs.get(&role).cloned()
    }
}
