//! [`WorkerAgent`] — a task-executing semantic agent over a [`BaseAgent`].
//!
//! A worker has a **fixed** tool set decided at build time (hard-hide): the model
//! sees and may use the same tools for the whole task, with no per-phase gating.
//! It runs a Plan -> Act -> Verify FSM whose stages are advanced by the model
//! calling `advance_stage`. Because the tool set never changes, the current stage
//! is conveyed by a per-stage guidance note (not by tool availability). A worker
//! can optionally spawn subagents (a build-time knob).

use std::sync::{Arc, Mutex};

use claw_api::{ClawApi, RetryPolicy};
use claw_memory::ConversationMemory;

use crate::agent::base_agent::{
    AgentCommand, AgentCommandError, AgentId, BaseAgent, BaseAgentBuildError, TickOutcome,
};
use crate::agent::internal_tools::{drain_goals, spawn_tool_group, SpawnSink};
use crate::agent::{append_child_result, Agent};
use crate::skills::SkillSet;
use crate::tools::{
    Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput, ToolSet, ToolSetError,
};

// -- Worker control tool (drives this agent's FSM) --------------------------
//
// A worker keeps a fixed tool set (hard-hide), so it cannot signal its stage
// through tool availability; the model advances stages explicitly by calling
// `advance_stage`, and the agent conveys the current stage via a per-stage
// guidance note. Like the other control tools, the handler only raises a signal.

/// Name of the `advance_stage` tool.
const ADVANCE_STAGE: &str = "advance_stage";
/// Group label for the worker control tool (provenance only).
const WORKER_TOOL_GROUP: &str = "worker";

/// Count of pending stage advances the worker drains each tick.
type AdvanceSink = Arc<Mutex<u32>>;

/// Build the worker control tool group over an advance sink.
fn worker_tool_group(sink: AdvanceSink) -> ToolGroup {
    ToolGroup::new(WORKER_TOOL_GROUP, [Tool::new(AdvanceStageTool { sink })])
}

/// Read and clear the count of pending advances.
fn take_advances(sink: &AdvanceSink) -> u32 {
    let mut count = sink.lock().unwrap_or_else(|poison| poison.into_inner());
    std::mem::replace(&mut count, 0)
}

/// `advance_stage()` — move to the next stage (Plan -> Act -> Verify).
struct AdvanceStageTool {
    sink: AdvanceSink,
}

impl ToolHandler for AdvanceStageTool {
    fn name(&self) -> &'static str {
        ADVANCE_STAGE
    }

    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"advance_stage","description":"Advance to the next stage of your work (Plan -> Act -> Verify). Call it when the current stage is complete.","parameters":{"type":"object","properties":{},"required":[]}}}"#
    }

    fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let mut count = self
            .sink
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *count = count.saturating_add(1);
        Ok(ToolOutput {
            output: "Advanced to the next stage.".to_string(),
            ok: true,
        })
    }
}

/// Built-in workflow guidance prepended to a worker's system prompt.
const WORKER_PREAMBLE: &str = "You complete your assigned goal in three stages. \
Plan: analyze the goal and outline your approach. \
Act: carry out the plan using your tools. \
Verify: check your work against the goal and fix any issues. \
Call advance_stage when the current stage is complete; a system note states the \
current stage before each step. When the goal is done, reply with your result. \
Use end_conversation only as a safety or ethics circuit-breaker, never for \
ordinary completion.";

/// The stages of a [`WorkerAgent`]'s work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerPhase {
    /// Analyze the goal and outline an approach.
    Plan,
    /// Carry out the plan.
    Act,
    /// Check the work against the goal.
    Verify,
}

impl WorkerPhase {
    /// The next stage; `Verify` is terminal in the stage progression.
    fn next(self) -> Self {
        match self {
            WorkerPhase::Plan => WorkerPhase::Act,
            WorkerPhase::Act => WorkerPhase::Verify,
            WorkerPhase::Verify => WorkerPhase::Verify,
        }
    }

    /// The per-stage guidance note shown to the model before each step.
    fn guidance(self) -> &'static str {
        match self {
            WorkerPhase::Plan => {
                "[system] Stage 1/3 — Plan: analyze the goal and outline your \
                 approach, then call advance_stage."
            }
            WorkerPhase::Act => {
                "[system] Stage 2/3 — Act: carry out your plan using your tools, \
                 then call advance_stage to verify."
            }
            WorkerPhase::Verify => {
                "[system] Stage 3/3 — Verify: check your work against the goal and \
                 fix any issues, then reply with your result."
            }
        }
    }
}

/// A worker agent: fixed tools, a Plan/Act/Verify FSM, optional spawning.
pub struct WorkerAgent {
    id: AgentId,
    base: BaseAgent,
    phase: WorkerPhase,
    /// Pending `advance_stage` calls, drained each tick.
    advance: AdvanceSink,
    /// Present iff this worker may spawn subagents.
    spawn: Option<SpawnSink>,
}

impl WorkerAgent {
    /// Start building a worker over a caller-owned LLM and memory.
    pub fn builder(id: AgentId, llm: ClawApi, memory: ConversationMemory) -> WorkerAgentBuilder {
        WorkerAgentBuilder {
            id,
            llm,
            memory,
            system_prompt: String::new(),
            tools: Vec::new(),
            skills: None,
            retry_policy: RetryPolicy::default(),
            tool_block_retries: None,
            spawn_enabled: false,
        }
    }

    /// Move to `phase` and publish its guidance note.
    ///
    /// The note goes in through the ordinary public `append_message` channel (the
    /// same one subagent results use) — a worker keeps a fixed tool set, so it
    /// cannot signal the stage via tool availability and instead states it in the
    /// transcript. Appended at the tail, it leaves the cached prefix intact.
    fn set_phase(&mut self, phase: WorkerPhase) {
        self.phase = phase;
        self.base.append_message(phase.guidance());
    }

    /// Turn one requested goal into a child agent.
    ///
    /// Implemented in the multi-agent phase, where the scheduler/registry that
    /// owns the agent graph is designed.
    fn spawn_subagent(&mut self, _goal: String) {
        todo!("subagent spawning is implemented in the multi-agent phase")
    }
}

impl Agent for WorkerAgent {
    fn id(&self) -> AgentId {
        self.id
    }

    fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError> {
        self.base.send_command(command)
    }

    fn deliver_child_result(&mut self, child: AgentId, text: String, ok: bool) {
        append_child_result(&mut self.base, child, text, ok);
    }

    fn tick(&mut self) -> TickOutcome {
        let outcome = self.base.tick();

        // Advance the stage once per `advance_stage` call made this tick.
        let advances = take_advances(&self.advance);
        for _ in 0..advances {
            let next = self.phase.next();
            self.set_phase(next);
        }

        // Drain any spawn requests recorded by the tool this tick.
        let goals = self.spawn.as_ref().map(drain_goals).unwrap_or_default();
        for goal in goals {
            self.spawn_subagent(goal);
        }

        // A delivered result or terminal outcome closes the task; the next one
        // starts back at Plan.
        let segment_closed = matches!(
            outcome,
            TickOutcome::Yielded { .. }
                | TickOutcome::Ended { .. }
                | TickOutcome::Cancelled { .. }
                | TickOutcome::Failed(_)
        );
        if segment_closed && self.phase != WorkerPhase::Plan {
            self.set_phase(WorkerPhase::Plan);
        }

        outcome
    }
}

/// Failure assembling a [`WorkerAgent`].
#[derive(Debug, thiserror::Error)]
pub enum WorkerAgentBuildError {
    /// Assembling the tool set failed (e.g. a duplicate tool name).
    #[error(transparent)]
    Tools(#[from] ToolSetError),
    /// The underlying [`BaseAgent`] could not be built.
    #[error(transparent)]
    Base(#[from] BaseAgentBuildError),
}

/// Builder for a [`WorkerAgent`].
#[must_use = "a WorkerAgentBuilder does nothing until `.build()` is called"]
pub struct WorkerAgentBuilder {
    id: AgentId,
    llm: ClawApi,
    memory: ConversationMemory,
    system_prompt: String,
    tools: Vec<Tool>,
    skills: Option<SkillSet>,
    retry_policy: RetryPolicy,
    tool_block_retries: Option<u32>,
    spawn_enabled: bool,
}

impl WorkerAgentBuilder {
    /// Set extra task guidance appended after the built-in Plan/Act/Verify
    /// workflow preamble.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Set the worker's fixed tools. The `advance_stage` control tool and the
    /// internal control tools (`end_conversation`, `request_approval`) are merged
    /// on automatically.
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools = tools.into_iter().collect();
        self
    }

    /// Set the worker's skills.
    pub fn with_skills(mut self, skills: SkillSet) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Override the per-iteration LLM [`RetryPolicy`].
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Override how many consecutive gating-blocked tool rounds to tolerate.
    pub fn with_tool_block_retries(mut self, retries: u32) -> Self {
        self.tool_block_retries = Some(retries);
        self
    }

    /// Allow this worker to spawn subagents (adds the `spawn_subagent` tool).
    pub fn with_spawning(mut self) -> Self {
        self.spawn_enabled = true;
        self
    }

    /// Finish configuration and produce a runnable [`WorkerAgent`].
    ///
    /// # Errors
    ///
    /// [`WorkerAgentBuildError`] when tool assembly or base construction fails.
    pub fn build(self) -> Result<WorkerAgent, WorkerAgentBuildError> {
        let advance: AdvanceSink = AdvanceSink::default();
        let spawn = self.spawn_enabled.then(SpawnSink::default);

        let mut tool_set = ToolSet::new(self.tools)?;
        tool_set.extend_with_group(worker_tool_group(advance.clone()))?;
        if let Some(sink) = &spawn {
            tool_set.extend_with_group(spawn_tool_group(sink.clone()))?;
        }

        let mut base_builder = BaseAgent::builder(self.llm, self.memory)
            .with_system_prompt(compose_prompt(&self.system_prompt))
            .with_tools(tool_set)
            .with_retry_policy(self.retry_policy);
        if let Some(retries) = self.tool_block_retries {
            base_builder = base_builder.with_tool_block_retries(retries);
        }
        if let Some(skills) = self.skills {
            base_builder = base_builder.with_skills(skills);
        }
        let base = base_builder.build()?;

        let mut agent = WorkerAgent {
            id: self.id,
            base,
            phase: WorkerPhase::Plan,
            advance,
            spawn,
        };
        // Publish the initial Plan-stage guidance before the first tick.
        agent.set_phase(WorkerPhase::Plan);
        Ok(agent)
    }
}

/// Prepend the built-in workflow preamble to the caller's system prompt.
fn compose_prompt(caller: &str) -> String {
    if caller.trim().is_empty() {
        WORKER_PREAMBLE.to_string()
    } else {
        format!("{WORKER_PREAMBLE}\n\n{caller}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use claw_api::{ClawApi, ClawApiConfig};
    use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
    use claw_interfaces::MemFs;
    use claw_memory::{
        CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
        MemoryTaskPool, PoolConfig,
    };
    use serde_json::{json, Value};

    use super::*;
    use crate::agent::CancelReason;

    // -- Host-test scaffolding (a scripted LLM + hermetic memory) -----------

    /// Serves scripted LLM bodies in order; panics if called past the script.
    struct ScriptedHttp {
        steps: Mutex<VecDeque<String>>,
    }

    impl ClawHttp for ScriptedHttp {
        fn post_json(
            &self,
            _request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            let body = self
                .steps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pop_front()
                .expect("ScriptedHttp: LLM called more times than scripted");
            Ok(HttpResponse {
                status_code: 200,
                body,
            })
        }
    }

    /// Never compacts.
    struct StubCompactor;

    impl Compactor for StubCompactor {
        fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
            Ok(Vec::new())
        }
    }

    /// Build a tool-capable LLM that replays `bodies` in order.
    fn scripted_llm(bodies: Vec<String>) -> ClawApi {
        ClawApi::init(
            ClawApiConfig {
                api_key: Some("sk-test".into()),
                backend_type: "openai_compatible".into(),
                model: Some("gpt-test".into()),
                base_url: Some("https://example.invalid".into()),
                supports_tools: true,
                ..Default::default()
            },
            Arc::new(ScriptedHttp {
                steps: Mutex::new(bodies.into_iter().collect()),
            }),
        )
        .expect("init llm")
    }

    /// A hermetic conversation memory keyed on `agent_id`.
    fn test_memory(agent_id: AgentId) -> ConversationMemory {
        let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("memory pool"));
        ConversationMemory::new(
            agent_id.0,
            ConversationConfig::new(format!("/mem/agent-{}", agent_id.0)),
            ConversationDeps {
                fs: Arc::new(MemFs::default()),
                pool,
                compactor: Arc::new(StubCompactor),
            },
        )
    }

    /// An LLM + a cloned read-only view of the same memory.
    fn llm_and_memory(
        agent_id: AgentId,
        bodies: Vec<String>,
    ) -> (ClawApi, ConversationMemory, ConversationMemory) {
        let memory = test_memory(agent_id);
        let view = memory.clone();
        (scripted_llm(bodies), memory, view)
    }

    /// A plain-text assistant answer body.
    fn body_plain_text(text: &str) -> String {
        json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }

    /// A single tool-call body.
    fn body_tool_call(id: &str, name: &str, arguments_json: &str) -> String {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": id,
                        "function": { "name": name, "arguments": arguments_json }
                    }]
                }
            }]
        })
        .to_string()
    }

    /// An `advance_stage()` tool-call body (Plan -> Act -> Verify).
    fn body_advance_stage(id: &str) -> String {
        body_tool_call(id, "advance_stage", "{}")
    }

    /// The `content` strings of every message currently in `memory`.
    fn transcript_contents(memory: &ConversationMemory) -> Vec<String> {
        memory
            .messages()
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|m| m.get("content").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Tick until a non-`Working` outcome.
    fn drive(agent: &mut WorkerAgent) -> TickOutcome {
        loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                other => return other,
            }
        }
    }

    #[test]
    fn starts_in_plan() {
        let (llm, memory, _view) = llm_and_memory(AgentId(1), Vec::new());
        let agent = WorkerAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();
        assert_eq!(agent.phase, WorkerPhase::Plan);
    }

    #[test]
    fn advance_stage_steps_plan_act_verify() {
        let (llm, memory, _view) = llm_and_memory(
            AgentId(1),
            vec![
                body_advance_stage("a1"),
                body_advance_stage("a2"),
                body_plain_text("result"),
            ],
        );
        let mut agent = WorkerAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("do it".into()))
            .unwrap();

        assert!(matches!(agent.tick(), TickOutcome::Working));
        assert_eq!(agent.phase, WorkerPhase::Act);
        assert!(matches!(agent.tick(), TickOutcome::Working));
        assert_eq!(agent.phase, WorkerPhase::Verify);

        // Final reply closes the task and resets to Plan.
        match agent.tick() {
            TickOutcome::Yielded { text } => assert_eq!(text, "result"),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(agent.phase, WorkerPhase::Plan);
    }

    #[test]
    fn append_runs_and_yields() {
        let (llm, memory, _view) = llm_and_memory(AgentId(1), vec![body_plain_text("hi")]);
        let mut agent = WorkerAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("go".into()))
            .unwrap();
        match drive(&mut agent) {
            TickOutcome::Yielded { text } => assert_eq!(text, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cancel_yields_cancelled() {
        let (llm, memory, _view) = llm_and_memory(AgentId(1), Vec::new());
        let mut agent = WorkerAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("go".into()))
            .unwrap();
        agent
            .send_command(AgentCommand::Cancel {
                reason: CancelReason::UserRequested,
            })
            .unwrap();
        match agent.tick() {
            TickOutcome::Cancelled { reason } => assert_eq!(reason, CancelReason::UserRequested),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn child_result_is_appended_as_message() {
        let (llm, memory, view) = llm_and_memory(AgentId(1), vec![body_plain_text("ack")]);
        let mut agent = WorkerAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent.deliver_child_result(AgentId(2), "done".into(), true);
        let _ = drive(&mut agent);

        assert!(
            transcript_contents(&view)
                .iter()
                .any(|c| c.contains("[subagent agent-2 ok] done")),
            "child result not present in memory"
        );
    }
}
