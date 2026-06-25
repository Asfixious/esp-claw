//! Capability groups (plugins), mirroring `claw_cap_group_t`.

use std::sync::Arc;

use crate::descriptor::{CapabilityDescriptor, CapabilityState};
use crate::error::CapabilityError;

/// Group-level lifecycle hooks (mirrors the C `group_init`/`group_start`/
/// `group_stop` function pointers). All default to no-ops.
pub trait GroupHooks: Send + Sync {
    /// Called once when the group is registered.
    fn init(&self) -> Result<(), CapabilityError> {
        Ok(())
    }

    /// Called when the group is enabled/started, before member `start`s.
    fn start(&self) -> Result<(), CapabilityError> {
        Ok(())
    }

    /// Called when the group is disabled/unregistered, after member `stop`s.
    fn stop(&self) -> Result<(), CapabilityError> {
        Ok(())
    }
}

/// A registrable group of capabilities (mirrors `claw_cap_group_t`; the C-only
/// `plugin_ctx` is dropped). Members are validated and registered together.
#[derive(Clone)]
pub struct CapabilityGroup {
    pub group_id: String,
    pub plugin_name: String,
    pub version: String,
    pub descriptors: Vec<CapabilityDescriptor>,
    pub hooks: Option<Arc<dyn GroupHooks>>,
}

impl CapabilityGroup {
    /// Creates a group from an id, plugin name, version, and its descriptors.
    pub fn new(
        group_id: impl Into<String>,
        plugin_name: impl Into<String>,
        version: impl Into<String>,
        descriptors: impl IntoIterator<Item = CapabilityDescriptor>,
    ) -> Self {
        Self {
            group_id: group_id.into(),
            plugin_name: plugin_name.into(),
            version: version.into(),
            descriptors: descriptors.into_iter().collect(),
            hooks: None,
        }
    }

    /// Attaches group-level lifecycle hooks.
    pub fn with_hooks(mut self, hooks: Arc<dyn GroupHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }
}

/// Summary of a registered group, returned by `list_groups`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupInfo {
    pub group_id: String,
    pub plugin_name: String,
    pub version: String,
    pub state: CapabilityState,
    pub descriptor_count: usize,
}
