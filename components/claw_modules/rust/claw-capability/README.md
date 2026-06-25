# claw-capability

Capability registry and dispatch for the ESP-Claw agent framework, Rust-first.

A *capability* is a model-callable action (files, Lua, web search, IM, …). This
crate is the registration + dispatch layer for them: it owns capability
descriptors and metadata, capability groups, the lifecycle state machine, global
and per-session LLM visibility, the dispatch path, and the LLM tool-list /
catalog renderers. It is a pure-Rust reimplementation of the C `claw_cap`
subsystem; the C function pointers become a handler trait, and all state lives
on an instance `Registry` (replacing the C `s_runtime` global).

## How it works

Capabilities are registered as **descriptors** (optionally bundled into
**groups**), the registry is **started**, and each call is **gated** by
lifecycle state and LLM visibility before it reaches the handler. Capabilities
the registry does not own can fall through to an optional `RegistryBackend` —
the seam to the legacy C registry during migration.

## Public API

Re-exported from the crate root:

| Item | Role |
|------|------|
| `Registry` | The owning instance: registration, lifecycle (`start_all` / `stop_all`, group enable/disable), visibility, `invoke` dispatch, listing, and the `build_llm_tools_json` / `build_catalog` renderers. |
| `RegistryBackend` | Fallback seam for capabilities the registry doesn't own (e.g. the C registry during migration). |
| `CapabilityDescriptor` | One capability's metadata + handler. `new(name, kind, handler)`, `with_flags(..)`. |
| `CapabilityKind` / `CapabilityFlags` | What a capability does, and its behavior flags (e.g. `CALLABLE_BY_LLM`, `ROOT_AGENT_ONLY`, `EMITS_EVENTS`) as a bitset. |
| `CapabilityState` | Lifecycle state (`Registered`, `Started`, …) with string labels. |
| `CapabilityHandler` | The trait a capability implements: `execute(input_json, context) -> CapabilityInvokeResult`. |
| `CapabilityInvoker` / `CapabilityInvokeResult` | The dispatch trait and its result (`output`, `ok`). |
| `CapabilityContext` / `CapabilityCaller` | Per-call context (request/session ids) and who initiated the call (`Agent`, …). |
| `CapabilityGroup` / `GroupHooks` / `GroupInfo` | A registrable group of capabilities, its lifecycle hooks, and a summary view. |
| `DescriptorRuntimeInfo` / `DescriptorSnapshot` | Runtime state and listable snapshots of registered descriptors. |
| `CapabilityError` | Dispatch / lifecycle failure. |

## Example

```rust
use std::sync::Arc;

use claw_capability::{
    CapabilityCaller, CapabilityContext, CapabilityDescriptor, CapabilityFlags,
    CapabilityHandler, CapabilityInvokeResult, CapabilityError, Registry,
};

struct Echo;
impl CapabilityHandler for Echo {
    fn execute(
        &self,
        input_json: &str,
        _context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        Ok(CapabilityInvokeResult { output: input_json.to_string(), ok: true })
    }
}

let registry = Registry::new(None);
registry
    .register(
        CapabilityDescriptor::new("echo", "echo", Arc::new(Echo))
            .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
    )
    .expect("register echo");
registry.start_all().expect("start");

let context = CapabilityContext { caller: CapabilityCaller::Agent, ..Default::default() };
let result = registry.invoke("echo", r#"{"hello":"world"}"#, &context).expect("invoke");
assert!(result.ok);
```

A fuller, runnable walkthrough — descriptors, groups, lifecycle gating, LLM
visibility, and a `RegistryBackend` fallthrough — is in `examples/`:

```bash
cargo run --example capability_dispatch -p claw-capability --target x86_64-unknown-linux-gnu
```

## Where it fits

A pure-Rust core crate depending only on `thiserror` and `serde_json`. It bundles
into the firmware's `claw_rt` staticlib and is fully host-testable. Concrete C
capability descriptors are adapted into this registry at the `claw_capi`
boundary; the registry itself stays free of FFI.
