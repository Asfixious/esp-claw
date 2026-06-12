//! Layer 2 context assembly: role-agnostic prompt scaffolding.

use crate::agent::{AgentRole, AgentSpec, ContextBuildInput, ContextBundle, OutputSchema, SemanticPhase};
use crate::llm_output::schema_json_for_name;

// todo change this
/// Negative capabilities injected every LLM round (not RAG-dependent).
pub const NEGATIVE_CAPABILITIES: &str = "\
- No USB access\n\
- No physical hardware access\n\
- No unprovided credentials\n\
- Search is not a substitute for a missing capability\n";

/// Assembles shared prompt blocks; role-specific sections come from [`AgentSpec`].
pub struct ContextAssembler;

impl ContextAssembler {
    pub fn assemble(
        spec: &dyn AgentSpec,
        input: &ContextBuildInput<'_>,
        schema: &OutputSchema,
        tools_json: Option<&str>,
    ) -> ContextBundle {
        let phase = input.state.semantic_phase();
        let protocol = Self::protocol_block(input.state.role, phase, schema);
        let state_block = spec.state_block(input);
        let plan_block = spec.plan_block(input);
        let env_block = format!(
            "# ENV\n{}\n\n# Global policy\n{}\n\n# Negative capabilities\n{}",
            input.env_manifest, input.global_policy, NEGATIVE_CAPABILITIES
        );
        let memory_block = format!("# Memory snapshot\n{}", input.memory_snapshot);
        let tool_policy = Self::tool_policy_block(tools_json);

        let system_prompt = format!(
            "{protocol}\n\n{env_block}\n\n{state_block}\n\n{plan_block}\n\n{tool_policy}\n\n{memory_block}"
        );

        let messages = spec.conversation_messages(input);

        ContextBundle {
            system_prompt,
            messages,
            tools_json: tools_json.map(str::to_string),
        }
    }

    fn protocol_block(role: AgentRole, phase: SemanticPhase, schema: &OutputSchema) -> String {
        let json_schema = schema_json_for_name(&schema.name)
            .map(|s| format!("\njson_schema={s}"))
            .unwrap_or_default();
        format!(
            "# Protocol\nrole={role:?}\nphase={phase:?}\noutput_schema={}: {}{json_schema}\nRespond with a single JSON object matching json_schema.",
            schema.name, schema.description
        )
    }

    fn tool_policy_block(tools_json: Option<&str>) -> String {
        match tools_json {
            Some(tools) => format!("# Visible tools\n{tools}"),
            None => "# Visible tools\n(none)".to_string(),
        }
    }
}
