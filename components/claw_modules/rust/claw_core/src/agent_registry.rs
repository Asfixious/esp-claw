//! [`AgentSpec`] registration by role.

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent_spec::{AgentRole, AgentSpec};

/// Maps each [`AgentRole`] to its semantic controller implementation.
#[derive(Default)]
pub struct AgentRegistry {
    specs: HashMap<AgentRole, Arc<dyn AgentSpec>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, spec: Arc<dyn AgentSpec>) {
        self.specs.insert(spec.role(), spec);
    }

    pub fn get(&self, role: AgentRole) -> Option<Arc<dyn AgentSpec>> {
        self.specs.get(&role).cloned()
    }
}
