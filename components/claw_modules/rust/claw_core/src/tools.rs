//! Tools: model-callable units ([`Tool`]), named bundles ([`ToolGroup`]), and the
//! per-agent aggregate ([`ToolSet`]) the iteration loop consumes.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
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
    #[error("out of memory")]
    NoMem,
}

/// One model-callable tool: the full, concise shape a caller must provide.
///
/// Implement this once per tool. A collection of tools is assembled into a
/// [`ToolSet`] (which dispatches `invoke` by [`name`](ToolHandler::name) and
/// combines every [`schema`](ToolHandler::schema) into the JSON sent to the LLM).
pub trait ToolHandler: Send + Sync {
    /// The tool's name. Must match the `name` field inside [`schema`](Self::schema)
    /// and what the model emits in its `tool_call`.
    ///
    /// `&'static str` because tool names are compile-time identities: this lets a
    /// [`ToolSet`] key its dispatch map on the borrowed name with no allocation.
    fn name(&self) -> &'static str;

    /// This tool's OpenAI function schema as JSON text — one
    /// `{"type":"function", ...}` object.
    ///
    /// `&'static str` because the schema is a compile-time constant (typically a
    /// string literal). [`ToolSet`] splices these texts into the tools array
    /// without building or serializing any `serde_json::Value`. The returned text
    /// must be a valid JSON object; this is checked with a `debug_assert!` when
    /// the set is built.
    fn schema(&self) -> &'static str;

    /// Execute one model `tool_call` addressed to this tool.
    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError>;
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
    pub fn name(&self) -> &'static str {
        self.handler.name()
    }

    /// The tool's function schema as JSON text (delegates to the handler).
    pub fn schema(&self) -> &'static str {
        self.handler.schema()
    }

    /// Execute one `tool_call` (delegates to the handler).
    pub fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        self.handler.invoke(call)
    }
}

/// A named bundle of [`Tool`]s registered together.
///
/// A group is the unit of registration — its `name` is a `&'static str` group
/// identity (e.g. `"fs"`, `"web"`) attached to every tool it carries, for
/// provenance and logging. Grouping does *not* namespace dispatch: tool names
/// stay flat and globally unique across groups (see [`ToolSet::from_groups`]).
pub struct ToolGroup {
    name: &'static str,
    tools: Vec<Tool>,
}

impl ToolGroup {
    /// Bundle `tools` under the group identity `name`.
    pub fn new(name: &'static str, tools: impl IntoIterator<Item = Tool>) -> Self {
        Self {
            name,
            tools: tools.into_iter().collect(),
        }
    }

    /// The group's identity.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The tools in this group.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }
}

/// Group label for tools registered without an explicit [`ToolGroup`].
pub const DEFAULT_TOOL_GROUP: &str = "default";

/// One tool plus its group identity, as stored for dispatch.
struct Entry {
    tool: Tool,
    /// The [`ToolGroup`] this tool was registered under (provenance).
    group: &'static str,
}

/// Error from assembling a [`ToolSet`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ToolSetError {
    /// Two tools (within or across groups) share a name; dispatch is by flat
    /// name, so this would silently shadow one — rejected at construction.
    #[error("duplicate tool name across groups: {0}")]
    DuplicateToolName(&'static str),
}

/// A set of [`Tool`]s, organized by group, ready for one or more iterations.
///
/// Built once from groups; precomputes the combined schemas JSON and a flat
/// flat name→tool dispatch map. Keys are the tools' own `&'static str` names —
/// borrowed, never cloned — and each entry carries its group label (also
/// borrowed). Dispatch is O(1) and flat across all groups; the group is metadata
/// only. This is the aggregate the iteration loop consumes directly, via
/// [`schemas_json`](Self::schemas_json) and [`invoke`](Self::invoke).
pub struct ToolSet {
    by_name: HashMap<&'static str, Entry>,
    /// `[schema, schema, …]` serialized once, or `None` when empty.
    schemas_json: Option<String>,
}

impl ToolSet {
    /// Assemble ungrouped tools under [`DEFAULT_TOOL_GROUP`].
    ///
    /// See [`from_groups`](Self::from_groups) for the grouped form and the
    /// duplicate-name rule.
    pub fn new(tools: impl IntoIterator<Item = Tool>) -> Result<Self, ToolSetError> {
        Self::from_groups([ToolGroup::new(DEFAULT_TOOL_GROUP, tools)])
    }

    /// Assemble a set from named groups.
    ///
    /// Tool names must be globally unique across all groups; a collision returns
    /// [`ToolSetError::DuplicateToolName`] rather than silently shadowing a tool.
    /// The combined schema array is serialized once here so
    /// [`schemas_json`](Self::schemas_json) is free per request.
    pub fn from_groups(groups: impl IntoIterator<Item = ToolGroup>) -> Result<Self, ToolSetError> {
        let mut by_name: HashMap<&'static str, Entry> = HashMap::new();
        let mut schemas: Vec<&'static str> = Vec::new();
        for group in groups {
            let group_name = group.name;
            for tool in group.tools {
                let name = tool.name();
                if by_name.contains_key(name) {
                    return Err(ToolSetError::DuplicateToolName(name));
                }
                let schema = tool.schema();
                debug_assert!(
                    serde_json::from_str::<Value>(schema).is_ok_and(|value| value.is_object()),
                    "tool '{name}' returned an invalid JSON object schema: {schema}"
                );
                schemas.push(schema);
                by_name.insert(
                    name,
                    Entry {
                        tool,
                        group: group_name,
                    },
                );
            }
        }
        // Splice the per-tool schema texts into one `[obj, obj, …]` array — no
        // `Value` is built or serialized; claw-api parses the result once.
        let schemas_json = (!schemas.is_empty()).then(|| format!("[{}]", schemas.join(",")));
        Ok(Self {
            by_name,
            schemas_json,
        })
    }

    /// True when no tools were added.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The group a tool belongs to, or `None` if there is no such tool.
    pub fn group_of(&self, name: &str) -> Option<&'static str> {
        self.by_name.get(name).map(|entry| entry.group)
    }

    /// The combined OpenAI-style tools JSON array sent to the LLM, or `None` when
    /// there are no tools. Precomputed at construction, so this is a free borrow.
    pub fn schemas_json(&self) -> Option<&str> {
        self.schemas_json.as_deref()
    }

    /// Dispatch one model `tool_call` to its tool by name.
    ///
    /// Returns [`ToolError::NotFound`] when no tool owns `call.name`.
    pub fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        match self.by_name.get(call.name) {
            Some(entry) => entry.tool.invoke(call),
            None => Err(ToolError::NotFound(call.name.to_string())),
        }
    }
}

