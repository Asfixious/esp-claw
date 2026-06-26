//! The tool-execution seam: per-call gating and dispatch, isolated from the
//! iteration's orchestration (preemption checkpoints, tracing spans, and message
//! assembly stay in [`crate::iteration_loop`]).
//!
//! One call passes through three stages: **soft-hide** gating (is the tool
//! permitted this phase?), **permission** gating (does the policy allow / ask /
//! deny it?), then **execution**. The runner returns a neutral [`CallOutcome`]
//! the iteration loop turns into a tool message and a [`ToolRun`].
//!
//! [`ToolRun`]: crate::iteration_loop::ToolRun
//!
//! ## Async / concurrency seam
//!
//! Today every call runs synchronously, in the model's order. The shape here —
//! *classify → gate → execute*, with a per-tool [`concurrent`](ToolSet::concurrent)
//! hint surfaced via [`is_concurrent`](ToolRunner::is_concurrent) — is the seam a
//! future async runner grows into: side-effect-free `concurrent` calls awaited
//! together, serializing ones run in order. Keeping that decision *here* means the
//! iteration loop does not change when concurrency lands.

use crate::tools::{AllowedTools, ToolError, ToolInvocation, ToolOutput, ToolSet};
use claw_permission::{Action, PermissionDecision};

/// The permission seam the runner consults before executing a classified call.
///
/// Implemented by the agent layer that owns the permission policy, the grant
/// store, and the acting agent's identity — the runner stays agnostic of all
/// three and only asks "what is the verdict for this action?".
pub trait ToolGate {
    /// The permission verdict for the call described by `action`.
    fn decide(&self, action: &Action) -> PermissionDecision;
}

/// What an `Ask` decision needs the agent layer to remember to resolve it: the
/// human-facing `summary` and the action `signature` to grant/deny against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovalNeeded {
    /// Shown to the approver (the policy's reason).
    pub summary: String,
    /// The action signature a grant/denial is recorded under.
    pub signature: String,
}

/// The runner's verdict for one call, ready for the iteration loop to render.
pub(crate) struct CallOutcome {
    /// The tool-message content handed back to the model.
    pub content: String,
    /// Whether the call succeeded (false for blocked / denied / asked / a tool
    /// that ran and reported failure).
    pub ok: bool,
    /// True when refused by soft-hide gating (drives the retry-then-fail policy).
    pub blocked: bool,
    /// `Some` when the permission policy asked for human approval; the tool did
    /// not run and the agent layer must raise + later resolve the request.
    pub approval: Option<ApprovalNeeded>,
}

impl CallOutcome {
    /// A plain executed result.
    fn ran(content: String, ok: bool) -> Self {
        Self {
            content,
            ok,
            blocked: false,
            approval: None,
        }
    }
}

/// Gates and executes individual tool calls for one iteration. Cheap to build per
/// batch; borrows the tool set, the optional soft-hide allow-set, and the optional
/// permission gate.
pub(crate) struct ToolRunner<'a> {
    tools: &'a ToolSet,
    allowed: Option<&'a AllowedTools>,
    gate: Option<&'a dyn ToolGate>,
}

impl<'a> ToolRunner<'a> {
    /// Build a runner over `tools`, the soft-hide `allowed` set (`None` = ungated),
    /// and the permission `gate` (`None` = no permission layer; every call that
    /// passes soft-hide runs).
    pub(crate) fn new(
        tools: &'a ToolSet,
        allowed: Option<&'a AllowedTools>,
        gate: Option<&'a dyn ToolGate>,
    ) -> Self {
        Self {
            tools,
            allowed,
            gate,
        }
    }

    /// Whether `name`'s tool may run concurrently (the async-seam hint; unknown
    /// tools are treated as serializing).
    ///
    /// Reserved for the future async runner (see the module docs): today every
    /// call runs in order, so nothing consults this yet.
    #[allow(dead_code)]
    pub(crate) fn is_concurrent(&self, name: &str) -> bool {
        self.tools.concurrent(name).unwrap_or(false)
    }

    /// Gate `call` and, if permitted, execute it.
    ///
    /// # Errors
    ///
    /// Propagates [`ToolError`] from dispatch (e.g. an unknown tool or a tool that
    /// errored). Refusals (soft-hide / permission deny / ask) are *not* errors —
    /// they come back as a [`CallOutcome`] the model can react to.
    pub(crate) fn run_one(&self, call: &ToolInvocation<'_>) -> Result<CallOutcome, ToolError> {
        // 1. Soft-hide gating: the schema superset reached the model, but a tool
        //    not in `allowed` must not run this phase.
        if self.allowed.is_some_and(|allowed| !allowed.contains(call.name)) {
            return Ok(CallOutcome {
                content: blocked_tool_message(call.name),
                ok: false,
                blocked: true,
                approval: None,
            });
        }

        // 2. Permission gating: classify the call and ask the policy. An unknown
        //    tool cannot be classified — it falls through to dispatch, which
        //    returns the NotFound error.
        if let (Some(gate), Some(action)) = (self.gate, self.tools.classify(call)) {
            match gate.decide(&action) {
                PermissionDecision::Allow => {}
                PermissionDecision::Deny { reason } => {
                    return Ok(CallOutcome::ran(denied_tool_message(call.name, &reason), false));
                }
                PermissionDecision::Ask { reason } => {
                    return Ok(CallOutcome {
                        content: ask_tool_message(call.name),
                        ok: false,
                        blocked: false,
                        approval: Some(ApprovalNeeded {
                            summary: reason,
                            signature: action.signature(),
                        }),
                    });
                }
            }
        }

        // 3. Execute.
        let ToolOutput { output, ok } = self.tools.invoke(call)?;
        Ok(CallOutcome::ran(output, ok))
    }
}

/// Content handed to the model when soft-hide gating refuses a call. Worded so
/// the model treats it as a policy restriction (not a transient failure to
/// retry) and switches to a permitted tool.
pub(crate) fn blocked_tool_message(name: &str) -> String {
    let name = display_name(name);
    format!(
        "Tool \"{name}\" is not available in the current phase and was not executed. \
         This is a policy restriction, not a transient error: do not retry it. \
         Use one of the tools listed as available in the latest instructions instead."
    )
}

/// Content handed to the model when the permission policy denies a call.
fn denied_tool_message(name: &str, reason: &str) -> String {
    let name = display_name(name);
    format!(
        "Tool \"{name}\" was denied by policy and was not executed: {reason} \
         This is a policy restriction, not a transient error: do not retry it."
    )
}

/// Content handed to the model when the permission policy asks for approval. The
/// tool did not run; a human decision is pending and the model should wait.
fn ask_tool_message(name: &str) -> String {
    let name = display_name(name);
    format!(
        "Tool \"{name}\" requires human approval and was not executed yet. \
         A decision has been requested; wait for it before retrying."
    )
}

/// The display form of a (possibly empty) tool name.
fn display_name(name: &str) -> &str {
    if name.is_empty() {
        "(null)"
    } else {
        name
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolGroup, ToolHandler, ToolOutput};
    use claw_permission::{Action, RiskClass};

    /// A tool that records nothing and returns a fixed result; risk-classified so
    /// the permission path can be exercised.
    struct RiskyTool;
    impl ToolHandler for RiskyTool {
        fn name(&self) -> &'static str {
            "risky"
        }
        fn schema(&self) -> &'static str {
            r#"{"type":"function","function":{"name":"risky"}}"#
        }
        fn classify(&self, _call: &ToolInvocation<'_>) -> Action {
            Action::new("risky", RiskClass::High)
        }
        fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: "ran".into(),
                ok: true,
            })
        }
    }

    /// A gate scripted with one fixed decision.
    struct FixedGate(PermissionDecision);
    impl ToolGate for FixedGate {
        fn decide(&self, _action: &Action) -> PermissionDecision {
            self.0.clone()
        }
    }

    fn tools() -> ToolSet {
        ToolSet::from_groups([ToolGroup::new("g", [Tool::new(RiskyTool)])]).unwrap()
    }

    fn call() -> ToolInvocation<'static> {
        ToolInvocation {
            id: Some("t1"),
            name: "risky",
            arguments_json: "{}",
        }
    }

    #[test]
    fn allow_runs_the_tool() {
        let tools = tools();
        let gate = FixedGate(PermissionDecision::Allow);
        let runner = ToolRunner::new(&tools, None, Some(&gate));
        let outcome = runner.run_one(&call()).unwrap();
        assert_eq!(outcome.content, "ran");
        assert!(outcome.ok);
        assert!(outcome.approval.is_none());
    }

    #[test]
    fn deny_refuses_without_running() {
        let tools = tools();
        let gate = FixedGate(PermissionDecision::Deny {
            reason: "no".into(),
        });
        let runner = ToolRunner::new(&tools, None, Some(&gate));
        let outcome = runner.run_one(&call()).unwrap();
        assert!(!outcome.ok);
        assert!(outcome.approval.is_none());
        assert!(outcome.content.contains("denied by policy"));
    }

    #[test]
    fn ask_yields_approval_needed_without_running() {
        let tools = tools();
        let gate = FixedGate(PermissionDecision::Ask {
            reason: "confirm".into(),
        });
        let runner = ToolRunner::new(&tools, None, Some(&gate));
        let outcome = runner.run_one(&call()).unwrap();
        assert!(!outcome.ok);
        let approval = outcome.approval.expect("approval needed");
        assert_eq!(approval.summary, "confirm");
        assert_eq!(approval.signature, "risky");
    }

    #[test]
    fn soft_hide_blocks_before_permission() {
        let tools = tools();
        let allowed = AllowedTools::new(["other"]);
        // Even an Allow gate never runs: soft-hide refuses first.
        let gate = FixedGate(PermissionDecision::Allow);
        let runner = ToolRunner::new(&tools, Some(&allowed), Some(&gate));
        let outcome = runner.run_one(&call()).unwrap();
        assert!(outcome.blocked);
        assert!(!outcome.ok);
    }

    #[test]
    fn no_gate_runs_normally() {
        let tools = tools();
        let runner = ToolRunner::new(&tools, None, None);
        let outcome = runner.run_one(&call()).unwrap();
        assert_eq!(outcome.content, "ran");
        assert!(outcome.ok);
    }
}
