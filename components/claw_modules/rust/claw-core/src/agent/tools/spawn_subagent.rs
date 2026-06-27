//! `spawn_subagent(kind, goal, termination)` — request a child agent of `kind`
//! to work on `goal`.

use std::sync::Arc;

use claw_permission::{Action, RiskClass};
use claw_tool::{tool_metadata, ToolError, ToolHandler, ToolInvocation, ToolOutput};

use crate::agent::kind::AgentKind;
use crate::agent::graph::{AgentContext, SpawnPolicy, TerminationPolicy};
use crate::agent::manifest::AgentManifest;

use super::string_argument;

/// Parse the `spawn_subagent` tool's `termination` argument; empty defaults to
/// [`AutoOnIdle`](TerminationPolicy::AutoOnIdle).
///
/// # Errors
///
/// [`ToolError::InvokeFailed`] for any value other than `auto` / `manual`.
fn parse_termination(raw: &str) -> Result<TerminationPolicy, ToolError> {
    match raw.trim() {
        "" | "auto" => Ok(TerminationPolicy::AutoOnIdle),
        "manual" => Ok(TerminationPolicy::Manual),
        other => Err(ToolError::InvokeFailed(format!(
            "spawn_subagent 'termination' must be one of auto|manual, got '{other}'"
        ))),
    }
}

/// Creates a child agent (scoped to the parent's `allowed_kinds`) via the graph
/// context.
pub(crate) struct SpawnSubagentTool {
    context: Arc<AgentContext>,
    /// The parent kind's `allowed_kinds`, enforced before any spawn is requested.
    /// The matching menu the model reads up front is served by the sibling
    /// `list_spawnable_agents` tool, which renders the same policy's catalog.
    policy: SpawnPolicy,
}

impl SpawnSubagentTool {
    /// Build the tool over the agent's `context` and its spawn `policy`.
    pub(crate) fn new(context: Arc<AgentContext>, policy: SpawnPolicy) -> Self {
        Self { context, policy }
    }
}

impl ToolHandler for SpawnSubagentTool {
    tool_metadata!("spawn_subagent");

    fn classify(&self, _call: &ToolInvocation<'_>) -> Action {
        // Creating a child mutates the graph — worth a policy look, but reversible.
        Action::new("spawn_subagent", RiskClass::Moderate)
    }

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let kind = string_argument(call.arguments_json, "kind")?;
        if kind.trim().is_empty() {
            return Err(ToolError::InvokeFailed(
                "spawn_subagent requires a non-empty 'kind'".to_string(),
            ));
        }
        let kind = AgentKind::new(kind);

        // Enforce the manifest's `allowed_kinds`. A disallowed kind is refused with
        // a matched tool error (`ok = false`, like soft-hide gating) so the model
        // can pick a permitted kind or do the work itself — not as `Err`, which
        // would fail the whole iteration and rob the model of self-correction.
        if !self.policy.allows(&kind) {
            tracing::warn!(requested_kind = %kind, "spawn kind rejected by allowed_kinds");
            return Ok(ToolOutput {
                output: format!(
                    "spawn_subagent: kind '{kind}' is not permitted for this agent. \
                     Allowed: {}. This is a policy restriction, not a transient error: \
                     pick a permitted kind or handle the work yourself.",
                    self.policy.describe()
                ),
                ok: false,
            });
        }

        // The policy may permit the kind (notably `Any`, which permits any
        // *string*), but only a kind with a baked manifest can actually be built.
        // Without this guard a bogus kind would be reported as "spawned" here and
        // then silently dropped at `materialize_spawn`. Refuse it as a matched tool
        // error (like a disallowed kind) so the model can pick a real one.
        if AgentManifest::for_kind(kind.as_str()).is_none() {
            tracing::warn!(requested_kind = %kind, "spawn of an unknown (non-baked) kind refused");
            let available = self
                .policy
                .catalog()
                .iter()
                .map(|(agent_kind, _)| agent_kind.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let available = if available.is_empty() {
                "(none)".to_string()
            } else {
                available
            };
            return Ok(ToolOutput {
                output: format!(
                    "spawn_subagent: '{kind}' is not a known agent kind, so it cannot be \
                     created. Spawnable kinds: {available}. Call list_spawnable_agents to see \
                     what you can spawn."
                ),
                ok: false,
            });
        }

        // A name is required: it is the supervision label shown by list/watch so
        // the model can tell its subagents apart. Operations still key on the
        // returned id, never the name.
        let name = string_argument(call.arguments_json, "name")?;
        let name = name.trim();
        if name.is_empty() {
            return Err(ToolError::InvokeFailed(
                "spawn_subagent requires a non-empty 'name'".to_string(),
            ));
        }
        let name = name.to_string();

        let goal = string_argument(call.arguments_json, "goal")?;
        // Optional lifecycle policy: default one-shot (`auto`); `manual` keeps the
        // child alive and idle after it yields so this agent can supervise it.
        let termination = parse_termination(&string_argument(call.arguments_json, "termination")?)?;
        let child = self
            .context
            .spawn(kind, Some(name.clone()), goal, termination);
        Ok(ToolOutput {
            output: format!(
                "Subagent {child} named '{name}' requested; its result will be reported back when it finishes."
            ),
            ok: true,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{subagent_tool_group, test_support::tool_named};
    use super::*;
    use crate::agent::base_agent::AgentId;
    use crate::agent::graph::test_support::{context_for, spawned_kinds, RecordingHost};
    use crate::agent::graph::{GraphEffect, GraphHost};

    fn spawn_tool(host: Arc<RecordingHost>, policy: SpawnPolicy) -> claw_tool::Tool {
        let context = context_for(host as Arc<dyn GraphHost>, AgentId(1));
        tool_named(&subagent_tool_group(context, policy), "spawn_subagent")
    }

    #[test]
    fn termination_policy_parses_auto_manual_and_rejects_others() {
        assert_eq!(parse_termination("").unwrap(), TerminationPolicy::AutoOnIdle);
        assert_eq!(
            parse_termination("auto").unwrap(),
            TerminationPolicy::AutoOnIdle
        );
        assert_eq!(
            parse_termination("manual").unwrap(),
            TerminationPolicy::Manual
        );
        assert!(matches!(
            parse_termination("forever"),
            Err(ToolError::InvokeFailed(_))
        ));
    }

    #[test]
    fn spawn_rejects_disallowed_kind_without_spawning() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(
            Arc::clone(&host),
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"researcher","goal":"x"}"#,
            })
            .unwrap();

        // Refused as a matched tool error (not Err), and no spawn was emitted.
        assert!(!output.ok);
        assert!(output.output.contains("not permitted"));
        assert!(output.output.contains("worker"));
        assert!(host.effects.lock().unwrap().is_empty());
    }

    #[test]
    fn spawn_allows_permitted_kind() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(
            Arc::clone(&host),
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"worker","name":"w","goal":"x"}"#,
            })
            .unwrap();

        assert!(output.ok);
        assert_eq!(spawned_kinds(&host), &[AgentKind::new("worker")]);
    }

    #[test]
    fn spawn_policy_any_allows_any_baked_kind() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(Arc::clone(&host), SpawnPolicy::Any);

        // `conversation` is a baked kind outside any `Only` allow-set, so it
        // exercises `Any` permitting kinds beyond a fixed list.
        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"conversation","name":"chat","goal":"x"}"#,
            })
            .unwrap();

        assert!(output.ok);
        assert_eq!(spawned_kinds(&host), &[AgentKind::new("conversation")]);
    }

    #[test]
    fn spawn_carries_the_trimmed_name_into_the_effect() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(
            Arc::clone(&host),
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"worker","name":"  scout  ","goal":"x"}"#,
            })
            .unwrap();

        assert!(output.ok);
        // The id-based confirmation also echoes the name for the model.
        assert!(output.output.contains("named 'scout'"));
        let effects = host.effects.lock().unwrap();
        let name = effects
            .iter()
            .find_map(|(_, effect)| match effect {
                GraphEffect::Spawn { name, .. } => Some(name.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(name, Some("scout".to_string()));
    }

    #[test]
    fn spawn_requires_a_non_empty_name() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(
            Arc::clone(&host),
            SpawnPolicy::Only(vec![AgentKind::new("worker")]),
        );

        // Missing name -> hard tool error, nothing emitted.
        assert!(matches!(
            tool.invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"worker","goal":"x"}"#,
            }),
            Err(ToolError::InvokeFailed(_))
        ));

        // Whitespace-only name is also rejected.
        assert!(matches!(
            tool.invoke(&ToolInvocation {
                id: Some("t2"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"worker","name":"   ","goal":"x"}"#,
            }),
            Err(ToolError::InvokeFailed(_))
        ));

        assert!(host.effects.lock().unwrap().is_empty());
    }

    #[test]
    fn spawn_any_rejects_an_unknown_kind_without_spawning() {
        let host = Arc::new(RecordingHost::default());
        let tool = spawn_tool(Arc::clone(&host), SpawnPolicy::Any);

        // `Any` permits any string, but a non-baked kind cannot be built — it must
        // be refused here, not "spawned" and then silently dropped at materialize.
        let output = tool
            .invoke(&ToolInvocation {
                id: Some("t1"),
                name: "spawn_subagent",
                arguments_json: r#"{"kind":"ghost","goal":"x"}"#,
            })
            .unwrap();

        assert!(!output.ok);
        assert!(output.output.contains("not a known agent kind"));
        // The valid kinds are listed to steer the model.
        assert!(output.output.contains("worker"));
        // Nothing was emitted to the graph.
        assert!(host.effects.lock().unwrap().is_empty());
    }
}
