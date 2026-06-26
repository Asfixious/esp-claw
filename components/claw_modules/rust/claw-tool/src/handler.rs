//! Defining a tool: the [`ToolHandler`] trait, the [`tool_metadata!`] macro that
//! bakes its metadata from `resources/tools/<name>/`, and the cheap-to-clone
//! [`Tool`] value built from a handler.

use std::sync::Arc;

use claw_permission::{Action, RiskClass};
use thiserror::Error;

/// One model tool_call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolInvocation<'a> {
    pub id: Option<&'a str>,
    pub name: &'a str,
    pub arguments_json: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub output: String,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("tool invoke failed: {0}")]
    InvokeFailed(String),
}

/// One model-callable tool: the full, concise shape a caller must provide.
///
/// Implement this once per tool. A collection of tools is assembled into a
/// [`ToolSet`](crate::ToolSet) (which dispatches `invoke` by
/// [`name`](ToolHandler::name) and combines every [`schema`](ToolHandler::schema)
/// into the JSON sent to the LLM).
pub trait ToolHandler: Send + Sync {
    /// The tool's name. Must match the `name` field inside [`schema`](Self::schema)
    /// and what the model emits in its `tool_call`.
    ///
    /// Returns `&str` tied to `&self` — baked tools return a `&'static str` (which
    /// coerces), runtime-registered tools may return a field borrow.
    /// [`ToolSet`](crate::ToolSet) copies the name into an owned `String` key.
    fn name(&self) -> &str;

    /// This tool's OpenAI function schema as JSON text — one
    /// `{"type":"function", ...}` object.
    ///
    /// Returns `&str` tied to `&self` — typically a compile-time constant (string
    /// literal or `include_str!`). [`ToolSet`](crate::ToolSet) splices these texts
    /// into the tools array without building or serializing any
    /// `serde_json::Value`. The returned text must be a valid JSON object; this is
    /// checked with a `debug_assert!` when the set is built.
    fn schema(&self) -> &str;

    /// This tool's soft-tools prompt prose, or `None` when it has none.
    ///
    /// Baked from `resources/tools/<name>/usage.md` (a blank file ⇒ `None`). This
    /// is the *prompt* surface — guidance the model reads — as opposed to
    /// [`schema`](Self::schema), the *API* surface. The default is `None` for
    /// hand-written handlers that opt out; the [`tool_metadata!`] macro overrides
    /// it from the baked file. The aggregate stitches every non-empty value into
    /// the tool-policy prompt block (assembled by `claw-context`).
    fn usage(&self) -> Option<&str> {
        None
    }

    /// Whether this tool is safe to run *concurrently* with other tool calls in
    /// the same batch — true only when it has no observable side effects that
    /// could interleave badly (e.g. `web_search`, a pure read). The default is
    /// `false` (serialize), the safe choice: a tool that mutates shared state
    /// (e.g. `write_file` to the same path) must not race another call.
    fn concurrent(&self) -> bool {
        false
    }

    /// Describe what *this specific call* does as a permission [`Action`] (verb +
    /// optional resource + risk). The runtime evaluates it through the permission
    /// policy before invoking. The default is a [`Safe`](RiskClass::Safe) action
    /// keyed on the tool name; tools with side effects override this to raise the
    /// risk and name the resource they touch.
    fn classify(&self, _call: &ToolInvocation<'_>) -> Action {
        Action::new(self.name(), RiskClass::Safe)
    }

    /// Execute one model `tool_call` addressed to this tool.
    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError>;
}

/// Generate the `name()`, `schema()`, and `usage()` methods of a [`ToolHandler`]
/// impl from the tool's baked directory `resources/tools/<name>/`.
///
/// `name()` returns the given literal; `schema()` embeds, at compile time,
/// `resources/tools/<name>/schema.json`; `usage()` embeds
/// `resources/tools/<name>/usage.md` (a blank file ⇒ `None`). All paths resolve
/// against **the using crate's** `CARGO_MANIFEST_DIR` (where the macro is
/// invoked), and stay `&'static str` constants — no runtime cost. Only
/// [`invoke`](ToolHandler::invoke) is then written by hand. The build step
/// ([`bake`](crate::bake)) enforces that the directory holds exactly these two
/// files and that the directory name equals the schema's `function.name`.
#[macro_export]
macro_rules! tool_metadata {
    ($name:literal) => {
        fn name(&self) -> &str {
            $name
        }

        fn schema(&self) -> &str {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/tools/",
                $name,
                "/schema.json"
            ))
        }

        fn usage(&self) -> ::std::option::Option<&str> {
            const USAGE: &str = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/tools/",
                $name,
                "/usage.md"
            ));
            if USAGE.trim().is_empty() {
                ::std::option::Option::None
            } else {
                ::std::option::Option::Some(USAGE)
            }
        }
    };
}

/// A single tool as a cheap-to-clone value.
///
/// The handler lives behind an `Arc`, so cloning a `Tool` is a reference-count
/// bump — callers pass `Tool` by value (e.g. in an array) without paying for the
/// underlying implementation:
///
/// ```ignore
/// agent.with_tools([Tool::new(EchoTool), Tool::new(WeatherTool)]);
/// ```
#[derive(Clone)]
pub struct Tool {
    handler: Arc<dyn ToolHandler>,
}

impl Tool {
    /// Wrap a [`ToolHandler`] into a shareable `Tool` value.
    pub fn new(handler: impl ToolHandler + 'static) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }

    /// The tool's name (delegates to the handler).
    pub fn name(&self) -> &str {
        self.handler.name()
    }

    /// The tool's function schema as JSON text (delegates to the handler).
    pub fn schema(&self) -> &str {
        self.handler.schema()
    }

    /// The tool's soft-tools prompt prose, if any (delegates to the handler).
    pub fn usage(&self) -> Option<&str> {
        self.handler.usage()
    }

    /// Whether this tool may run concurrently in a batch (delegates to the handler).
    pub fn concurrent(&self) -> bool {
        self.handler.concurrent()
    }

    /// The permission [`Action`] this call represents (delegates to the handler).
    pub fn classify(&self, call: &ToolInvocation<'_>) -> Action {
        self.handler.classify(call)
    }

    /// Execute one `tool_call` (delegates to the handler).
    pub fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        self.handler.invoke(call)
    }
}
