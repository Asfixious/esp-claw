//! Layer 2 context assembly: fixed per-iteration injection blocks.

use serde_json::{json, Value};

use crate::agent_spec::{AgentRole, AgentState, ContextBuildInput, ContextBundle, OutputSchema, RoleState, SemanticPhase};
use crate::llm_output::schema_json_for_name;

/// Negative capabilities injected every LLM round (not RAG-dependent).
pub const NEGATIVE_CAPABILITIES: &str = "\
- No USB access\n\
- No physical hardware access\n\
- No unprovided credentials\n\
- Search is not a substitute for a missing capability\n";

/// Assembles the full prompt and messages for one [`crate::iteration_loop`] step.
pub struct ContextAssembler;

impl ContextAssembler {
    pub fn assemble(input: &ContextBuildInput<'_>, schema: &OutputSchema, tools_json: Option<&str>) -> ContextBundle {
        let phase = input.state.semantic_phase();
        let protocol = Self::protocol_block(input.state.role, phase, schema);
        let state_block = Self::state_block(input.state);
        let plan_block = Self::plan_block(input.state, input.task);
        let env_block = format!(
            "# ENV\n{}\n\n# Global policy\n{}\n\n# Negative capabilities\n{}",
            input.env_manifest, input.global_policy, NEGATIVE_CAPABILITIES
        );
        let memory_block = format!("# Memory snapshot\n{}", input.memory_snapshot);
        let tool_policy = Self::tool_policy_block(tools_json);

        let system_prompt = format!(
            "{protocol}\n\n{env_block}\n\n{state_block}\n\n{plan_block}\n\n{tool_policy}\n\n{memory_block}"
        );

        let messages = Self::conversation_messages(input.state);

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

    fn state_block(state: &AgentState) -> String {
        match &state.role_state {
            RoleState::Frontend(frontend) => format!(
                "# STATE\ninstance={}\nrun={}\niteration={}\nphase={:?}\ntask_id={:?}\nworker={:?}",
                state.instance_id,
                state.run_id,
                state.iteration_id,
                frontend.phase,
                frontend.task_id,
                frontend.worker_instance_id,
            ),
            RoleState::Worker(worker) => format!(
                "# STATE\ninstance={}\nrun={}\niteration={}\nphase={:?}\ntask={}\nstep={:?}\nblocked={:?}",
                state.instance_id,
                state.run_id,
                state.iteration_id,
                worker.phase,
                worker.task_id,
                worker.current_step_id(),
                worker.blocked_reason,
            ),
        }
    }

    fn plan_block(state: &AgentState, task: Option<&crate::protocol::TaskContract>) -> String {
        let header = "# PLAN\n";
        match &state.role_state {
            RoleState::Worker(worker) => {
                let steps: Vec<String> = worker.plan_steps.iter().map(ToString::to_string).collect();
                let current = worker
                    .current_step_id()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "(none)".to_string());
                let revision = worker
                    .revision_note
                    .as_deref()
                    .map(|n| format!("\nrevision_note={n}"))
                    .unwrap_or_default();
                format!(
                    "{header}task={}\ncurrent_step={current}\nsteps={steps:?}{revision}",
                    worker.task_id
                )
            }
            RoleState::Frontend(_) => {
                if let Some(task) = task {
                    format!("{header}task={}\ngoal={}\nstatus={:?}", task.task_id, task.goal, task.status)
                } else {
                    format!("{header}(no active task)")
                }
            }
        }
    }

    fn tool_policy_block(tools_json: Option<&str>) -> String {
        match tools_json {
            Some(tools) => format!("# Visible tools\n{tools}"),
            None => "# Visible tools\n(none)".to_string(),
        }
    }

    fn conversation_messages(state: &AgentState) -> Value {
        match &state.role_state {
            RoleState::Frontend(frontend) => {
                let mut arr = Vec::new();
                for text in &frontend.pending_user_text {
                    arr.push(json!({"role": "user", "content": text}));
                }
                Value::Array(arr)
            }
            RoleState::Worker(worker) => worker.message_tail.clone(),
        }
    }
}
