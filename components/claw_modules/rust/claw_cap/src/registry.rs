//! Capability registry (Rust-first with optional C fallback backend).

use std::collections::HashMap;
use std::sync::RwLock;

use crate::context::{CapabilityContext, ToolContext};
use crate::error::CapabilityError;
use crate::invoker::{CapabilityInvokeResult, CapabilityInvoker};

type RustHandler = Box<
    dyn Fn(&str, &ToolContext) -> Result<CapabilityInvokeResult, CapabilityError> + Send + Sync,
>;

/// Second-class backend for C-registered capabilities (`claw_cap.c`).
pub trait RegistryBackend: Send + Sync {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError>;
}

/// Rust-first registry: in-process handlers take priority; everything else
/// delegates to the optional C backend.
pub struct Registry {
    handlers: RwLock<HashMap<String, RustHandler>>,
    backend: Option<Box<dyn RegistryBackend>>,
}

impl Registry {
    pub fn new(backend: Option<Box<dyn RegistryBackend>>) -> Self {
        Registry {
            handlers: RwLock::new(HashMap::new()),
            backend,
        }
    }

    pub fn register_handler(
        &self,
        id_or_name: impl Into<String>,
        handler: RustHandler,
    ) -> Result<(), CapabilityError> {
        let mut handlers = self.handlers.write().map_err(|_| CapabilityError::Failed)?;
        handlers.insert(id_or_name.into(), handler);
        Ok(())
    }
}

impl CapabilityInvoker for Registry {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &ToolContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        if let Ok(handlers) = self.handlers.read() {
            if let Some(handler) = handlers.get(capability_name) {
                return handler(input_json, context);
            }
        }
        let Some(backend) = &self.backend else {
            return Err(CapabilityError::NotFound);
        };
        backend.invoke(capability_name, input_json, &context.to_capability_context())
    }
}
