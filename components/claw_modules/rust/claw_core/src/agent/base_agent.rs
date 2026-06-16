//! Layer 2 base agent: drives a multi-iteration task, one iteration per `tick`.
//!
//! Split of responsibilities:
//! - [`BaseAgent::builder`] starts construction; [`BaseAgentBuilder`] collects
//!   optional tools and skills and [`build`](BaseAgentBuilder::build)s the agent.
//!   This keeps the *build* phase separate from the *run* phase.
//! - [`BaseAgent::run`] is pure setup for one task: it injects the system prompt
//!   and appends the user's goal to memory. No iteration runs here.
//! - The driver pumps [`BaseAgent::tick`] until it returns a terminal
//!   [`BaseAgentState`]. Each `tick` performs exactly one [`IterationLoop`]
//!   round-trip, so the driver can interleave new commands and other agents
//!   between ticks (cooperative scheduling).
//!
//! The agent **owns** its LLM client, interrupt flag, conversation memory, tools,
//! skills, and the per-task system prompt set by `run`. Only the goal in
//! [`RunParams`] is borrowed, just for seeding.
//!
//! Outbound results ride on the value [`tick`](BaseAgent::tick) returns — one
//! user-facing output per tick. Internal tool-round messages are *context*, not
//! output: they are committed to memory and fed back in on the next iteration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use claw_api::ClawApi;
use claw_memory::{ConversationMemory, GroupGuard};

use crate::iteration_loop::{
    ChatMessages, CompletedKind, InterruptionControl, IterationId, IterationLoop,
    IterationLoopError, IterationOutcome, IterationStep, PreemptedOutcome, SystemPrompt,
};
use crate::skills::{SkillError, SkillGroup, SkillId, SkillSet};
use crate::tools::ToolSet;

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
    /// Terminal: the task failed; carries the cause.
    Failed(AgentRunError),
}

/// Cause of a terminal [`BaseAgentState::Failed`].
///
/// Wraps the lower-level errors a tick can hit: a failed LLM/tool iteration, or
/// a failure assembling the loaded skills' context before the iteration runs.
#[derive(Clone, Debug, thiserror::Error)]
pub enum AgentRunError {
    /// The LLM/tool iteration itself failed.
    #[error(transparent)]
    Iteration(#[from] IterationLoopError),
    /// Assembling the skill context failed (e.g. a loaded skill's document could
    /// not be read).
    #[error(transparent)]
    Skill(#[from] SkillError),
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
    /// [`run`](BaseAgent::run) was called while the previous task is still
    /// running (it has not reached a terminal [`BaseAgentState`]).
    #[error("run() called while a task is still in progress")]
    Busy,
}

/// Per-task input for [`BaseAgent::run`].
///
/// The agent's identity (its system prompt) is agent-level config, set once via
/// [`BaseAgentBuilder::with_system_prompt`]; only the goal varies per task.
pub struct RunParams<'a> {
    /// The task goal / initial user prompt.
    pub goal: &'a str,
}

/// A base agent that runs one task at a time as a sequence of iterations.
///
/// Lifecycle, in three phases:
/// 1. **Build** once via [`BaseAgent::builder`] — set the system prompt, tools,
///    and skills, then [`build`](BaseAgentBuilder::build). The built agent is
///    long-lived and reused across tasks; its conversation memory accumulates.
/// 2. **Run** a task with [`run`](BaseAgent::run) — seeds the goal, no iteration
///    happens yet.
/// 3. **Pump** [`tick`](BaseAgent::tick) until it returns a terminal
///    [`BaseAgentState`] ([`Completed`](BaseAgentState::Completed) /
///    [`Failed`](BaseAgentState::Failed)); `Working`/`Interrupted` mean keep going.
///
/// # Examples
///
/// ```ignore
/// let mut agent = BaseAgent::builder(llm, memory)
///     .with_system_prompt("You are a helpful assistant.")
///     .with_tools(tools)
///     .build();
///
/// agent.run(RunParams { goal: "summarize today's news" })?;
/// let answer = loop {
///     match agent.tick() {
///         BaseAgentState::Completed(text) => break text,
///         BaseAgentState::Failed(error) => return Err(error.into()),
///         // Working / Interrupted: an internal tool round or a preemption —
///         // keep pumping until terminal.
///         _ => continue,
///     }
/// };
/// ```
pub struct BaseAgent {
    llm: ClawApi,
    interruption: AgentInterruption,
    memory: ConversationMemory,
    tools: Option<ToolSet>,
    skills: Option<SkillSet>,
    /// Agent-level system prompt (its persona/identity), fixed across tasks.
    system_prompt: String,
    /// Cached `system_prompt` + skills context, assembled once and reused across
    /// ticks. Empty means "no skill content, borrow `system_prompt` directly".
    /// Rebuilt only when [`effective_prompt_dirty`](Self::effective_prompt_dirty)
    /// is set — at build, or on a skill load/unload.
    effective_prompt: String,
    effective_prompt_dirty: bool,
    /// Open group guard kept alive from `run()` until the first response, so
    /// the user turn and the assistant reply commit as one group — preventing
    /// compaction from orphaning a reply with no user turn.
    open_turn: Option<GroupGuard>,
    next_iteration: IterationId,
    lifecycle: Lifecycle,
}

impl BaseAgent {
    /// Start building an agent over a caller-owned [`ConversationMemory`].
    ///
    /// The agent's only construction inputs are its LLM client and its memory;
    /// how the memory is built (and keyed) is the caller's concern, via
    /// [`ConversationMemory::new`]. The caller may keep a clone of the memory to
    /// inspect the conversation without going through `BaseAgent`:
    ///
    /// ```ignore
    /// let memory = ConversationMemory::new(agent_id, config, deps);
    /// let view = memory.clone();
    /// let agent = BaseAgent::builder(llm, memory).build();
    /// // later: let messages = view.messages();
    /// ```
    pub fn builder(llm: ClawApi, memory: ConversationMemory) -> BaseAgentBuilder {
        BaseAgentBuilder {
            llm,
            memory,
            tools: None,
            skills: None,
            system_prompt: String::new(),
        }
    }

    /// Load one skill into context at runtime (no restart).
    ///
    /// Returns [`SkillError::NotFound`] if no skill set is configured or the
    /// registry has no such skill.
    pub fn load_skill(&mut self, group: &'static str, id: SkillId) -> Result<(), SkillError> {
        match self.skills.as_mut() {
            Some(skills) => {
                skills.load(group, id)?;
                self.effective_prompt_dirty = true;
                Ok(())
            }
            None => Err(SkillError::NotFound(id.to_string())),
        }
    }

    /// Load a whole [`SkillGroup`] into context at runtime. No-op if the agent
    /// has no skill set configured.
    ///
    /// Returns [`SkillError`] if a skill in the group is not in the registry.
    pub fn load_skill_group(&mut self, group: SkillGroup) -> Result<(), SkillError> {
        match self.skills.as_mut() {
            Some(skills) => {
                skills.load_group(group)?;
                self.effective_prompt_dirty = true;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Unload a skill from context at runtime. No-op if absent.
    pub fn unload_skill(&mut self, id: &SkillId) {
        if let Some(skills) = self.skills.as_mut() {
            skills.unload(id);
            self.effective_prompt_dirty = true;
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

        // Rebuild the combined prompt only when the base prompt or skills changed;
        // otherwise this is a no-op and we reuse the cached string below.
        if let Err(error) = self.refresh_effective_prompt() {
            return self.on_skill_error(error);
        }
        let system_prompt = self.effective_prompt();

        let messages = self.memory.messages();

        let outcome = {
            let iteration_loop = IterationLoop {
                llm: &self.llm,
                interruption: &self.interruption,
            };
            let step = IterationStep {
                iteration_id,
                system_prompt: SystemPrompt(system_prompt),
                messages: ChatMessages(&messages),
                tools: self.tools.as_ref(),
            };
            iteration_loop.run(step)
        };

        match outcome {
            Ok(IterationOutcome::Completed(outcome)) => self.on_completed(outcome.kind),
            Ok(IterationOutcome::Preempted(outcome)) => self.on_preempted(outcome),
            Err(err) => self.on_error(err),
        }
    }

    /// Rebuild the cached combined prompt if it is stale; otherwise a no-op.
    ///
    /// When the loaded skills produce content, the cache holds `system_prompt`
    /// concatenated with it (allocated once per change). When there is none, the
    /// cache is left empty as the signal to borrow `system_prompt` directly — see
    /// [`effective_prompt`](Self::effective_prompt).
    fn refresh_effective_prompt(&mut self) -> Result<(), SkillError> {
        if !self.effective_prompt_dirty {
            return Ok(());
        }
        let skill_context = match self.skills.as_mut() {
            Some(skills) => skills.context()?,
            None => "",
        };
        self.effective_prompt.clear();
        if !skill_context.is_empty() {
            self.effective_prompt.push_str(&self.system_prompt);
            self.effective_prompt.push_str("\n\n");
            self.effective_prompt.push_str(skill_context);
        }
        self.effective_prompt_dirty = false;
        Ok(())
    }

    /// The prompt to send this iteration — the cached combined prompt when skills
    /// contributed content, else the base prompt borrowed with no copy.
    ///
    /// Assumes [`refresh_effective_prompt`](Self::refresh_effective_prompt) ran
    /// this tick so the cache is current.
    fn effective_prompt(&self) -> &str {
        if self.effective_prompt.is_empty() {
            &self.system_prompt
        } else {
            &self.effective_prompt
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
        BaseAgentState::Failed(AgentRunError::Iteration(err))
    }

    /// A failure assembling skill context ends the task before the iteration.
    fn on_skill_error(&mut self, err: SkillError) -> BaseAgentState {
        log::warn!("base_agent skill context failed: {err}");
        self.lifecycle = Lifecycle::Terminal;
        BaseAgentState::Failed(AgentRunError::Skill(err))
    }
}

/// Builder for a [`BaseAgent`], collecting all construction-time configuration.
///
/// Separates the *build* phase from the *run* phase: tools and skills are set
/// here (optional, any order), and [`build`](Self::build) produces a finished
/// `BaseAgent` that exposes only the runtime API (`run`/`tick`/`load_skill`/…).
/// You cannot drive an agent before building it, and the consuming setters can't
/// be mixed with the agent's `&mut self` methods — the misuse that the two-phase
/// split prevents.
#[must_use = "a BaseAgentBuilder does nothing until `.build()` is called"]
pub struct BaseAgentBuilder {
    llm: ClawApi,
    memory: ConversationMemory,
    tools: Option<ToolSet>,
    skills: Option<SkillSet>,
    system_prompt: String,
}

impl BaseAgentBuilder {
    /// Set the tools available to the agent across all tasks.
    ///
    /// Takes a pre-built [`ToolSet`] so construction (which can fail on duplicate
    /// names) stays in one place; build it with
    /// [`ToolSet::from_groups`](crate::tools::ToolSet::from_groups) or
    /// [`ToolSet::new`](crate::tools::ToolSet::new).
    pub fn with_tools(mut self, tools: ToolSet) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set the agent's skills.
    ///
    /// The [`SkillSet`] is the agent's loaded-skill state; it stays mutable after
    /// build so skills can be loaded or unloaded at runtime via
    /// [`BaseAgent::load_skill`] / [`BaseAgent::unload_skill`].
    pub fn with_skills(mut self, skills: SkillSet) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Set the agent's system prompt — its instructions/persona, fixed across
    /// all of its tasks. Defaults to empty.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Finish configuration and produce a runnable [`BaseAgent`].
    pub fn build(self) -> BaseAgent {
        BaseAgent {
            llm: self.llm,
            interruption: AgentInterruption {
                flag: Arc::new(AtomicBool::new(false)),
            },
            memory: self.memory,
            tools: self.tools,
            skills: self.skills,
            system_prompt: self.system_prompt,
            effective_prompt: String::new(),
            // Build the combined prompt on the first tick.
            effective_prompt_dirty: true,
            open_turn: None,
            next_iteration: IterationId(0),
            lifecycle: Lifecycle::Idle,
        }
    }
}
