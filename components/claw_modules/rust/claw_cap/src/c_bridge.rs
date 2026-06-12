//! `TurnContext` ↔ capability routing conversions (pure Rust).

use claw_core::{ToolCaller, TurnContext};

use crate::context::{CapabilityCaller, CapabilityContext};

pub fn caller_from_tool(caller: ToolCaller) -> CapabilityCaller {
    match caller {
        ToolCaller::System => CapabilityCaller::System,
        ToolCaller::RootAgent => CapabilityCaller::Agent,
        ToolCaller::Console => CapabilityCaller::Console,
        ToolCaller::SubAgent => CapabilityCaller::SubAgent,
    }
}

pub fn tool_from_caller(caller: CapabilityCaller) -> ToolCaller {
    match caller {
        CapabilityCaller::System => ToolCaller::System,
        CapabilityCaller::Agent => ToolCaller::RootAgent,
        CapabilityCaller::Console => ToolCaller::Console,
        CapabilityCaller::SubAgent => ToolCaller::SubAgent,
    }
}

/// Map orchestrator turn identity to the cap dispatch context.
pub fn turn_to_capability_context(ctx: &TurnContext) -> CapabilityContext {
    CapabilityContext {
        request_id: ctx.iteration_id,
        session_id: ctx.session_id.clone(),
        source_channel: ctx.source_channel.clone(),
        source_chat_id: ctx.source_chat_id.clone(),
        target_channel: ctx.target_channel.clone(),
        target_chat_id: ctx.target_chat_id.clone(),
        source_capability: ctx.source_cap.clone(),
        caller: caller_from_tool(ctx.caller),
    }
}

pub fn capability_to_turn(ctx: &CapabilityContext) -> TurnContext {
    TurnContext {
        iteration_id: ctx.request_id,
        session_id: ctx.session_id.clone(),
        source_channel: ctx.source_channel.clone(),
        source_chat_id: ctx.source_chat_id.clone(),
        target_channel: ctx.target_channel.clone(),
        target_chat_id: ctx.target_chat_id.clone(),
        source_cap: ctx.source_capability.clone(),
        caller: tool_from_caller(ctx.caller),
    }
}
