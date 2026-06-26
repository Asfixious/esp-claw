//! LLM tool-list and catalog rendering, mirroring `claw_cap_build_llm_tools_json`
//! and `claw_cap_build_catalog`.

use serde_json::{json, Value};

use crate::context::CapabilityContext;
use crate::registry::Registry;

/// Maximum tool-description length in bytes before truncation (mirrors
/// `CLAW_CAP_TOOL_DESCRIPTION_MAX`).
const TOOL_DESCRIPTION_MAX: usize = 256;

impl Registry {
    /// Builds the JSON tool list of LLM-visible capabilities for `context`,
    /// mirroring `claw_cap_build_llm_tools_json`.
    ///
    /// When `wrap_for_responses_api` is `true`, each tool is wrapped as
    /// `{"type":"function","function":{...}}`; otherwise the raw
    /// `{"name","description","input_schema"}` form is emitted.
    pub fn build_llm_tools_json(
        &self,
        context: &CapabilityContext,
        wrap_for_responses_api: bool,
    ) -> String {
        let tools: Vec<Value> = self
            .visible_snapshots(context)
            .into_iter()
            .map(|snapshot| {
                let description = cap_description(snapshot.description.as_deref().unwrap_or(""));
                let input_schema = parse_input_schema(snapshot.input_schema_json.as_deref());
                if wrap_for_responses_api {
                    json!({
                        "type": "function",
                        "function": {
                            "name": snapshot.name,
                            "description": description,
                            "parameters": input_schema,
                        }
                    })
                } else {
                    json!({
                        "name": snapshot.name,
                        "description": description,
                        "input_schema": input_schema,
                    })
                }
            })
            .collect();

        Value::Array(tools).to_string()
    }

    /// Builds the human-readable capability catalog, mirroring
    /// `claw_cap_build_catalog`.
    pub fn build_catalog(&self) -> String {
        let mut catalog = String::from("Registered capabilities:\n");
        for snapshot in self.list() {
            let family = snapshot.family.as_deref().unwrap_or("cap");
            let description = snapshot.description.as_deref().unwrap_or("");
            catalog.push_str(&format!(
                "- {} [{}]: {}\n",
                snapshot.name, family, description
            ));
        }
        catalog
    }
}

/// Parses the schema string, defaulting to an empty object schema and falling
/// back to the same default when the string is not valid JSON.
fn parse_input_schema(input_schema_json: Option<&str>) -> Value {
    let default = || json!({ "type": "object", "properties": {} });
    match input_schema_json {
        Some(schema) => serde_json::from_str(schema).unwrap_or_else(|_| default()),
        None => default(),
    }
}

/// Truncates a description to at most [`TOOL_DESCRIPTION_MAX`] bytes on a UTF-8
/// char boundary, so the emitted JSON never contains a split codepoint.
fn cap_description(description: &str) -> String {
    if description.len() <= TOOL_DESCRIPTION_MAX {
        return description.to_string();
    }
    let mut end = TOOL_DESCRIPTION_MAX;
    while end > 0 && !description.is_char_boundary(end) {
        end -= 1;
    }
    description.get(..end).unwrap_or("").to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        CapabilityCaller, CapabilityDescriptor, CapabilityError, CapabilityFlags,
        CapabilityHandler, CapabilityInvokeResult, Registry,
    };

    struct Noop;
    impl CapabilityHandler for Noop {
        fn execute(
            &self,
            _input_json: &str,
            _context: &CapabilityContext,
        ) -> Result<CapabilityInvokeResult, CapabilityError> {
            Ok(CapabilityInvokeResult {
                output: String::new(),
                ok: true,
            })
        }
    }

    fn agent_ctx() -> CapabilityContext {
        CapabilityContext {
            caller: CapabilityCaller::Agent,
            ..Default::default()
        }
    }

    fn registry_with_one_tool() -> Registry {
        let registry = Registry::new(None);
        registry
            .register(
                CapabilityDescriptor::new("get_time", "get_time", Arc::new(Noop))
                    .with_family("time")
                    .with_description("Return the current time")
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM)
                    .with_input_schema(
                        r#"{"type":"object","properties":{"tz":{"type":"string"}}}"#,
                    ),
            )
            .unwrap();
        registry
    }

    #[test]
    fn raw_tools_json_shape() {
        let registry = registry_with_one_tool();
        let json = registry.build_llm_tools_json(&agent_ctx(), false);
        let value: Value = serde_json::from_str(&json).unwrap();
        let tool = &value.as_array().unwrap()[0];
        assert_eq!(tool["name"], "get_time");
        assert_eq!(tool["description"], "Return the current time");
        assert_eq!(tool["input_schema"]["properties"]["tz"]["type"], "string");
    }

    #[test]
    fn wrapped_tools_json_shape() {
        let registry = registry_with_one_tool();
        let json = registry.build_llm_tools_json(&agent_ctx(), true);
        let value: Value = serde_json::from_str(&json).unwrap();
        let tool = &value.as_array().unwrap()[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "get_time");
        assert_eq!(
            tool["function"]["parameters"]["properties"]["tz"]["type"],
            "string"
        );
    }

    #[test]
    fn missing_schema_defaults_to_empty_object() {
        let registry = Registry::new(None);
        registry
            .register(
                CapabilityDescriptor::new("bare", "bare", Arc::new(Noop))
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();
        let json = registry.build_llm_tools_json(&agent_ctx(), false);
        let value: Value = serde_json::from_str(&json).unwrap();
        let tool = &value.as_array().unwrap()[0];
        assert_eq!(tool["input_schema"]["type"], "object");
        assert!(tool["input_schema"]["properties"].is_object());
    }

    #[test]
    fn description_capped_on_char_boundary() {
        // Multi-byte chars so a naive byte cut would split a codepoint.
        let long = "\u{4e2d}".repeat(200); // 200 * 3 bytes = 600 bytes
        let capped = cap_description(&long);
        assert!(capped.len() <= TOOL_DESCRIPTION_MAX);
        // Still valid UTF-8 and a whole number of chars.
        assert_eq!(capped.len() % 3, 0);
    }

    #[test]
    fn catalog_lists_capabilities() {
        let registry = registry_with_one_tool();
        let catalog = registry.build_catalog();
        assert!(catalog.starts_with("Registered capabilities:\n"));
        assert!(catalog.contains("- get_time [time]: Return the current time"));
    }

    fn tool_names(json: &str) -> Vec<String> {
        let value: Value = serde_json::from_str(json).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn tools_exclude_non_llm_and_event_source() {
        use crate::CapabilityKind;
        let registry = Registry::new(None);
        // Visible: callable + CALLABLE_BY_LLM.
        registry
            .register(
                CapabilityDescriptor::new("visible", "visible", Arc::new(Noop))
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();
        // Hidden: no CALLABLE_BY_LLM flag.
        registry
            .register(CapabilityDescriptor::new(
                "internal",
                "internal",
                Arc::new(Noop),
            ))
            .unwrap();
        // Hidden: event source kind even with the flag set.
        registry
            .register(
                CapabilityDescriptor::new("events", "events", Arc::new(Noop))
                    .with_kind(CapabilityKind::EventSource)
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM),
            )
            .unwrap();

        let names = tool_names(&registry.build_llm_tools_json(&agent_ctx(), false));
        assert_eq!(names, vec!["visible".to_string()]);
    }

    #[test]
    fn tools_respect_global_visibility() {
        use crate::CapabilityGroup;
        let registry = Registry::new(None);
        registry
            .register_group(CapabilityGroup::new(
                "g1",
                "g1",
                "1",
                [CapabilityDescriptor::new("a", "a", Arc::new(Noop))
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM)],
            ))
            .unwrap();
        registry
            .register_group(CapabilityGroup::new(
                "g2",
                "g2",
                "1",
                [CapabilityDescriptor::new("b", "b", Arc::new(Noop))
                    .with_flags(CapabilityFlags::CALLABLE_BY_LLM)],
            ))
            .unwrap();
        registry.set_llm_visible_groups(["g1".to_string()]).unwrap();

        let names = tool_names(&registry.build_llm_tools_json(&agent_ctx(), false));
        assert_eq!(names, vec!["a".to_string()]);
    }
}
