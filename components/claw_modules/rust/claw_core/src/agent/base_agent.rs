//! Layer 2 base agent: drives a multi-iteration task, one iteration per `tick`.
//!
//! Split of responsibilities:
//! - [`BaseAgent::new`] takes the agent's owned LLM client, the shared memory
//!   task pool singleton, and a [`BaseAgentConfig`] that controls conversation
//!   identity and persistence. The agent builds its [`ConversationMemory`]
//!   internally.
//! - [`BaseAgent::run`] is pure setup for one task: it injects the per-task
//!   context (system prompt, tools, skills), appends the user's goal to memory,
//!   and resets iteration state. No iteration runs here.
//! - The driver (orchestrator) then pumps [`BaseAgent::tick`] until it returns a
//!   terminal [`BaseAgentState`]. Each `tick` performs exactly one
//!   [`IterationLoop`] round-trip, so the driver can interleave new commands and
//!   other agents between ticks (cooperative scheduling).
//!
//! The agent **owns** its LLM client, interrupt flag, conversation memory, and
//! the per-task context set by `run` (system prompt, tools, skills), so it
//! carries no lifetime parameter and can be stored and driven over time. Only the
//! goal in [`RunParams`] is borrowed, just for seeding.
//!
//! Outbound results ride on the value [`tick`](BaseAgent::tick) returns — one
//! user-facing output per tick. Internal tool-round messages are *context*, not
//! output: they are committed to memory and fed back in on the next iteration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use claw_api::ClawApi;
use claw_interfaces::ClawFs;
use claw_memory::{
    Compactor, ConversationConfig, ConversationDeps, ConversationMemory, GroupGuard, MemoryTaskPool,
};

use crate::iteration_loop::{
    ChatMessages, CompletedKind, InterruptionControl, IterationId, IterationLoop,
    IterationLoopError, IterationOutcome, IterationStep, IterationTools, PreemptedOutcome,
    SystemPrompt,
};
use crate::skills::Skills;

crate::define_prefixed_id!(AgentId, "agent-", "agent");

/// Result of a single [`BaseAgent::tick`].
///
/// This is the agent's *outbound* surface: at most one user-facing output per
/// tick. The driver pumps `tick` while it returns [`Working`](BaseAgentState::Working)
/// and stops on a terminal variant.
#[derive(Clone, Debug)]
pub enum BaseAgentState {
    /// No task loaded (or the previous task already finished). Nothing to do.
    Idle,
    /// The iteration advanced but produced nothing user-facing this tick
    /// (e.g. an internal tool round). Keep ticking.
    Working,
    /// This tick's iteration was preempted (an interrupt landed). The agent has
    /// merged what was produced and will rebuild context on the next tick — the
    /// task is *not* over, so keep ticking.
    Interrupted,
    /// Terminal: the task finished with this final answer.
    Completed(String),
    /// Terminal: the task failed; carries the iteration cause.
    Failed(IterationLoopError),
}

/// Internal lifecycle, kept separate from the rich [`BaseAgentState`] so it stays
/// `Copy` and can gate `tick`/`run` without owning any payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Idle,
    Running,
    Terminal,
}

/// A cloneable handle for raising an agent's interrupt flag from another task.
///
/// Obtain it via [`BaseAgent::interrupt_handle`] **before** entering the tick
/// loop: you cannot borrow the agent while a `tick` holds `&mut self`. Once held,
/// it can be raised from anywhere — including while a `tick` is blocked on the
/// LLM HTTP — because it shares the same `Arc<AtomicBool>` that
/// [`IterationLoop`] polls at its preemption checkpoints.
#[derive(Clone)]
pub struct AgentInterruptHandle {
    flag: Arc<AtomicBool>,
}

impl AgentInterruptHandle {
    /// Request cooperative interruption of the in-flight (or next) iteration.
    ///
    /// The current iteration ends preempted (terminal); the agent merges what was
    /// produced, rebuilds context, and continues on the next `tick`.
    pub fn interrupt(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// The agent's own interrupt flag, fed to the per-tick [`IterationLoop`].
///
/// The agent owns this (it is the thing that gets interrupted); callers never
/// supply it. [`BaseAgent::interrupt_handle`] hands out cloned references.
struct AgentInterruption {
    flag: Arc<AtomicBool>,
}

impl InterruptionControl for AgentInterruption {
    fn interrupt_flag(&self) -> &Arc<AtomicBool> {
        &self.flag
    }
}

/// Error from [`BaseAgent::run`] (task setup only).
///
/// Per the 1-to-1 error rule, this carries only what `run` can actually return.
/// Iteration failures are *not* here — they surface as
/// [`BaseAgentState::Failed`] from a `tick`.
#[derive(Clone, Debug, thiserror::Error)]
pub enum AgentError {
    #[error("run() called while a task is still in progress")]
    Busy,
}

/// Configuration for a [`BaseAgent`]'s conversation memory.
pub struct BaseAgentConfig {
    /// Agent identity, used as the key for the conversation transcript files.
    pub agent_id: AgentId,
    /// Base directory for per-conversation files.
    pub memory_dir: String,
    /// Filesystem backend for persisting the conversation transcript.
    pub fs: Arc<dyn ClawFs>,
    /// Compactor for background conversation summarization.
    pub compactor: Arc<dyn Compactor>,
}

/// Per-task input for [`BaseAgent::run`].
///
/// The owned fields are moved into the agent (used across the task's ticks); the
/// goal is borrowed only for the duration of `run` (copied into the working
/// context). Per-task memories will live here.
pub struct RunParams<'a> {
    /// The task goal / initial user prompt.
    pub goal: &'a str,
    /// System prompt / instructions for the task.
    pub system_prompt: String,
    /// Tools available for the task, if any.
    pub tools: Option<Arc<dyn IterationTools>>,
    /// Skill context provider, if any.
    pub skills: Option<Arc<dyn Skills>>,
}

/// A base agent that runs one task at a time as a sequence of iterations.
pub struct BaseAgent {
    // --- owned capabilities, fixed at construction ---
    llm: ClawApi,
    interruption: AgentInterruption,
    memory: ConversationMemory,
    system_prompt: String,
    tools: Option<Arc<dyn IterationTools>>,
    skills: Option<Arc<dyn Skills>>,
    /// Open group guard started in `run()` with the user's goal appended.
    /// Kept alive until the first response lands so user+reply commit as one
    /// group — preventing compaction from orphaning a reply with no user turn.
    open_turn: Option<GroupGuard>,
    next_iteration: IterationId,
    lifecycle: Lifecycle,
}

impl BaseAgent {
    pub fn new(llm: ClawApi, pool: Arc<MemoryTaskPool>, config: BaseAgentConfig) -> Self {
        let memory = ConversationMemory::new(
            config.agent_id.0,
            ConversationConfig::new(config.memory_dir),
            ConversationDeps {
                fs: config.fs,
                pool,
                compactor: config.compactor,
            },
        );
        Self::with_memory(llm, memory)
    }

    /// Construct with a caller-owned [`ConversationMemory`].
    ///
    /// Use this when the caller needs to inspect the conversation without going
    /// through `BaseAgent`. Clone the memory before passing it in — the clone
    /// shares the same live `Arc`-backed state:
    ///
    /// ```ignore
    /// let memory = ConversationMemory::new(agent_id, config, deps);
    /// let view = memory.clone();
    /// let agent = BaseAgent::with_memory(llm, memory);
    /// // later:
    /// let messages = view.messages();
    /// ```
    pub fn with_memory(llm: ClawApi, memory: ConversationMemory) -> Self {
        Self {
            llm,
            interruption: AgentInterruption {
                flag: Arc::new(AtomicBool::new(false)),
            },
            memory,
            system_prompt: String::new(),
            tools: None,
            skills: None,
            open_turn: None,
            next_iteration: IterationId(0),
            lifecycle: Lifecycle::Idle,
        }
    }

    /// True while a task is loaded and still has iterations to run.
    pub fn is_running(&self) -> bool {
        self.lifecycle == Lifecycle::Running
    }

    /// A handle to interrupt this agent from another task.
    ///
    /// Grab it before the tick loop starts (see [`AgentInterruptHandle`]); the
    /// returned handle shares the agent's underlying interrupt flag, so raising
    /// it cancels an in-flight `tick` cooperatively.
    pub fn interrupt_handle(&self) -> AgentInterruptHandle {
        AgentInterruptHandle {
            flag: Arc::clone(&self.interruption.flag),
        }
    }

    /// Load per-task context and reset the working state. No iteration runs here.
    ///
    /// Returns [`AgentError::Busy`] if a previous task is still running.
    pub fn run(&mut self, params: RunParams<'_>) -> Result<(), AgentError> {
        if self.lifecycle == Lifecycle::Running {
            return Err(AgentError::Busy);
        }
        self.system_prompt = params.system_prompt;
        self.tools = params.tools;
        self.skills = params.skills;
        self.next_iteration = IterationId(0);
        let turn = self.memory.group();
        turn.append_user(params.goal);
        self.open_turn = Some(turn);
        self.lifecycle = Lifecycle::Running;
        Ok(())
    }

    /// Advance exactly one iteration and return its outbound result.
    ///
    /// A no-op returning [`BaseAgentState::Idle`] unless the agent is running.
    pub fn tick(&mut self) -> BaseAgentState {
        if self.lifecycle != Lifecycle::Running {
            return BaseAgentState::Idle;
        }

        let iteration_id = self.next_iteration;
        self.next_iteration = IterationId(iteration_id.0.saturating_add(1));

        let messages = self.memory.messages();

        let outcome = {
            let iteration_loop = IterationLoop {
                llm: &self.llm,
                interruption: &self.interruption,
            };
            let step = IterationStep {
                iteration_id,
                system_prompt: SystemPrompt(self.system_prompt.as_str()),
                messages: ChatMessages(&messages),
                tools: self.tools.as_deref(),
            };
            iteration_loop.run(step)
        };

        match outcome {
            Ok(IterationOutcome::Completed(outcome)) => self.on_completed(outcome.kind),
            Ok(IterationOutcome::Preempted(outcome)) => self.on_preempted(outcome),
            Err(err) => self.on_error(err),
        }
    }

    // -----------------------------------------------------------------------
    // Internal state transitions — each returns this tick's outbound result.
    // -----------------------------------------------------------------------

    /// A completed iteration: either a final answer or one executed tool round.
    fn on_completed(&mut self, kind: CompletedKind) -> BaseAgentState {
        match kind {
            CompletedKind::PlainText(answer) => {
                let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
                match answer.raw_message_json.as_deref() {
                    Some(raw) => turn.append_assistant(raw),
                    None => turn.append_patch(&serde_json::json!([{
                        "role": "assistant",
                        "content": answer.text
                    }])),
                }
                drop(turn);
                self.lifecycle = Lifecycle::Terminal;
                BaseAgentState::Completed(answer.text)
            }
            CompletedKind::Tools(tools) => {
                let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
                turn.append_patch(&tools.appended.0);
                BaseAgentState::Working
            }
        }
    }

    /// A preempted iteration is terminal, but the *task* is not.
    ///
    /// Per the iteration semantics, the next action happens in a new iteration:
    /// merge whatever was produced, rebuild context, and continue ticking.
    fn on_preempted(&mut self, outcome: PreemptedOutcome) -> BaseAgentState {
        if let Some(produced) = outcome.produced {
            let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
            turn.append_patch(&produced.0);
        }
        // todo: rebuild context from pending input/memories before the next iteration.
        BaseAgentState::Interrupted
    }

    /// A failed iteration ends the task.
    fn on_error(&mut self, err: IterationLoopError) -> BaseAgentState {
        log::warn!("base_agent iteration failed: {err}");
        self.lifecycle = Lifecycle::Terminal;
        BaseAgentState::Failed(err)
    }
}
