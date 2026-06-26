//! End-to-end capability dispatch with `claw-capability`.
//!
//! Run with:
//!
//! ```text
//! cargo run --example capability_dispatch -p claw-capability --target x86_64-unknown-linux-gnu
//! ```
//!
//! It mirrors the C subsystem: capabilities are registered as descriptors
//! (optionally grouped), the registry is started, and calls are gated by
//! lifecycle state and LLM visibility before reaching the handler. Capabilities
//! the registry does not own fall through to a [`RegistryBackend`] standing in
//! for the C registry (`claw_cap.c`).

use std::sync::Arc;

use claw_capability::{
    CapabilityCaller, CapabilityContext, CapabilityDescriptor, CapabilityError, CapabilityFlags,
    CapabilityHandler, CapabilityInvokeResult, CapabilityInvoker, Registry, RegistryBackend,
};

/// Echoes its caller and input back as model-visible text.
struct EchoHandler;

impl CapabilityHandler for EchoHandler {
    fn execute(
        &self,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        Ok(CapabilityInvokeResult {
            output: format!("caller={:?} input={input_json}", context.caller),
            ok: true,
        })
    }
}

/// A root-agent-only capability, gated by `ROOT_AGENT_ONLY`.
struct RebootHandler;

impl CapabilityHandler for RebootHandler {
    fn execute(
        &self,
        _input_json: &str,
        _context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        Ok(CapabilityInvokeResult {
            output: "rebooting".to_string(),
            ok: true,
        })
    }
}

/// Stands in for the C capability registry reached over FFI.
struct CRegistryBackend;

impl RegistryBackend for CRegistryBackend {
    fn invoke(
        &self,
        capability_name: &str,
        _input_json: &str,
        _context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        match capability_name {
            "time" => Ok(CapabilityInvokeResult {
                output: "2026-06-25 15:00:00".to_string(),
                ok: true,
            }),
            _ => Err(CapabilityError::NotFound),
        }
    }
}

fn main() {
    let registry = Registry::new(Some(Box::new(CRegistryBackend)));

    registry
        .register(
            CapabilityDescriptor::new("echo", "echo", Arc::new(EchoHandler))
                .with_description("Echo the input back")
                .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
        )
        .expect("register echo");
    registry
        .register(
            CapabilityDescriptor::new("reboot", "reboot", Arc::new(RebootHandler))
                .with_flags(CapabilityFlags::CALLABLE_BY_LLM | CapabilityFlags::ROOT_AGENT_ONLY),
        )
        .expect("register reboot");
    registry.start_all().expect("start all");

    let context = CapabilityContext {
        request_id: 7,
        session_id: Some("sess-1".to_string()),
        source_channel: Some("telegram".to_string()),
        caller: CapabilityCaller::Agent,
        ..Default::default()
    };

    // 1. Rust handler, gated visibility passes for the agent caller.
    let echo = registry
        .invoke("echo", r#"{"msg":"hi"}"#, &context)
        .expect("echo");
    println!("echo -> ok={} output={:?}", echo.ok, echo.output);

    // 2. Unknown to the registry, served by the C backend fallback.
    let time = registry.invoke("time", "{}", &context).expect("time");
    println!("time -> ok={} output={:?}", time.ok, time.output);

    // 3. ROOT_AGENT_ONLY capability denied for a sub-agent caller.
    let sub_context = CapabilityContext {
        caller: CapabilityCaller::SubAgent,
        ..context.clone()
    };
    match registry.invoke("reboot", "{}", &sub_context) {
        Err(CapabilityError::NotVisible) => println!("reboot (sub-agent) -> denied: not visible"),
        other => println!("reboot (sub-agent) -> unexpected: {other:?}"),
    }

    // 4. The LLM tool list reflects visibility for this caller.
    let tools = registry.build_llm_tools_json(&context, true);
    println!("tools(agent) -> {tools}");

    // 5. The human-readable catalog.
    print!("{}", registry.build_catalog());
}
