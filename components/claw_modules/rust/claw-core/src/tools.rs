//! Tools: model-callable units ([`Tool`]), named bundles ([`ToolGroup`]), and the
//! per-agent aggregate ([`ToolSet`]) the iteration loop consumes.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use claw_permission::{Action, RiskClass};
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

    /// This tool's soft-tools prompt prose, or `None` when it has none.
    ///
    /// Baked from `resources/tools/<name>/usage.md` (a blank file ⇒ `None`). This
    /// is the *prompt* surface — guidance the model reads — as opposed to
    /// [`schema`](Self::schema), the *API* surface. The default is `None` for
    /// hand-written handlers that opt out; the [`tool_metadata!`] macro overrides
    /// it from the baked file. The aggregate stitches every non-empty value into
    /// the tool-policy prompt block (assembled by `claw-context`).
    fn usage(&self) -> Option<Cow<'static, str>> {
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
/// against the crate root and stay `&'static str` constants — no runtime cost.
/// Only [`invoke`](ToolHandler::invoke) is then written by hand. The build step
/// (`manifest_gen/tool_bake.rs`) enforces that the directory holds exactly these
/// two files and that the directory name equals the schema's `function.name`.
macro_rules! tool_metadata {
    ($name:literal) => {
        fn name(&self) -> &'static str {
            $name
        }

        fn schema(&self) -> &'static str {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/tools/",
                $name,
                "/schema.json"
            ))
        }

        fn usage(&self) -> ::std::option::Option<::std::borrow::Cow<'static, str>> {
            const USAGE: &str = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/tools/",
                $name,
                "/usage.md"
            ));
            if USAGE.trim().is_empty() {
                ::std::option::Option::None
            } else {
                ::std::option::Option::Some(::std::borrow::Cow::Borrowed(USAGE))
            }
        }
    };
}
pub(crate) use tool_metadata;

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

    /// The tool's soft-tools prompt prose, if any (delegates to the handler).
    pub fn usage(&self) -> Option<Cow<'static, str>> {
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

/// Dev-time consistency check for a tool's schema, run when a [`ToolSet`] is
/// assembled. Verifies the schema text is a JSON object **and** that its
/// `function.name` matches the handler's [`name()`](ToolHandler::name) — the one
/// invariant the `tool_metadata!` macro cannot enforce at compile time (the macro
/// guarantees `name()` equals the schema filename, but not the `name` field
/// *inside* the JSON).
///
/// Every check is wrapped in `debug_assert*!`, so this is compiled out and costs
/// nothing in release builds; the JSON is parsed only under `cfg(debug_assertions)`.
fn debug_validate_schema(name: &str, schema: &str) {
    debug_assert!(
        serde_json::from_str::<Value>(schema).is_ok_and(|value| value.is_object()),
        "tool '{name}' returned an invalid JSON object schema: {schema}"
    );
    debug_assert_eq!(
        serde_json::from_str::<Value>(schema)
            .ok()
            .as_ref()
            .and_then(|value| value.get("function"))
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str),
        Some(name),
        "tool '{name}' schema function.name must match handler name(): {schema}"
    );
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
                debug_validate_schema(name, schema);
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

    /// An empty set — no tools, no schemas. The infallible base other groups are
    /// merged onto with [`extend_with_group`](Self::extend_with_group).
    pub fn empty() -> Self {
        Self {
            by_name: HashMap::new(),
            schemas_json: None,
        }
    }

    /// Merge another [`ToolGroup`] into this set, re-checking the flat-unique-name
    /// rule against the tools already present.
    ///
    /// Used to fold an agent's built-in (internal) tool group onto the caller's
    /// tools after construction. The combined schema array is rebuilt once here.
    ///
    /// # Errors
    ///
    /// [`ToolSetError::DuplicateToolName`] if a tool in `group` collides with one
    /// already in the set (or another in the same group); no tool is inserted past
    /// the collision and the existing schema array is left intact.
    pub fn extend_with_group(&mut self, group: ToolGroup) -> Result<(), ToolSetError> {
        let group_name = group.name;
        for tool in group.tools {
            let name = tool.name();
            if self.by_name.contains_key(name) {
                return Err(ToolSetError::DuplicateToolName(name));
            }
            let schema = tool.schema();
            debug_validate_schema(name, schema);
            self.by_name.insert(
                name,
                Entry {
                    tool,
                    group: group_name,
                },
            );
        }
        self.rebuild_schemas();
        Ok(())
    }

    /// Re-serialize the combined `[schema, …]` array from the current entries.
    fn rebuild_schemas(&mut self) {
        let schemas: Vec<&str> = self
            .by_name
            .values()
            .map(|entry| entry.tool.schema())
            .collect();
        self.schemas_json = (!schemas.is_empty()).then(|| format!("[{}]", schemas.join(",")));
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

    /// The soft-tools prompt section: every tool's [`usage`](ToolHandler::usage)
    /// prose, stitched into one Markdown block, or `None` when no tool carries
    /// usage. This is the *content* of the tool-policy prompt block; an assembler
    /// (`claw-context`) decides where it goes. The schema (the API surface) is the
    /// separate [`schemas_json`](Self::schemas_json) output.
    ///
    /// Tools are emitted in name order so the block is deterministic (the backing
    /// map is unordered), keeping the cached prompt prefix stable across builds.
    pub fn tool_context(&self) -> Option<String> {
        let mut sections: Vec<(&'static str, Cow<'static, str>)> = self
            .by_name
            .iter()
            .filter_map(|(name, entry)| entry.tool.usage().map(|usage| (*name, usage)))
            .collect();
        if sections.is_empty() {
            return None;
        }
        sections.sort_by_key(|(name, _)| *name);

        let mut rendered = String::from("# Tool usage");
        for (name, usage) in sections {
            rendered.push_str("\n\n## ");
            rendered.push_str(name);
            rendered.push('\n');
            rendered.push_str(usage.trim());
        }
        Some(rendered)
    }

    /// Classify `call` into a permission [`Action`] via its tool, or `None` when
    /// no tool owns `call.name` (an unknown call cannot be classified).
    pub fn classify(&self, call: &ToolInvocation<'_>) -> Option<Action> {
        self.by_name
            .get(call.name)
            .map(|entry| entry.tool.classify(call))
    }

    /// Whether the tool named `name` may run concurrently, or `None` when there is
    /// no such tool.
    pub fn concurrent(&self, name: &str) -> Option<bool> {
        self.by_name.get(name).map(|entry| entry.tool.concurrent())
    }

    /// Dispatch one model `tool_call` to its tool by name.
    ///
    /// Returns [`ToolError::NotFound`] when no tool owns `call.name`.
    pub fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        // Note: the per-call `toolcall` span is created one layer up, in
        // `iteration_loop::run_tool_calls`, so it also covers calls refused by
        // soft-hide gating (which never reach here).
        match self.by_name.get(call.name) {
            Some(entry) => entry.tool.invoke(call),
            None => Err(ToolError::NotFound(call.name.to_string())),
        }
    }
}

/// The set of tool names allowed to *execute* in the current phase ("soft-hide"
/// gating).
///
/// Soft-hide keeps the full [`ToolSet`] schema (the superset) in the prompt so
/// the cached `tools` prefix never changes, while restricting which of those
/// tools may actually run right now. The iteration loop consults this before
/// invoking each tool and refuses any call whose name is absent (the model is
/// handed a tool error instead). A `None` allow-set elsewhere means "ungated":
/// every tool in the set may run.
///
/// Names are `&'static str` to match [`ToolHandler::name`]; the allow-set is
/// built from the same compile-time identities.
///
/// # Examples
///
/// ```
/// use claw_core::AllowedTools;
///
/// let allowed = AllowedTools::new(["read_file", "list_dir"]);
/// assert!(allowed.contains("read_file"));
/// assert!(!allowed.contains("write_file"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowedTools {
    names: HashSet<&'static str>,
}

impl AllowedTools {
    /// Build an allow-set from a collection of permitted tool names.
    pub fn new(names: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }

    /// True when `name` is permitted to execute this phase.
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// True when no tool is permitted (an empty allow-set blocks everything).
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The permitted names in a stable (alphabetical) order.
    ///
    /// The backing set is unordered; sorting gives deterministic output for the
    /// phase note built from this allow-set (and for tests).
    ///
    /// Only the reserved soft-tools phase note consumes this today, so it is
    /// `dead_code` outside the gating tests until a phased agent reattaches.
    #[allow(dead_code)]
    pub(crate) fn sorted_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.names.iter().copied().collect();
        names.sort_unstable();
        names
    }
}

impl FromIterator<&'static str> for AllowedTools {
    fn from_iter<T: IntoIterator<Item = &'static str>>(iter: T) -> Self {
        Self {
            names: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A handler whose `name()` deliberately disagrees with the `function.name`
    /// inside its schema — the inconsistency `tool_metadata!` cannot catch.
    struct MismatchedNameTool;

    impl ToolHandler for MismatchedNameTool {
        fn name(&self) -> &'static str {
            "alpha"
        }

        fn schema(&self) -> &'static str {
            r#"{"type":"function","function":{"name":"beta","parameters":{"type":"object"}}}"#
        }

        fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: String::new(),
                ok: true,
            })
        }
    }

    /// In dev builds, assembling a set surfaces a `name()` vs schema `function.name`
    /// mismatch via the `debug_assert_eq!` in `debug_validate_schema`. Gated to
    /// `debug_assertions` because that check is compiled out of release builds.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "function.name must match handler name")]
    fn schema_name_mismatch_is_caught_in_dev() {
        let _ = ToolSet::new([Tool::new(MismatchedNameTool)]);
    }

    /// A tool carrying soft-tools usage prose, for the `tool_context` tests.
    struct UsageTool {
        name: &'static str,
        usage: Option<&'static str>,
    }

    impl ToolHandler for UsageTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn schema(&self) -> &'static str {
            // function.name must match name(); the leaked string keeps it 'static.
            Box::leak(
                format!(r#"{{"type":"function","function":{{"name":"{}"}}}}"#, self.name)
                    .into_boxed_str(),
            )
        }
        fn usage(&self) -> Option<Cow<'static, str>> {
            self.usage.map(Cow::Borrowed)
        }
        fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: String::new(),
                ok: true,
            })
        }
    }

    #[test]
    fn tool_context_is_none_when_no_tool_has_usage() {
        let set = ToolSet::new([Tool::new(UsageTool {
            name: "bare",
            usage: None,
        })])
        .unwrap();
        assert_eq!(set.tool_context(), None);
    }

    #[test]
    fn tool_context_stitches_usage_in_name_order() {
        // Inserted out of order; only the two with usage appear, sorted by name.
        let set = ToolSet::new([
            Tool::new(UsageTool {
                name: "zeta",
                usage: Some("Zeta does Z."),
            }),
            Tool::new(UsageTool {
                name: "alpha",
                usage: Some("Alpha does A."),
            }),
            Tool::new(UsageTool {
                name: "silent",
                usage: None,
            }),
        ])
        .unwrap();

        assert_eq!(
            set.tool_context().as_deref(),
            Some("# Tool usage\n\n## alpha\nAlpha does A.\n\n## zeta\nZeta does Z.")
        );
    }
}
