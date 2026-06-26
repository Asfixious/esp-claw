//! Capability registry: owns descriptors, groups, lifecycle state, and LLM
//! visibility, mirroring the C `s_runtime` in `claw_cap.c` as instance state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::context::{CapabilityCaller, CapabilityContext};
use crate::descriptor::{
    CapabilityDescriptor, CapabilityHandler, CapabilityState, DescriptorRuntimeInfo,
    DescriptorSnapshot,
};
use crate::error::CapabilityError;
use crate::group::{CapabilityGroup, GroupHooks, GroupInfo};
use crate::invoker::{CapabilityInvokeResult, CapabilityInvoker};

/// Poll interval while draining in-flight calls during unregister.
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// Second-class backend for capabilities not owned by this registry (e.g. the
/// legacy C registry reached over FFI).
///
/// A [`Registry`] tries its own descriptors first and only delegates to this
/// backend on a miss, so it is the seam through which C-implemented
/// capabilities are reached during migration.
pub trait RegistryBackend: Send + Sync {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError>;
}

struct DescriptorEntry {
    descriptor: CapabilityDescriptor,
    state: CapabilityState,
    active_calls: u32,
    group_id: String,
    init_called: bool,
}

struct GroupEntry {
    plugin_name: String,
    version: String,
    hooks: Option<Arc<dyn GroupHooks>>,
    member_ids: Vec<String>,
    state: CapabilityState,
}

#[derive(Default)]
struct RegistryState {
    groups: HashMap<String, GroupEntry>,
    descriptors: HashMap<String, DescriptorEntry>,
    name_to_id: HashMap<String, String>,
    global_visible_groups: Vec<String>,
    session_visibility: HashMap<String, Vec<String>>,
    started: bool,
}

impl RegistryState {
    fn resolve_id(&self, id_or_name: &str) -> Option<String> {
        if self.descriptors.contains_key(id_or_name) {
            return Some(id_or_name.to_string());
        }
        self.name_to_id.get(id_or_name).cloned()
    }

    fn set_group_and_members_state(&mut self, group_id: &str, state: CapabilityState) {
        let member_ids = match self.groups.get_mut(group_id) {
            Some(group) => {
                group.state = state;
                group.member_ids.clone()
            }
            None => return,
        };
        for id in member_ids {
            if let Some(entry) = self.descriptors.get_mut(&id) {
                entry.state = state;
            }
        }
    }

    fn group_has_active_calls(&self, group_id: &str) -> bool {
        let Some(group) = self.groups.get(group_id) else {
            return false;
        };
        group
            .member_ids
            .iter()
            .filter_map(|id| self.descriptors.get(id))
            .any(|entry| entry.active_calls > 0)
    }

    /// Whether a group is LLM-visible for `session_id` (global list empty and no
    /// session entry => all visible; otherwise membership in either list).
    fn group_is_llm_visible(&self, group_id: &str, session_id: Option<&str>) -> bool {
        let session_entry = session_id.and_then(|s| self.session_visibility.get(s));
        if self.global_visible_groups.is_empty() && session_entry.is_none() {
            return true;
        }
        if self.global_visible_groups.iter().any(|g| g == group_id) {
            return true;
        }
        session_entry.is_some_and(|groups| groups.iter().any(|g| g == group_id))
    }

    /// Full LLM-visibility gate for a descriptor (mirrors `claw_cap_is_llm_visible`).
    fn is_llm_visible(
        &self,
        descriptor_id: &str,
        session_id: Option<&str>,
        caller: CapabilityCaller,
    ) -> bool {
        let Some(entry) = self.descriptors.get(descriptor_id) else {
            return false;
        };
        if !entry.state.is_available() {
            return false;
        }
        use crate::descriptor::CapabilityKind;
        if !matches!(
            entry.descriptor.kind,
            CapabilityKind::Callable | CapabilityKind::Hybrid
        ) {
            return false;
        }
        use crate::descriptor::CapabilityFlags;
        if !entry
            .descriptor
            .flags
            .contains(CapabilityFlags::CALLABLE_BY_LLM)
        {
            return false;
        }
        if caller == CapabilityCaller::SubAgent
            && entry
                .descriptor
                .flags
                .contains(CapabilityFlags::ROOT_AGENT_ONLY)
        {
            return false;
        }
        self.group_is_llm_visible(&entry.group_id, session_id)
    }
}

impl CapabilityState {
    /// Available for listing/calling: registered or started.
    pub(crate) fn is_available(self) -> bool {
        matches!(self, CapabilityState::Registered | CapabilityState::Started)
    }
}

/// Capability registry: descriptors, groups, lifecycle, and LLM visibility.
///
/// Construct with [`Registry::new`] (or [`Default`]); register capabilities via
/// [`register`](Registry::register) / [`register_group`](Registry::register_group),
/// then call them through [`CapabilityInvoker::invoke`] or [`call`](Registry::call).
pub struct Registry {
    inner: Mutex<RegistryState>,
    backend: Option<Box<dyn RegistryBackend>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Registry {
    /// Creates a registry with an optional fallback backend for capabilities it
    /// does not own. Pass `None` for a pure-Rust registry where unknown
    /// capabilities resolve to [`CapabilityError::NotFound`].
    pub fn new(backend: Option<Box<dyn RegistryBackend>>) -> Self {
        Self {
            inner: Mutex::new(RegistryState::default()),
            backend,
        }
    }

    fn state(&self) -> MutexGuard<'_, RegistryState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // --- Registration ---------------------------------------------------

    /// Registers a single capability as a one-member group (`version = "1"`),
    /// mirroring `claw_cap_register`.
    pub fn register(&self, descriptor: CapabilityDescriptor) -> Result<(), CapabilityError> {
        let group = CapabilityGroup::new(
            descriptor.id.clone(),
            descriptor.name.clone(),
            "1",
            [descriptor],
        );
        self.register_group(group)
    }

    /// Registers a closure as a callable, LLM-visible capability under
    /// `id_or_name`. Convenience over [`register`](Registry::register).
    pub fn register_handler(
        &self,
        id_or_name: impl Into<String>,
        handler: impl Fn(&str, &CapabilityContext) -> Result<CapabilityInvokeResult, CapabilityError>
            + Send
            + Sync
            + 'static,
    ) -> Result<(), CapabilityError> {
        use crate::descriptor::CapabilityFlags;
        let id = id_or_name.into();
        let descriptor =
            CapabilityDescriptor::new(id.clone(), id, Arc::new(ClosureHandler(handler)))
                .with_flags(CapabilityFlags::CALLABLE_BY_LLM);
        self.register(descriptor)
    }

    /// Registers a group of capabilities, mirroring `claw_cap_register_group`.
    pub fn register_group(&self, group: CapabilityGroup) -> Result<(), CapabilityError> {
        let group_id = group.group_id.clone();
        {
            let mut state = self.state();
            Self::validate_group(&state, &group)?;

            // Run group init before exposing any member (matches C, which calls
            // group_init under the registry lock during registration).
            if let Some(hooks) = &group.hooks {
                hooks.init()?;
            }

            let member_ids: Vec<String> = group.descriptors.iter().map(|d| d.id.clone()).collect();
            for descriptor in group.descriptors {
                state
                    .name_to_id
                    .insert(descriptor.name.clone(), descriptor.id.clone());
                state.descriptors.insert(
                    descriptor.id.clone(),
                    DescriptorEntry {
                        group_id: group_id.clone(),
                        state: CapabilityState::Registered,
                        active_calls: 0,
                        init_called: false,
                        descriptor,
                    },
                );
            }
            state.groups.insert(
                group_id.clone(),
                GroupEntry {
                    plugin_name: group.plugin_name,
                    version: group.version,
                    hooks: group.hooks,
                    member_ids,
                    state: CapabilityState::Registered,
                },
            );
        }

        if self.state().started {
            self.enable_group(&group_id)?;
        }
        Ok(())
    }

    fn validate_group(
        state: &RegistryState,
        group: &CapabilityGroup,
    ) -> Result<(), CapabilityError> {
        if group.group_id.is_empty() || group.descriptors.is_empty() {
            return Err(CapabilityError::InvalidArg);
        }
        if state.groups.contains_key(&group.group_id) {
            return Err(CapabilityError::AlreadyExists);
        }
        // Conflict semantics match C `claw_cap_names_conflict_locked`: only
        // id-vs-id and name-vs-name collide; cross collisions (id == other name)
        // are intentionally allowed.
        for (index, descriptor) in group.descriptors.iter().enumerate() {
            if descriptor.id.is_empty() || descriptor.name.is_empty() {
                return Err(CapabilityError::InvalidArg);
            }
            if state.descriptors.contains_key(&descriptor.id)
                || state.name_to_id.contains_key(&descriptor.name)
            {
                return Err(CapabilityError::AlreadyExists);
            }
            for other in group.descriptors.iter().skip(index + 1) {
                if descriptor.id == other.id || descriptor.name == other.name {
                    return Err(CapabilityError::AlreadyExists);
                }
            }
        }
        Ok(())
    }

    // --- Lifecycle ------------------------------------------------------

    /// Starts the registry and enables every non-disabled group, mirroring
    /// `claw_cap_start_all`.
    pub fn start_all(&self) -> Result<(), CapabilityError> {
        let to_enable: Vec<String> = {
            let mut state = self.state();
            if state.started {
                return Ok(());
            }
            state.started = true;
            state
                .groups
                .iter()
                .filter(|(_, group)| group.state != CapabilityState::Disabled)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for group_id in to_enable {
            let _ = self.enable_group(&group_id);
        }
        Ok(())
    }

    /// Disables every started group and clears the started flag, mirroring
    /// `claw_cap_stop_all`.
    pub fn stop_all(&self) -> Result<(), CapabilityError> {
        let to_disable: Vec<String> = {
            let state = self.state();
            state
                .groups
                .iter()
                .filter(|(_, group)| group.state == CapabilityState::Started)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for group_id in to_disable {
            let _ = self.disable_group(&group_id);
        }
        self.state().started = false;
        Ok(())
    }

    /// Enables (and, if the registry is started, starts) a group, mirroring
    /// `claw_cap_enable_group`.
    pub fn enable_group(&self, group_id: &str) -> Result<(), CapabilityError> {
        if group_id.is_empty() {
            return Err(CapabilityError::InvalidArg);
        }

        let (hooks, members) = {
            let mut state = self.state();
            let group = state
                .groups
                .get(group_id)
                .ok_or(CapabilityError::NotFound)?;
            match group.state {
                CapabilityState::Draining | CapabilityState::Unloading => {
                    return Err(CapabilityError::InvalidState);
                }
                CapabilityState::Started => return Ok(()),
                _ => {}
            }
            if !state.started {
                state.set_group_and_members_state(group_id, CapabilityState::Registered);
                return Ok(());
            }
            state.set_group_and_members_state(group_id, CapabilityState::Started);
            let hooks = state.groups.get(group_id).and_then(|g| g.hooks.clone());
            let members = state
                .groups
                .get(group_id)
                .map(|g| g.member_ids.clone())
                .unwrap_or_default();
            (hooks, members)
        };

        if let Err(error) = self.run_start_callbacks(hooks, &members) {
            self.state()
                .set_group_and_members_state(group_id, CapabilityState::Disabled);
            return Err(error);
        }
        Ok(())
    }

    fn run_start_callbacks(
        &self,
        hooks: Option<Arc<dyn GroupHooks>>,
        members: &[String],
    ) -> Result<(), CapabilityError> {
        if let Some(hooks) = hooks {
            hooks.start()?;
        }
        for id in members {
            let (handler, needs_init) = {
                let state = self.state();
                match state.descriptors.get(id) {
                    Some(entry) => (entry.descriptor.handler.clone(), !entry.init_called),
                    None => continue,
                }
            };
            if needs_init {
                handler.init()?;
                if let Some(entry) = self.state().descriptors.get_mut(id) {
                    entry.init_called = true;
                }
            }
            handler.start()?;
        }
        Ok(())
    }

    /// Disables a group, running stop callbacks best-effort, mirroring
    /// `claw_cap_disable_group`.
    pub fn disable_group(&self, group_id: &str) -> Result<(), CapabilityError> {
        if group_id.is_empty() {
            return Err(CapabilityError::InvalidArg);
        }

        let (hooks, members) = {
            let mut state = self.state();
            let group = state
                .groups
                .get(group_id)
                .ok_or(CapabilityError::NotFound)?;
            match group.state {
                CapabilityState::Disabled => return Ok(()),
                CapabilityState::Draining | CapabilityState::Unloading => {
                    return Err(CapabilityError::InvalidState);
                }
                _ => {}
            }
            state.set_group_and_members_state(group_id, CapabilityState::Disabled);
            let hooks = state.groups.get(group_id).and_then(|g| g.hooks.clone());
            let members = state
                .groups
                .get(group_id)
                .map(|g| g.member_ids.clone())
                .unwrap_or_default();
            (hooks, members)
        };

        self.run_stop_callbacks(hooks, &members);
        Ok(())
    }

    fn run_stop_callbacks(&self, hooks: Option<Arc<dyn GroupHooks>>, members: &[String]) {
        for id in members.iter().rev() {
            let handler = self
                .state()
                .descriptors
                .get(id)
                .map(|entry| entry.descriptor.handler.clone());
            if let Some(handler) = handler {
                let _ = handler.stop();
            }
        }
        if let Some(hooks) = hooks {
            let _ = hooks.stop();
        }
    }

    /// Drains in-flight calls then removes a group, mirroring
    /// `claw_cap_unregister_group`. `timeout` of `None` waits indefinitely.
    pub fn unregister_group(
        &self,
        group_id: &str,
        timeout: Option<Duration>,
    ) -> Result<(), CapabilityError> {
        if group_id.is_empty() {
            return Err(CapabilityError::InvalidArg);
        }

        {
            let mut state = self.state();
            let group = state
                .groups
                .get(group_id)
                .ok_or(CapabilityError::NotFound)?;
            if group.state == CapabilityState::Unloading {
                return Err(CapabilityError::InvalidState);
            }
            state.set_group_and_members_state(group_id, CapabilityState::Draining);
        }

        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            {
                let mut state = self.state();
                if !state.group_has_active_calls(group_id) {
                    state.set_group_and_members_state(group_id, CapabilityState::Unloading);
                    break;
                }
            }
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Err(CapabilityError::Timeout);
                }
            }
            std::thread::sleep(DRAIN_POLL);
        }

        let (hooks, members) = {
            let state = self.state();
            let hooks = state.groups.get(group_id).and_then(|g| g.hooks.clone());
            let members = state
                .groups
                .get(group_id)
                .map(|g| g.member_ids.clone())
                .unwrap_or_default();
            (hooks, members)
        };
        self.run_stop_callbacks(hooks, &members);

        let mut state = self.state();
        if let Some(group) = state.groups.remove(group_id) {
            for id in group.member_ids {
                if let Some(entry) = state.descriptors.remove(&id) {
                    state.name_to_id.remove(&entry.descriptor.name);
                }
            }
        }
        Ok(())
    }

    /// Unregisters a single capability (only when it is the sole member of its
    /// group), mirroring `claw_cap_unregister`.
    pub fn unregister(
        &self,
        id_or_name: &str,
        timeout: Option<Duration>,
    ) -> Result<(), CapabilityError> {
        if id_or_name.is_empty() {
            return Err(CapabilityError::InvalidArg);
        }
        let group_id = {
            let state = self.state();
            let id = state
                .resolve_id(id_or_name)
                .ok_or(CapabilityError::NotFound)?;
            let group_id = state
                .descriptors
                .get(&id)
                .map(|entry| entry.group_id.clone())
                .ok_or(CapabilityError::NotFound)?;
            let member_count = state
                .groups
                .get(&group_id)
                .map(|group| group.member_ids.len())
                .unwrap_or(0);
            if member_count != 1 {
                return Err(CapabilityError::NotSupported);
            }
            group_id
        };
        self.unregister_group(&group_id, timeout)
    }

    // --- Visibility -----------------------------------------------------

    /// Replaces the global set of LLM-visible groups, mirroring
    /// `claw_cap_set_llm_visible_groups`. An empty set means all groups visible.
    pub fn set_llm_visible_groups(
        &self,
        group_ids: impl IntoIterator<Item = String>,
    ) -> Result<(), CapabilityError> {
        let groups = Self::collect_non_empty(group_ids)?;
        self.state().global_visible_groups = groups;
        Ok(())
    }

    /// Sets the per-session LLM-visible groups, mirroring
    /// `claw_cap_set_session_llm_visible_groups`. An empty set removes the
    /// session entry.
    pub fn set_session_llm_visible_groups(
        &self,
        session_id: impl Into<String>,
        group_ids: impl IntoIterator<Item = String>,
    ) -> Result<(), CapabilityError> {
        let session_id = session_id.into();
        if session_id.is_empty() {
            return Err(CapabilityError::InvalidArg);
        }
        let groups = Self::collect_non_empty(group_ids)?;
        let mut state = self.state();
        if groups.is_empty() {
            state.session_visibility.remove(&session_id);
        } else {
            state.session_visibility.insert(session_id, groups);
        }
        Ok(())
    }

    fn collect_non_empty(
        group_ids: impl IntoIterator<Item = String>,
    ) -> Result<Vec<String>, CapabilityError> {
        group_ids
            .into_iter()
            .map(|id| {
                if id.is_empty() {
                    Err(CapabilityError::InvalidArg)
                } else {
                    Ok(id)
                }
            })
            .collect()
    }

    // --- Dispatch -------------------------------------------------------

    /// Dispatches a capability call, mirroring `claw_cap_call`: resolve, gate on
    /// availability + LLM visibility (for agent callers), run the handler with
    /// the registry unlocked, then settle the active-call count. Unknown
    /// capabilities fall through to the [`RegistryBackend`] if present.
    pub fn call(
        &self,
        id_or_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        let (id, handler) = {
            let mut state = self.state();
            let Some(id) = state.resolve_id(id_or_name) else {
                drop(state);
                return match &self.backend {
                    Some(backend) => backend.invoke(id_or_name, input_json, context),
                    None => Err(CapabilityError::NotFound),
                };
            };

            let entry = state.descriptors.get(&id).ok_or(CapabilityError::Failed)?;
            if !entry.state.is_available() {
                return Err(CapabilityError::NotAvailable);
            }
            if matches!(
                context.caller,
                CapabilityCaller::Agent | CapabilityCaller::SubAgent
            ) {
                let session = agent_session(context);
                if !state.is_llm_visible(&id, session, context.caller) {
                    return Err(CapabilityError::NotVisible);
                }
            }

            let entry = state
                .descriptors
                .get_mut(&id)
                .ok_or(CapabilityError::Failed)?;
            entry.active_calls += 1;
            (id, entry.descriptor.handler.clone())
        };

        let result = handler.execute(input_json, context);

        if let Some(entry) = self.state().descriptors.get_mut(&id) {
            if entry.active_calls > 0 {
                entry.active_calls -= 1;
            }
        }
        result
    }

    // --- Listing / queries ---------------------------------------------

    /// Whether a group is registered, mirroring `claw_cap_group_exists`.
    pub fn group_exists(&self, group_id: &str) -> bool {
        self.state().groups.contains_key(group_id)
    }

    /// Current lifecycle state of a group, mirroring `claw_cap_get_group_state`.
    pub fn get_group_state(&self, group_id: &str) -> Result<CapabilityState, CapabilityError> {
        self.state()
            .groups
            .get(group_id)
            .map(|group| group.state)
            .ok_or(CapabilityError::NotFound)
    }

    /// Runtime info for a descriptor, mirroring `claw_cap_get_descriptor_state`.
    pub fn get_descriptor_state(
        &self,
        id_or_name: &str,
    ) -> Result<DescriptorRuntimeInfo, CapabilityError> {
        let state = self.state();
        let id = state
            .resolve_id(id_or_name)
            .ok_or(CapabilityError::NotFound)?;
        let entry = state
            .descriptors
            .get(&id)
            .ok_or(CapabilityError::NotFound)?;
        Ok(DescriptorRuntimeInfo {
            id: entry.descriptor.id.clone(),
            name: entry.descriptor.name.clone(),
            group_id: entry.group_id.clone(),
            state: entry.state,
            active_calls: entry.active_calls,
        })
    }

    /// Looks up a listable descriptor, mirroring `claw_cap_find`.
    pub fn find(&self, id_or_name: &str) -> Option<DescriptorSnapshot> {
        let state = self.state();
        let id = state.resolve_id(id_or_name)?;
        let entry = state.descriptors.get(&id)?;
        entry
            .state
            .is_available()
            .then(|| entry.descriptor.snapshot())
    }

    /// Snapshots of all listable descriptors, mirroring `claw_cap_list`.
    pub fn list(&self) -> Vec<DescriptorSnapshot> {
        self.state()
            .descriptors
            .values()
            .filter(|entry| entry.state.is_available())
            .map(|entry| entry.descriptor.snapshot())
            .collect()
    }

    /// Summaries of all registered groups, mirroring `claw_cap_list_groups`.
    pub fn list_groups(&self) -> Vec<GroupInfo> {
        self.state()
            .groups
            .iter()
            .map(|(group_id, group)| GroupInfo {
                group_id: group_id.clone(),
                plugin_name: group.plugin_name.clone(),
                version: group.version.clone(),
                state: group.state,
                descriptor_count: group.member_ids.len(),
            })
            .collect()
    }

    // --- Internal helpers shared with tools.rs --------------------------

    /// Snapshots of descriptors LLM-visible to `context`'s caller/session.
    pub(crate) fn visible_snapshots(&self, context: &CapabilityContext) -> Vec<DescriptorSnapshot> {
        let state = self.state();
        let session = context.session_id.as_deref().filter(|s| !s.is_empty());
        state
            .descriptors
            .values()
            .filter(|entry| state.is_llm_visible(&entry.descriptor.id, session, context.caller))
            .map(|entry| entry.descriptor.snapshot())
            .collect()
    }
}

impl CapabilityInvoker for Registry {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        self.call(capability_name, input_json, context)
    }
}

/// Session id to use for visibility gating: only meaningful for agent callers.
fn agent_session(context: &CapabilityContext) -> Option<&str> {
    if matches!(
        context.caller,
        CapabilityCaller::Agent | CapabilityCaller::SubAgent
    ) {
        context.session_id.as_deref().filter(|s| !s.is_empty())
    } else {
        None
    }
}

/// Adapts a closure into a [`CapabilityHandler`] for `register_handler`.
struct ClosureHandler<F>(F);

impl<F> CapabilityHandler for ClosureHandler<F>
where
    F: Fn(&str, &CapabilityContext) -> Result<CapabilityInvokeResult, CapabilityError>
        + Send
        + Sync,
{
    fn execute(
        &self,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        (self.0)(input_json, context)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::descriptor::{CapabilityFlags, CapabilityKind};

    /// Handler that always succeeds and counts lifecycle calls.
    #[derive(Default)]
    struct CountingHandler {
        init: AtomicUsize,
        start: AtomicUsize,
        stop: AtomicUsize,
    }

    impl CapabilityHandler for CountingHandler {
        fn execute(
            &self,
            input_json: &str,
            _context: &CapabilityContext,
        ) -> Result<CapabilityInvokeResult, CapabilityError> {
            Ok(CapabilityInvokeResult {
                output: input_json.to_string(),
                ok: true,
            })
        }
        fn init(&self) -> Result<(), CapabilityError> {
            self.init.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn start(&self) -> Result<(), CapabilityError> {
            self.start.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn stop(&self) -> Result<(), CapabilityError> {
            self.stop.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn callable(id: &str) -> CapabilityDescriptor {
        CapabilityDescriptor::new(id, id, Arc::new(CountingHandler::default()))
            .with_flags(CapabilityFlags::CALLABLE_BY_LLM)
    }

    fn agent_ctx() -> CapabilityContext {
        CapabilityContext {
            caller: CapabilityCaller::Agent,
            ..Default::default()
        }
    }

    #[test]
    fn register_then_dispatch() {
        let registry = Registry::new(None);
        registry.register(callable("echo")).unwrap();
        let result = registry.call("echo", "{\"x\":1}", &agent_ctx()).unwrap();
        assert!(result.ok);
        assert_eq!(result.output, "{\"x\":1}");
        // Resolvable by name too (here id == name).
        assert!(registry.find("echo").is_some());
    }

    fn callable_named(id: &str, name: &str) -> CapabilityDescriptor {
        CapabilityDescriptor::new(id, name, Arc::new(CountingHandler::default()))
            .with_flags(CapabilityFlags::CALLABLE_BY_LLM)
    }

    #[test]
    fn duplicate_id_conflicts() {
        let registry = Registry::new(None);
        registry.register(callable_named("id1", "name1")).unwrap();
        assert_eq!(
            registry.register(callable_named("id1", "name2")),
            Err(CapabilityError::AlreadyExists)
        );
    }

    #[test]
    fn duplicate_name_conflicts() {
        let registry = Registry::new(None);
        registry.register(callable_named("id1", "shared")).unwrap();
        assert_eq!(
            registry.register(callable_named("id2", "shared")),
            Err(CapabilityError::AlreadyExists)
        );
    }

    #[test]
    fn cross_id_name_collision_is_allowed() {
        // Matches C: a new id may equal an existing name (and vice versa).
        let registry = Registry::new(None);
        registry.register(callable_named("alpha", "beta")).unwrap();
        registry.register(callable_named("beta", "gamma")).unwrap();
        assert_eq!(registry.list().len(), 2);
    }

    #[test]
    fn empty_id_or_name_rejected() {
        let registry = Registry::new(None);
        assert_eq!(
            registry.register(callable_named("", "name")),
            Err(CapabilityError::InvalidArg)
        );
        assert_eq!(
            registry.register(callable_named("id", "")),
            Err(CapabilityError::InvalidArg)
        );
    }

    #[test]
    fn unknown_capability_without_backend_is_not_found() {
        let registry = Registry::new(None);
        assert_eq!(
            registry.call("nope", "{}", &agent_ctx()),
            Err(CapabilityError::NotFound)
        );
    }

    #[test]
    fn backend_fallback_serves_unowned_capability() {
        struct Backend;
        impl RegistryBackend for Backend {
            fn invoke(
                &self,
                name: &str,
                _input: &str,
                _ctx: &CapabilityContext,
            ) -> Result<CapabilityInvokeResult, CapabilityError> {
                assert_eq!(name, "from_c");
                Ok(CapabilityInvokeResult {
                    output: "c".to_string(),
                    ok: true,
                })
            }
        }
        let registry = Registry::new(Some(Box::new(Backend)));
        let result = registry.call("from_c", "{}", &agent_ctx()).unwrap();
        assert_eq!(result.output, "c");
    }

    #[test]
    fn disabled_capability_is_not_available() {
        let registry = Registry::new(None);
        registry.register(callable("svc")).unwrap();
        registry.start_all().unwrap();
        registry.disable_group("svc").unwrap();
        // System caller bypasses visibility, so this surfaces availability.
        let ctx = CapabilityContext {
            caller: CapabilityCaller::System,
            ..Default::default()
        };
        assert_eq!(
            registry.call("svc", "{}", &ctx),
            Err(CapabilityError::NotAvailable)
        );
    }

    #[test]
    fn lifecycle_callbacks_run_on_start_and_stop() {
        let handler = Arc::new(CountingHandler::default());
        let registry = Registry::new(None);
        registry
            .register(
                CapabilityDescriptor::new("svc", "svc", handler.clone())
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();
        // Not started yet: no callbacks.
        assert_eq!(handler.start.load(Ordering::SeqCst), 0);
        registry.start_all().unwrap();
        assert_eq!(handler.init.load(Ordering::SeqCst), 1);
        assert_eq!(handler.start.load(Ordering::SeqCst), 1);
        registry.disable_group("svc").unwrap();
        assert_eq!(handler.stop.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry.get_group_state("svc").unwrap(),
            CapabilityState::Disabled
        );
    }

    #[test]
    fn register_while_started_auto_enables() {
        let handler = Arc::new(CountingHandler::default());
        let registry = Registry::new(None);
        registry.start_all().unwrap();
        registry
            .register(
                CapabilityDescriptor::new("late", "late", handler.clone())
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();
        assert_eq!(handler.start.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry.get_group_state("late").unwrap(),
            CapabilityState::Started
        );
    }

    #[test]
    fn visibility_default_global_and_session() {
        let registry = Registry::new(None);
        registry
            .register_group(CapabilityGroup::new("g1", "g1", "1", [callable("a")]))
            .unwrap();
        registry
            .register_group(CapabilityGroup::new("g2", "g2", "1", [callable("b")]))
            .unwrap();

        // Default: empty global list => everything visible.
        assert!(registry.call("a", "{}", &agent_ctx()).is_ok());
        assert!(registry.call("b", "{}", &agent_ctx()).is_ok());

        // Global gating to g1 only.
        registry.set_llm_visible_groups(["g1".to_string()]).unwrap();
        assert!(registry.call("a", "{}", &agent_ctx()).is_ok());
        assert_eq!(
            registry.call("b", "{}", &agent_ctx()),
            Err(CapabilityError::NotVisible)
        );

        // Session override adds g2 for sess-1.
        registry
            .set_session_llm_visible_groups("sess-1", ["g2".to_string()])
            .unwrap();
        let sess_ctx = CapabilityContext {
            caller: CapabilityCaller::Agent,
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        assert!(registry.call("b", "{}", &sess_ctx).is_ok());
    }

    #[test]
    fn root_agent_only_hidden_from_sub_agent() {
        let registry = Registry::new(None);
        let descriptor = CapabilityDescriptor::new(
            "root_tool",
            "root_tool",
            Arc::new(CountingHandler::default()),
        )
        .with_flags(CapabilityFlags::CALLABLE_BY_LLM | CapabilityFlags::ROOT_AGENT_ONLY);
        registry.register(descriptor).unwrap();

        assert!(registry.call("root_tool", "{}", &agent_ctx()).is_ok());
        let sub_ctx = CapabilityContext {
            caller: CapabilityCaller::SubAgent,
            ..Default::default()
        };
        assert_eq!(
            registry.call("root_tool", "{}", &sub_ctx),
            Err(CapabilityError::NotVisible)
        );
    }

    #[test]
    fn non_llm_capability_callable_by_system_only() {
        let registry = Registry::new(None);
        // No CALLABLE_BY_LLM flag.
        registry
            .register(CapabilityDescriptor::new(
                "internal",
                "internal",
                Arc::new(CountingHandler::default()),
            ))
            .unwrap();
        let system_ctx = CapabilityContext {
            caller: CapabilityCaller::System,
            ..Default::default()
        };
        assert!(registry.call("internal", "{}", &system_ctx).is_ok());
        assert_eq!(
            registry.call("internal", "{}", &agent_ctx()),
            Err(CapabilityError::NotVisible)
        );
    }

    #[test]
    fn unregister_single_member_then_gone() {
        let registry = Registry::new(None);
        registry.register(callable("solo")).unwrap();
        registry
            .unregister("solo", Some(Duration::from_secs(1)))
            .unwrap();
        assert!(registry.find("solo").is_none());
        assert!(!registry.group_exists("solo"));
    }

    #[test]
    fn unregister_member_of_multi_group_is_unsupported() {
        let registry = Registry::new(None);
        registry
            .register_group(CapabilityGroup::new(
                "pair",
                "pair",
                "1",
                [callable("x"), callable("y")],
            ))
            .unwrap();
        assert_eq!(
            registry.unregister("x", Some(Duration::from_secs(1))),
            Err(CapabilityError::NotSupported)
        );
    }

    #[test]
    fn listing_and_state_queries() {
        let registry = Registry::new(None);
        registry
            .register(callable("a").with_kind(CapabilityKind::Callable))
            .unwrap();
        registry.start_all().unwrap();

        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list_groups().len(), 1);
        let info = registry.get_descriptor_state("a").unwrap();
        assert_eq!(info.id, "a");
        assert_eq!(info.group_id, "a");
        assert_eq!(info.state, CapabilityState::Started);
        assert_eq!(info.active_calls, 0);
    }

    #[test]
    fn reenable_starts_again_without_reinitializing() {
        let handler = Arc::new(CountingHandler::default());
        let registry = Registry::new(None);
        registry
            .register(
                CapabilityDescriptor::new("svc", "svc", handler.clone())
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();
        registry.start_all().unwrap();
        registry.disable_group("svc").unwrap();
        registry.enable_group("svc").unwrap();

        // init runs once across enable/disable/enable; start runs each enable.
        assert_eq!(handler.init.load(Ordering::SeqCst), 1);
        assert_eq!(handler.start.load(Ordering::SeqCst), 2);
        assert_eq!(handler.stop.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry.get_group_state("svc").unwrap(),
            CapabilityState::Started
        );
    }

    #[test]
    fn enable_disable_unknown_group_errors() {
        let registry = Registry::new(None);
        assert_eq!(
            registry.enable_group("nope"),
            Err(CapabilityError::NotFound)
        );
        assert_eq!(
            registry.disable_group("nope"),
            Err(CapabilityError::NotFound)
        );
        assert_eq!(
            registry.get_group_state("nope"),
            Err(CapabilityError::NotFound)
        );
    }

    #[test]
    fn session_visibility_empty_clears_entry() {
        let registry = Registry::new(None);
        registry
            .register_group(CapabilityGroup::new("g1", "g1", "1", [callable("a")]))
            .unwrap();
        registry
            .set_llm_visible_groups(["other".to_string()])
            .unwrap();
        let ctx = CapabilityContext {
            caller: CapabilityCaller::Agent,
            session_id: Some("s".to_string()),
            ..Default::default()
        };
        // Session grants g1.
        registry
            .set_session_llm_visible_groups("s", ["g1".to_string()])
            .unwrap();
        assert!(registry.call("a", "{}", &ctx).is_ok());
        // Clearing the session removes the grant => hidden again.
        registry.set_session_llm_visible_groups("s", []).unwrap();
        assert_eq!(
            registry.call("a", "{}", &ctx),
            Err(CapabilityError::NotVisible)
        );
    }

    #[test]
    fn unregister_times_out_while_a_call_is_in_flight() {
        use std::sync::Condvar;

        // Phases: 0 idle -> 1 running (handler entered) -> 2 release.
        struct Gate {
            lock: Mutex<u8>,
            cv: Condvar,
        }
        struct Blocking(Arc<Gate>);
        impl CapabilityHandler for Blocking {
            fn execute(
                &self,
                _input: &str,
                _ctx: &CapabilityContext,
            ) -> Result<CapabilityInvokeResult, CapabilityError> {
                {
                    let mut phase = self.0.lock.lock().unwrap();
                    *phase = 1;
                    self.0.cv.notify_all();
                }
                let mut phase = self.0.lock.lock().unwrap();
                while *phase != 2 {
                    phase = self.0.cv.wait(phase).unwrap();
                }
                Ok(CapabilityInvokeResult {
                    output: String::new(),
                    ok: true,
                })
            }
        }

        let gate = Arc::new(Gate {
            lock: Mutex::new(0),
            cv: Condvar::new(),
        });
        let registry = Arc::new(Registry::new(None));
        registry
            .register(
                CapabilityDescriptor::new("block", "block", Arc::new(Blocking(gate.clone())))
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();

        let caller = {
            let registry = registry.clone();
            std::thread::spawn(move || {
                let _ = registry.call("block", "{}", &agent_ctx());
            })
        };

        // Wait until the handler is actually running (active_calls == 1).
        {
            let mut phase = gate.lock.lock().unwrap();
            while *phase != 1 {
                phase = gate.cv.wait(phase).unwrap();
            }
        }

        // Zero deadline + an in-flight call => immediate Timeout.
        assert_eq!(
            registry.unregister_group("block", Some(Duration::ZERO)),
            Err(CapabilityError::Timeout)
        );

        // Release the handler and let the call finish.
        {
            let mut phase = gate.lock.lock().unwrap();
            *phase = 2;
            gate.cv.notify_all();
        }
        caller.join().unwrap();

        // The drain attempt left the group in Draining state.
        assert_eq!(
            registry.get_group_state("block").unwrap(),
            CapabilityState::Draining
        );
    }
}
