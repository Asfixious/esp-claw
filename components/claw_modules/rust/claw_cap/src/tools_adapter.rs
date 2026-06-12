//! [`Tools`] port adapter over [`CapabilityInvoker`] + optional catalog source.

use std::sync::Arc;

use claw_core::{ToolCatalog, ToolError, ToolInvocation, ToolOutput, Tools, TurnContext};

use crate::context::ToolContext;
use crate::error::CapabilityError;
use crate::invoker::CapabilityInvoker;

fn turn_to_tool_context(ctx: &TurnContext) -> ToolContext {
    ToolContext {
        iteration_id: ctx.iteration_id,
        session_id: ctx.session_id.clone(),
    }
}

fn map_cap_error(err: CapabilityError) -> ToolError {
    match err {
        CapabilityError::NotFound => ToolError::NotFound("capability".into()),
        CapabilityError::NoMem => ToolError::NoMem,
        other => ToolError::InvokeFailed(other.to_string()),
    }
}

/// Builds LLM tool JSON for a turn (C: `claw_cap_build_llm_tools_json`).
pub trait ToolCatalogSource: Send + Sync {
    fn catalog(&self, ctx: &TurnContext) -> ToolCatalog;
}

/// [`Tools`] implementation delegating invoke to [`CapabilityInvoker`].
pub struct InvokerTools<I> {
    invoker: I,
    catalog: Arc<dyn ToolCatalogSource>,
}

impl<I> InvokerTools<I>
where
    I: CapabilityInvoker,
{
    pub fn new(invoker: I, catalog: Arc<dyn ToolCatalogSource>) -> Self {
        Self { invoker, catalog }
    }
}

impl<I> Tools for InvokerTools<I>
where
    I: CapabilityInvoker,
{
    fn catalog(&self, ctx: &TurnContext) -> ToolCatalog {
        self.catalog.catalog(ctx)
    }

    fn invoke(
        &self,
        call: &ToolInvocation<'_>,
        ctx: &TurnContext,
    ) -> Result<ToolOutput, ToolError> {
        invoke_with_invoker(&self.invoker, call, ctx)
    }
}

/// Object-safe [`Tools`] over [`CapabilityInvoker`].
pub struct InvokerToolsDyn {
    invoker: Arc<dyn CapabilityInvoker>,
    catalog: Arc<dyn ToolCatalogSource>,
}

impl InvokerToolsDyn {
    pub fn new(invoker: Arc<dyn CapabilityInvoker>, catalog: Arc<dyn ToolCatalogSource>) -> Self {
        Self { invoker, catalog }
    }
}

impl Tools for InvokerToolsDyn {
    fn catalog(&self, ctx: &TurnContext) -> ToolCatalog {
        self.catalog.catalog(ctx)
    }

    fn invoke(
        &self,
        call: &ToolInvocation<'_>,
        ctx: &TurnContext,
    ) -> Result<ToolOutput, ToolError> {
        invoke_with_invoker(self.invoker.as_ref(), call, ctx)
    }
}

fn invoke_with_invoker(
    invoker: &dyn CapabilityInvoker,
    call: &ToolInvocation<'_>,
    ctx: &TurnContext,
) -> Result<ToolOutput, ToolError> {
    let tool_ctx = turn_to_tool_context(ctx);
    let result = invoker
        .invoke(call.name, call.arguments_json, &tool_ctx)
        .map_err(map_cap_error)?;
    Ok(ToolOutput {
        output: result.output,
        ok: result.ok,
    })
}

/// Static empty catalog (tests / no tools).
pub struct EmptyToolCatalog;

impl ToolCatalogSource for EmptyToolCatalog {
    fn catalog(&self, _ctx: &TurnContext) -> ToolCatalog {
        ToolCatalog::default()
    }
}

/// Fixed JSON catalog for host tests.
pub struct StaticToolCatalog {
    pub llm_json: Option<String>,
}

impl ToolCatalogSource for StaticToolCatalog {
    fn catalog(&self, _ctx: &TurnContext) -> ToolCatalog {
        ToolCatalog {
            llm_json: self.llm_json.clone(),
        }
    }
}
