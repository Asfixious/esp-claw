//! Capability descriptors: metadata + handler, mirroring `claw_cap_descriptor_t`.

use std::sync::Arc;

use crate::context::CapabilityContext;
use crate::error::CapabilityError;
use crate::invoker::CapabilityInvokeResult;

/// What a capability does (mirrors `claw_cap_kind_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CapabilityKind {
    /// Invocable by the agent tool loop.
    #[default]
    Callable,
    /// Emits events only; not directly callable.
    EventSource,
    /// Both callable and event-emitting.
    Hybrid,
}

/// Capability behavior flags (mirrors `claw_cap_flags_t`).
///
/// A bitset over the same bit positions as the C `claw_cap_flags_t` enum, so
/// the values cross the FFI boundary unchanged.
///
/// # Examples
///
/// ```
/// use claw_capability::CapabilityFlags;
///
/// let flags = CapabilityFlags::CALLABLE_BY_LLM | CapabilityFlags::ROOT_AGENT_ONLY;
/// assert!(flags.contains(CapabilityFlags::CALLABLE_BY_LLM));
/// assert!(!flags.contains(CapabilityFlags::EMITS_EVENTS));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CapabilityFlags(u32);

impl CapabilityFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);
    /// Exposed to the LLM tool loop (`CLAW_CAP_FLAG_CALLABLE_BY_LLM`).
    pub const CALLABLE_BY_LLM: Self = Self(1 << 0);
    /// Emits events (`CLAW_CAP_FLAG_EMITS_EVENTS`).
    pub const EMITS_EVENTS: Self = Self(1 << 1);
    /// Has init/start/stop lifecycle (`CLAW_CAP_FLAG_SUPPORTS_LIFECYCLE`).
    pub const SUPPORTS_LIFECYCLE: Self = Self(1 << 2);
    /// Restricted capability (`CLAW_CAP_FLAG_RESTRICTED`).
    pub const RESTRICTED: Self = Self(1 << 3);
    /// Only the root agent may use it (`CLAW_CAP_FLAG_ROOT_AGENT_ONLY`).
    pub const ROOT_AGENT_ONLY: Self = Self(1 << 4);

    /// Wraps a raw flag bitset (e.g. a value received over the C ABI).
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw flag bitset.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns `true` when every bit in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl core::ops::BitOr for CapabilityFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for CapabilityFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Lifecycle state of a capability or group (mirrors `claw_cap_state_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CapabilityState {
    /// Registered but not yet started.
    #[default]
    Registered,
    /// Started and serving calls.
    Started,
    /// Administratively disabled.
    Disabled,
    /// Waiting for in-flight calls to finish before unloading.
    Draining,
    /// Drained; being removed.
    Unloading,
}

impl CapabilityState {
    /// Lowercase label, matching `claw_cap_state_to_string`.
    ///
    /// # Examples
    ///
    /// ```
    /// use claw_capability::CapabilityState;
    ///
    /// assert_eq!(CapabilityState::Started.as_str(), "started");
    /// assert_eq!(CapabilityState::default().as_str(), "registered");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            CapabilityState::Registered => "registered",
            CapabilityState::Started => "started",
            CapabilityState::Disabled => "disabled",
            CapabilityState::Draining => "draining",
            CapabilityState::Unloading => "unloading",
        }
    }
}

/// Executes a capability and (optionally) participates in lifecycle callbacks.
///
/// Replaces the C `execute`/`init`/`start`/`stop` function pointers. Only
/// [`execute`](CapabilityHandler::execute) is required; the lifecycle hooks
/// default to no-ops for the common case.
pub trait CapabilityHandler: Send + Sync {
    /// Runs the capability. `output` of the returned result is always
    /// model-visible text; `ok = false` marks a handler-level failure that is
    /// still surfaced to the model.
    fn execute(
        &self,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError>;

    /// One-time initialization, called once before the first `start`.
    fn init(&self) -> Result<(), CapabilityError> {
        Ok(())
    }

    /// Called when the owning group is enabled/started.
    fn start(&self) -> Result<(), CapabilityError> {
        Ok(())
    }

    /// Called when the owning group is disabled/unregistered.
    fn stop(&self) -> Result<(), CapabilityError> {
        Ok(())
    }
}

/// A registered capability: identity, metadata, and its handler.
///
/// Mirrors `claw_cap_descriptor_t`; the C function pointers become the
/// [`CapabilityHandler`]. Build with [`CapabilityDescriptor::new`] and the
/// `with_*` setters.
#[derive(Clone)]
pub struct CapabilityDescriptor {
    pub id: String,
    pub name: String,
    pub family: Option<String>,
    pub description: Option<String>,
    pub kind: CapabilityKind,
    pub flags: CapabilityFlags,
    pub input_schema_json: Option<String>,
    pub handler: Arc<dyn CapabilityHandler>,
}

impl CapabilityDescriptor {
    /// Creates a descriptor with the given id, name, and handler. Defaults to
    /// [`CapabilityKind::Callable`] with no flags; use the `with_*` setters to
    /// add metadata and flags.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        handler: Arc<dyn CapabilityHandler>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            family: None,
            description: None,
            kind: CapabilityKind::Callable,
            flags: CapabilityFlags::NONE,
            input_schema_json: None,
            handler,
        }
    }

    /// Sets the capability family (catalog grouping label).
    pub fn with_family(mut self, family: impl Into<String>) -> Self {
        self.family = Some(family.into());
        self
    }

    /// Sets the human/model-readable description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the capability kind.
    pub fn with_kind(mut self, kind: CapabilityKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets the behavior flags.
    pub fn with_flags(mut self, flags: CapabilityFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Sets the JSON input schema string.
    pub fn with_input_schema(mut self, input_schema_json: impl Into<String>) -> Self {
        self.input_schema_json = Some(input_schema_json.into());
        self
    }

    /// Owned, handler-free metadata snapshot.
    pub(crate) fn snapshot(&self) -> DescriptorSnapshot {
        DescriptorSnapshot {
            id: self.id.clone(),
            name: self.name.clone(),
            family: self.family.clone(),
            description: self.description.clone(),
            kind: self.kind,
            flags: self.flags,
            input_schema_json: self.input_schema_json.clone(),
        }
    }
}

/// Owned, handler-free view of a descriptor, returned by `find`/`list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorSnapshot {
    pub id: String,
    pub name: String,
    pub family: Option<String>,
    pub description: Option<String>,
    pub kind: CapabilityKind,
    pub flags: CapabilityFlags,
    pub input_schema_json: Option<String>,
}

/// Runtime state of a descriptor, returned by `get_descriptor_state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorRuntimeInfo {
    pub id: String,
    pub name: String,
    pub group_id: String,
    pub state: CapabilityState,
    pub active_calls: u32,
}
