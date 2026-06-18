//! [`ConversationAgent`] — a user-facing semantic agent over a [`BaseAgent`].
//!
//! It runs a five-stage FSM that drives **soft-hide** tool gating (the full tool
//! schema is always sent, so the cached prompt prefix never moves; only which
//! tools may *execute* changes per stage, which also signals the stage to the
//! model):
//!
//! ```text
//!   Intake --understood--> Verify --begin_execution--> Delegate
//!     ^                                                    |
//!     |                                              (spawn_subagent)
//!     |                                                    v
//!     +------- Report <--compile_results-- Watch <---------+
//!                 (Yields the final answer)        (loops: spawn more)
//! ```
//!
//! - **Intake**: understand the request (clarify if needed); `understood` -> Verify.
//! - **Verify**: confirm intent and feasibility; `begin_execution` -> Delegate
//!   (this unlocks spawning).
//! - **Delegate**: split the work and `spawn_subagent` each subtask; a spawn moves
//!   to Watch.
//! - **Watch**: integrate subagent results (spawn more if needed); `compile_results`
//!   -> Report.
//! - **Report**: synthesize the final answer (spawning locked); the `Yielded`
//!   answer returns to Intake.
//!
//! A `Yielded` from Intake / Verify / Report (or any terminal outcome) returns to
//! Intake. A `Yielded` from Delegate / Watch is a mid-flight progress note and
//! keeps the stage, so incoming subagent results continue the same execution.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use claw_api::{ClawApi, RetryPolicy};
use claw_memory::ConversationMemory;

use crate::agent::base_agent::{
    AgentCommand, AgentCommandError, AgentId, BaseAgent, BaseAgentBuildError, TickOutcome,
};
use crate::agent::internal_tools::{
    drain_goals, spawn_tool_group, string_argument, SpawnSink, END_CONVERSATION, REQUEST_APPROVAL,
    SPAWN_SUBAGENT,
};
use crate::agent::{append_child_result, Agent};
use crate::skills::SkillSet;
use crate::tools::{
    AllowedTools, Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput, ToolSet,
    ToolSetError,
};

// -- Conversation control tools (drive this agent's FSM) --------------------
//
// Each tool only *raises a signal* (it has `&self`, so it cannot touch agent
// state); the agent drains the signals each tick and advances its phase. The
// three forward edges of the FSM are model-driven:
//
//   understood      : Intake -> Verify
//   begin_execution : Verify -> Delegate (unlocks spawning)
//   compile_results : Watch  -> Report (locks spawning to synthesize)

/// Name of the `understood` tool (Intake -> Verify).
const UNDERSTOOD: &str = "understood";
/// Name of the `begin_execution` tool (Verify -> Delegate).
const BEGIN_EXECUTION: &str = "begin_execution";
/// Name of the `compile_results` tool (Watch -> Report).
const COMPILE_RESULTS: &str = "compile_results";
/// Group label for conversation-only control tools (provenance only).
const CONVERSATION_TOOL_GROUP: &str = "conversation";

/// A conversation FSM transition raised by a control tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvSignal {
    /// The request is understood (Intake -> Verify).
    Understood,
    /// The request is confirmed and feasible; begin executing (Verify -> Delegate).
    BeginExecution,
    /// Subagent results are in; synthesize the final answer (Watch -> Report).
    CompileResults,
}

/// Queue the conversation control tools push [`ConvSignal`]s onto; the agent
/// drains it each tick.
type ConvSink = Arc<Mutex<VecDeque<ConvSignal>>>;

/// Build the conversation control tool group over a signal sink.
fn conversation_tool_group(sink: ConvSink) -> ToolGroup {
    ToolGroup::new(
        CONVERSATION_TOOL_GROUP,
        [
            Tool::new(SignalTool {
                sink: Arc::clone(&sink),
                signal: ConvSignal::Understood,
                tool_name: UNDERSTOOD,
                tool_schema: r#"{"type":"function","function":{"name":"understood","description":"Signal that you fully understand the user's request and are ready to verify it. Provide a one-line restatement of the request.","parameters":{"type":"object","properties":{"summary":{"type":"string","description":"A one-line restatement of the request."}},"required":["summary"]}}}"#,
                ack: "Understanding confirmed; proceed to verify the request.",
            }),
            Tool::new(SignalTool {
                sink: Arc::clone(&sink),
                signal: ConvSignal::BeginExecution,
                tool_name: BEGIN_EXECUTION,
                tool_schema: r#"{"type":"function","function":{"name":"begin_execution","description":"Confirm the request is feasible and begin executing it (this unlocks delegating work to subagents). Provide a short plan.","parameters":{"type":"object","properties":{"plan":{"type":"string","description":"A short plan of how the task will be executed."}},"required":["plan"]}}}"#,
                ack: "Execution unlocked; delegate subtasks with spawn_subagent.",
            }),
            Tool::new(SignalTool {
                sink,
                signal: ConvSignal::CompileResults,
                tool_name: COMPILE_RESULTS,
                tool_schema: r#"{"type":"function","function":{"name":"compile_results","description":"Signal that the subagents' results are sufficient and you will now synthesize the final answer for the user.","parameters":{"type":"object","properties":{},"required":[]}}}"#,
                ack: "Synthesizing the final answer; spawning is now closed.",
            }),
        ],
    )
}

/// Drain all pending FSM signals, leaving the sink empty.
fn drain_signals(sink: &ConvSink) -> Vec<ConvSignal> {
    sink.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .drain(..)
        .collect()
}

/// A control tool that pushes a fixed [`ConvSignal`] when invoked. Its argument
/// (if any) is for the model's own reasoning — already captured in the recorded
/// tool call — so the handler only needs to raise the signal.
struct SignalTool {
    sink: ConvSink,
    signal: ConvSignal,
    tool_name: &'static str,
    tool_schema: &'static str,
    ack: &'static str,
}

impl ToolHandler for SignalTool {
    fn name(&self) -> &'static str {
        self.tool_name
    }

    fn schema(&self) -> &'static str {
        self.tool_schema
    }

    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        // Validate any provided arguments are well-formed JSON (surface malformed
        // calls rather than swallowing them); the value itself is unused.
        let _ = string_argument(call.arguments_json, "summary")?;
        self.sink
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(self.signal);
        Ok(ToolOutput {
            output: self.ack.to_string(),
            ok: true,
        })
    }
}

/// Built-in workflow guidance prepended to a conversation agent's system prompt.
const CONVERSATION_PREAMBLE: &str = "You handle each user request in stages, and \
the tools available to you reflect your current stage. \
Intake: understand the request (ask clarifying questions if needed), then call \
understood. \
Verify: confirm what you will do and that it is feasible (if it is not, explain \
why), then call begin_execution. \
Delegate: break the work into subtasks and delegate each one with spawn_subagent. \
Watch: integrate the subagents' results as they arrive (spawn more if needed), \
then call compile_results. \
Report: write the final answer for the user. \
Use end_conversation only as a safety or ethics circuit-breaker, never for \
ordinary completion.";

/// The five stages of a [`ConversationAgent`]'s FSM, observable via
/// [`ConversationAgent::phase`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvPhase {
    /// Understand the request; spawning gated off.
    Intake,
    /// Confirm intent and feasibility; spawning gated off.
    Verify,
    /// Split the work and spawn subagents.
    Delegate,
    /// Integrate subagent results; spawn more or compile.
    Watch,
    /// Synthesize the final answer; spawning gated off.
    Report,
}

impl std::fmt::Display for ConvPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ConvPhase::Intake => "Intake",
            ConvPhase::Verify => "Verify",
            ConvPhase::Delegate => "Delegate",
            ConvPhase::Watch => "Watch",
            ConvPhase::Report => "Report",
        };
        f.write_str(name)
    }
}

/// Per-stage soft-hide allow-sets, precomputed at build time.
struct PhaseAllows {
    intake: AllowedTools,
    verify: AllowedTools,
    delegate: AllowedTools,
    watch: AllowedTools,
    report: AllowedTools,
}

/// A user-facing conversation agent with a stage-gated tool FSM and optional
/// spawning.
pub struct ConversationAgent {
    id: AgentId,
    base: BaseAgent,
    /// Present iff this agent may spawn subagents.
    spawn: Option<SpawnSink>,
    /// FSM transition signals raised by the conversation control tools.
    signals: ConvSink,
    phase: ConvPhase,
    allows: PhaseAllows,
}

impl ConversationAgent {
    /// Start building a conversation agent over a caller-owned LLM and memory.
    pub fn builder(
        id: AgentId,
        llm: ClawApi,
        memory: ConversationMemory,
    ) -> ConversationAgentBuilder {
        ConversationAgentBuilder {
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

    /// The agent's current FSM stage (read-only; the agent owns transitions).
    pub fn phase(&self) -> ConvPhase {
        self.phase
    }

    /// The allow-set for a stage.
    fn allow_for(&self, phase: ConvPhase) -> &AllowedTools {
        match phase {
            ConvPhase::Intake => &self.allows.intake,
            ConvPhase::Verify => &self.allows.verify,
            ConvPhase::Delegate => &self.allows.delegate,
            ConvPhase::Watch => &self.allows.watch,
            ConvPhase::Report => &self.allows.report,
        }
    }

    /// Move to `phase` and apply its tool gating. Gating persists in the base
    /// between ticks, so it is only re-applied on a transition.
    fn set_phase(&mut self, phase: ConvPhase) {
        self.phase = phase;
        let allowed = self.allow_for(phase).clone();
        self.base.set_active_tools(allowed);
    }

    /// Apply one drained FSM signal if it is valid for the current stage.
    fn apply_signal(&mut self, signal: ConvSignal) {
        match (self.phase, signal) {
            (ConvPhase::Intake, ConvSignal::Understood) => self.set_phase(ConvPhase::Verify),
            (ConvPhase::Verify, ConvSignal::BeginExecution) => self.set_phase(ConvPhase::Delegate),
            (ConvPhase::Watch, ConvSignal::CompileResults) => self.set_phase(ConvPhase::Report),
            // A signal that does not match the current stage is ignored (the tool
            // that raises it is gated off outside its stage, so this is defensive).
            _ => {}
        }
    }

    /// Turn one requested goal into a child agent.
    ///
    /// Implemented in the multi-agent phase, where the scheduler/registry that
    /// owns the agent graph is designed.
    fn spawn_subagent(&mut self, _goal: String) {
        todo!("subagent spawning is implemented in the multi-agent phase")
    }
}

impl Agent for ConversationAgent {
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

        // FSM transitions raised by control tools this tick.
        for signal in drain_signals(&self.signals) {
            self.apply_signal(signal);
        }

        // A spawn during Delegate moves to Watch; further spawns stay in Watch.
        let goals = self.spawn.as_ref().map(drain_goals).unwrap_or_default();
        if !goals.is_empty() && self.phase == ConvPhase::Delegate {
            self.set_phase(ConvPhase::Watch);
        }
        for goal in goals {
            self.spawn_subagent(goal);
        }

        // A user-facing answer from a planning/report stage (or any terminal
        // outcome) closes the segment and returns to Intake. A Yielded from
        // Delegate/Watch is a mid-flight progress note and keeps the stage.
        let reset = match (&outcome, self.phase) {
            (
                TickOutcome::Ended { .. } | TickOutcome::Cancelled { .. } | TickOutcome::Failed(_),
                _,
            ) => true,
            (
                TickOutcome::Yielded { .. },
                ConvPhase::Intake | ConvPhase::Verify | ConvPhase::Report,
            ) => true,
            _ => false,
        };
        if reset && self.phase != ConvPhase::Intake {
            self.set_phase(ConvPhase::Intake);
        }

        outcome
    }
}

/// Failure assembling a [`ConversationAgent`].
#[derive(Debug, thiserror::Error)]
pub enum ConversationAgentBuildError {
    /// Assembling the tool set failed (e.g. a duplicate tool name).
    #[error(transparent)]
    Tools(#[from] ToolSetError),
    /// The underlying [`BaseAgent`] could not be built.
    #[error(transparent)]
    Base(#[from] BaseAgentBuildError),
}

/// Builder for a [`ConversationAgent`].
#[must_use = "a ConversationAgentBuilder does nothing until `.build()` is called"]
pub struct ConversationAgentBuilder {
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

impl ConversationAgentBuilder {
    /// Set extra persona/guidance appended after the built-in stage workflow
    /// preamble.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Set the caller-provided tools (read/inspect capabilities, etc.). The
    /// conversation control tools and internal control tools are added
    /// automatically.
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools = tools.into_iter().collect();
        self
    }

    /// Set the agent's skills.
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

    /// Allow this agent to spawn subagents (adds the `spawn_subagent` tool,
    /// executable in the Delegate/Watch stages).
    pub fn with_spawning(mut self) -> Self {
        self.spawn_enabled = true;
        self
    }

    /// Finish configuration and produce a runnable [`ConversationAgent`].
    ///
    /// # Errors
    ///
    /// [`ConversationAgentBuildError`] when tool assembly or base construction
    /// fails.
    pub fn build(self) -> Result<ConversationAgent, ConversationAgentBuildError> {
        let signals: ConvSink = ConvSink::default();
        let spawn = self.spawn_enabled.then(SpawnSink::default);

        let allows = phase_allows(&self.tools, spawn.is_some());

        let mut tool_set = ToolSet::new(self.tools)?;
        tool_set.extend_with_group(conversation_tool_group(signals.clone()))?;
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

        let mut agent = ConversationAgent {
            id: self.id,
            base,
            spawn,
            signals,
            phase: ConvPhase::Intake,
            allows,
        };
        // Apply the initial Intake gating before the first tick.
        agent.set_phase(ConvPhase::Intake);
        Ok(agent)
    }
}

/// Compute the per-stage allow-sets from the caller tools and whether spawning is
/// enabled. Caller (read/inspect) tools are permitted in every stage; the control
/// tools are added per stage so tool availability tracks the FSM.
fn phase_allows(caller_tools: &[Tool], spawn_enabled: bool) -> PhaseAllows {
    let caller: Vec<&'static str> = caller_tools.iter().map(Tool::name).collect();
    let with = |extra: &[&'static str]| -> AllowedTools {
        let mut names = caller.clone();
        names.extend_from_slice(extra);
        AllowedTools::new(names)
    };
    let spawn: &[&'static str] = if spawn_enabled {
        &[SPAWN_SUBAGENT]
    } else {
        &[]
    };
    let mut delegate_extra = vec![END_CONVERSATION, REQUEST_APPROVAL];
    delegate_extra.extend_from_slice(spawn);
    let mut watch_extra = vec![COMPILE_RESULTS, END_CONVERSATION, REQUEST_APPROVAL];
    watch_extra.extend_from_slice(spawn);

    PhaseAllows {
        intake: with(&[UNDERSTOOD, END_CONVERSATION, REQUEST_APPROVAL]),
        verify: with(&[BEGIN_EXECUTION, END_CONVERSATION, REQUEST_APPROVAL]),
        delegate: with(&delegate_extra),
        watch: with(&watch_extra),
        report: with(&[END_CONVERSATION, REQUEST_APPROVAL]),
    }
}

/// Prepend the built-in workflow preamble to the caller's system prompt.
fn compose_prompt(caller: &str) -> String {
    if caller.trim().is_empty() {
        CONVERSATION_PREAMBLE.to_string()
    } else {
        format!("{CONVERSATION_PREAMBLE}\n\n{caller}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use claw_api::{ClawApi, ClawApiConfig};
    use claw_interfaces::{MemFs, ScriptedHttp};
    use claw_memory::{
        ConversationConfig, ConversationDeps, ConversationMemory, MemoryTaskPool, NoopCompactor,
        PoolConfig,
    };
    use serde_json::{json, Value};

    use super::*;

    // -- Host-test scaffolding (a scripted LLM + hermetic memory) -----------
    // `ScriptedHttp` (httpmock feature) and `NoopCompactor` (compactor-stub
    // feature) are shared from claw_interfaces / claw-memory.

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
            Arc::new(ScriptedHttp::new(bodies)),
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
                compactor: Arc::new(NoopCompactor),
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

    /// An `understood(summary)` tool-call body (Intake -> Verify).
    fn body_understood(summary: &str) -> String {
        body_tool_call(
            "u1",
            "understood",
            &json!({ "summary": summary }).to_string(),
        )
    }

    /// A `begin_execution(plan)` tool-call body (Verify -> Delegate).
    fn body_begin_execution(plan: &str) -> String {
        body_tool_call(
            "b1",
            "begin_execution",
            &json!({ "plan": plan }).to_string(),
        )
    }

    /// An `end_conversation(final_message)` tool-call body.
    fn body_end_conversation(final_message: &str) -> String {
        body_tool_call(
            "e1",
            "end_conversation",
            &json!({ "final_message": final_message }).to_string(),
        )
    }

    /// A `spawn_subagent(goal)` tool-call body.
    fn body_spawn(id: &str, goal: &str) -> String {
        body_tool_call(id, "spawn_subagent", &json!({ "goal": goal }).to_string())
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

    /// A harmless read-style caller tool, allowed in every stage.
    struct ReadTool;

    impl ToolHandler for ReadTool {
        fn name(&self) -> &'static str {
            "read_thing"
        }
        fn schema(&self) -> &'static str {
            r#"{"type":"function","function":{"name":"read_thing","description":"Read"}}"#
        }
        fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: "read".into(),
                ok: true,
            })
        }
    }

    /// Tick until a non-`Working` outcome.
    fn drive(agent: &mut ConversationAgent) -> TickOutcome {
        loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                other => return other,
            }
        }
    }

    #[test]
    fn starts_in_intake() {
        let (llm, memory, _view) = llm_and_memory(AgentId(1), Vec::new());
        let agent = ConversationAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();
        assert_eq!(agent.phase, ConvPhase::Intake);
    }

    #[test]
    fn intake_can_answer_directly_and_stays_intake() {
        let (llm, memory, _view) = llm_and_memory(AgentId(1), vec![body_plain_text("here you go")]);
        let mut agent = ConversationAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("question".into()))
            .unwrap();
        match drive(&mut agent) {
            TickOutcome::Yielded { text } => assert_eq!(text, "here you go"),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(agent.phase, ConvPhase::Intake);
    }

    #[test]
    fn understood_then_begin_execution_walks_intake_verify_delegate() {
        let (llm, memory, _view) = llm_and_memory(
            AgentId(1),
            vec![
                body_understood("user wants X"),
                body_begin_execution("plan"),
            ],
        );
        let mut agent = ConversationAgent::builder(AgentId(1), llm, memory)
            .with_spawning()
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("go".into()))
            .unwrap();

        assert!(matches!(agent.tick(), TickOutcome::Working));
        assert_eq!(agent.phase, ConvPhase::Verify);
        assert!(matches!(agent.tick(), TickOutcome::Working));
        assert_eq!(agent.phase, ConvPhase::Delegate);
    }

    /// In Intake, `spawn_subagent` is gated off: the call is refused with a
    /// matched tool error (never reaching the spawn stub), and the agent keeps
    /// working within the retry budget.
    #[test]
    fn spawn_is_blocked_in_intake() {
        let (llm, memory, view) = llm_and_memory(
            AgentId(1),
            vec![
                body_spawn("s1", "sub task"),
                body_end_conversation("stopping"),
            ],
        );
        let mut agent = ConversationAgent::builder(AgentId(1), llm, memory)
            .with_tools([Tool::new(ReadTool)])
            .with_spawning()
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("go".into()))
            .unwrap();
        match drive(&mut agent) {
            TickOutcome::Ended { final_message } => assert_eq!(final_message, "stopping"),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(agent.phase, ConvPhase::Intake);
        assert!(
            transcript_contents(&view)
                .iter()
                .any(|c| c.contains("not available in the current phase")),
            "spawn should have been gated off in Intake"
        );
    }

    #[test]
    fn end_conversation_ends() {
        let (llm, memory, _view) =
            llm_and_memory(AgentId(1), vec![body_end_conversation("goodbye")]);
        let mut agent = ConversationAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("go".into()))
            .unwrap();
        match drive(&mut agent) {
            TickOutcome::Ended { final_message } => assert_eq!(final_message, "goodbye"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn child_result_is_appended_as_message() {
        let (llm, memory, view) = llm_and_memory(AgentId(1), vec![body_plain_text("ack")]);
        let mut agent = ConversationAgent::builder(AgentId(1), llm, memory)
            .build()
            .unwrap();

        agent.deliver_child_result(AgentId(7), "subtask output".into(), true);
        let _ = drive(&mut agent);

        assert!(
            transcript_contents(&view)
                .iter()
                .any(|c| c.contains("[subagent agent-7 ok] subtask output")),
            "child result not present in memory"
        );
    }
}
