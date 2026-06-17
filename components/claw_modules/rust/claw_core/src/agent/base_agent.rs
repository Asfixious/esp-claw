//! Layer 2 base agent: a black box that takes **commands** in and reports an
//! outcome out, driven one iteration per [`tick`](BaseAgent::tick).
//!
//! # Command in, outcome out
//!
//! Everything entering the agent is an [`AgentCommand`]. Convenience methods
//! ([`run`](BaseAgent::run), [`append_message`](BaseAgent::append_message),
//! [`cancel`](BaseAgent::cancel), …) are thin wrappers that only
//! [`send_command`](BaseAgent::send_command) — they hold no state logic of their
//! own. Commands queue on an inbox and are reduced by the single funnel
//! `apply_inbound`. Each [`tick`](BaseAgent::tick) returns one [`TickOutcome`]:
//! the agent never has a side-channel of events, because everything it has to
//! report coincides with the moment a tick hands control back.
//!
//! This keeps the agent a uniform unit suitable as the core of a multi-agent
//! system: an orchestrator drives many agents through the identical
//! `send_command` / `tick` triple and never reaches into their internals.
//!
//! # Driving
//!
//! [`tick`](BaseAgent::tick) returns what happened this tick. `Working` means
//! "pump again now"; `Idle` means "nothing to do, wait for a command"; every
//! other variant is a result the driver acts on:
//!
//! ```ignore
//! loop {
//!     match agent.tick() {
//!         TickOutcome::Working => continue,
//!         TickOutcome::Idle => wait_for_command(),
//!         TickOutcome::Yielded { text } => { print(text); wait_for_command(); }
//!         TickOutcome::AwaitingApproval { id, .. } => decide(id),
//!         TickOutcome::Ended { final_message } => { print(final_message); break; }
//!         TickOutcome::Cancelled { .. } => break,
//!         TickOutcome::Failed(error) => { report(error); break; }
//!     }
//! }
//! ```
//!
//! # Termination
//!
//! A plain-text answer is [`Yielded`](TickOutcome::Yielded) — **non-terminal** —
//! and the agent goes idle awaiting the next message. A task ends only when the
//! agent decides so itself (the built-in `end_conversation` tool →
//! [`Ended`](TickOutcome::Ended)), when the orchestrator hard-stops it
//! ([`Cancel`](AgentCommand::Cancel) → [`Cancelled`](TickOutcome::Cancelled)), or
//! on [`Failed`](TickOutcome::Failed). The outside never preempts the agent's
//! reasoning; it only appends information and lets the agent re-decide. A terminal
//! outcome is reported once and leaves the agent **idle and reusable** — the next
//! [`AppendMessage`](AgentCommand::AppendMessage) starts a fresh task over the
//! same memory and identity.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use claw_api::{ClawApi, RetryPolicy};
use claw_memory::{ConversationMemory, GroupGuard};
use serde_json::{json, Value};

use crate::agent::internal_tools::{internal_tool_group, ControlSignal, ControlSink};
use crate::iteration_loop::{
    ChatMessages, CompletedKind, CompletedOutcome, InterruptionControl, IterationId, IterationLoop,
    IterationLoopError, IterationOutcome, IterationResult, IterationStep, PlainTextOutcome,
    PreemptedOutcome, SystemPrompt, ToolRun,
};
use crate::skills::{SkillError, SkillGroup, SkillId, SkillSet};
use crate::tools::{AllowedTools, ToolSet, ToolSetError};

crate::define_prefixed_id!(AgentId, "agent-", "agent");
crate::define_prefixed_id!(ApprovalId, "approval-", "approval");

/// Default for [`BaseAgentBuilder::with_tool_block_retries`]: tolerate one
/// gating-blocked tool round (one self-correction nudge) before failing.
const DEFAULT_TOOL_BLOCK_RETRIES: u32 = 1;

// ===========================================================================
// Public command / outcome vocabulary
// ===========================================================================

/// Inbound: a control input handed to the agent. This is the agent's entire
/// external surface — the outside drives the agent only through these.
///
/// Notably there is **no `Preempt`**: outside input never ends or interrupts the
/// agent's reasoning, it only adds information ([`AppendMessage`](Self::AppendMessage))
/// and lets the agent re-decide. Hard termination is [`Cancel`](Self::Cancel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentCommand {
    /// Append a user message. Starts a fresh task when the agent is idle;
    /// otherwise it joins the in-progress task.
    AppendMessage(String),
    /// Abandon the current task. (Orchestrator-initiated hard stop — distinct from
    /// the agent ending itself via `end_conversation`.) Being disruptive, it
    /// commits the abandoned turn and records an interruption marker (keyed on the
    /// [`CancelReason`]) in memory, so the next task does not inherit an
    /// unexplained, half-finished exchange.
    Cancel {
        /// Why the task is being abandoned; selects the recorded interruption marker.
        reason: CancelReason,
    },
    /// Stop scheduling iterations until [`Resume`](Self::Resume). No-op unless the
    /// agent is actively running.
    Pause,
    /// Resume a [`Pause`](Self::Pause)d agent.
    Resume,
    /// Deliver a human decision for a pending [`TickOutcome::AwaitingApproval`].
    /// Ignored unless the agent is awaiting this exact approval.
    ApprovalResult {
        /// The pending approval this decision answers.
        id: ApprovalId,
        /// The human's verdict.
        decision: ApprovalDecision,
    },
}

/// The agent's externally observable lifecycle state.
///
/// Exposed so a driver can read which state a rejected command hit off an
/// [`AgentCommandError`]. `Idle` means "no active task, awaiting input" — both
/// before the first task and after one finishes (terminal outcomes leave the
/// agent idle and reusable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// No active iteration; waiting for an [`AppendMessage`](AgentCommand::AppendMessage).
    Idle,
    /// A task is actively iterating.
    Running,
    /// A running task whose iteration scheduling is [`Pause`](AgentCommand::Pause)d.
    Paused,
    /// Stopped on a built-in `request_approval`, awaiting an
    /// [`ApprovalResult`](AgentCommand::ApprovalResult).
    AwaitingApproval,
}

/// Rejection of an [`AgentCommand`] that is invalid for the agent's current
/// [`AgentState`].
///
/// The agent is a state machine; not every command is meaningful in every
/// state (e.g. [`Resume`](AgentCommand::Resume) after a
/// [`Cancel`](AgentCommand::Cancel) left the agent idle). A rejected command is
/// **not** enqueued and the agent is left unchanged, so the caller can react
/// without racing a `tick`. Validation is against the state the agent *will* be
/// in once already-queued commands are applied, so batching commands between
/// ticks is sound.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentCommandError {
    /// [`Pause`](AgentCommand::Pause) is only valid while
    /// [`Running`](AgentState::Running).
    #[error("cannot pause: the agent is {state:?}, not running")]
    CannotPause {
        /// The state the agent was in when the pause was rejected.
        state: AgentState,
    },
    /// [`Resume`](AgentCommand::Resume) is only valid while
    /// [`Paused`](AgentState::Paused).
    #[error("cannot resume: the agent is {state:?}, not paused")]
    CannotResume {
        /// The state the agent was in when the resume was rejected.
        state: AgentState,
    },
    /// [`Cancel`](AgentCommand::Cancel) has nothing to act on while
    /// [`Idle`](AgentState::Idle).
    #[error("cannot cancel: the agent is idle with no active task")]
    NothingToCancel,
    /// [`ApprovalResult`](AgentCommand::ApprovalResult) is only valid while
    /// [`AwaitingApproval`](AgentState::AwaitingApproval).
    #[error("cannot resolve approval: the agent is {state:?}, not awaiting approval")]
    NotAwaitingApproval {
        /// The state the agent was in when the approval result was rejected.
        state: AgentState,
    },
    /// The agent is awaiting approval, but for a different request id.
    #[error("approval {got} does not match the pending approval {expected}")]
    ApprovalMismatch {
        /// The approval the agent is actually waiting on.
        expected: ApprovalId,
        /// The approval id the caller supplied.
        got: ApprovalId,
    },
}

/// Rejection of a soft-hide tool-gating change made outside
/// [`Idle`](AgentState::Idle).
///
/// Tool gating is a *pre-run* policy: a caller configures which tools the next
/// task may use, then starts it. Mutating the allow-set while a task is
/// `Running`/`Paused`/`AwaitingApproval` would let the policy desync from the
/// task in flight (and, once a semantic layer owns gating, fight that layer for
/// the same state), so it is refused. Validation is against the *projected*
/// state, so queueing a task and then trying to change gating is rejected too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot change tool gating: the agent is {state:?}, not idle")]
pub struct ToolGatingError {
    /// The state the agent was in when the gating change was rejected.
    pub state: AgentState,
}

/// Why a task was [`Cancel`](AgentCommand::Cancel)led, carried on the resulting
/// [`TickOutcome::Cancelled`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// A human asked to stop.
    UserRequested,
    /// The orchestrator replaced this task with a newer one.
    Superseded,
    /// The host is shutting the agent down.
    Shutdown,
}

/// A human's answer to an approval request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The human approved; the agent resumes and proceeds.
    Approved,
    /// The human rejected, with a reason recorded for the agent to reconsider.
    Rejected(String),
}

/// What one [`tick`](BaseAgent::tick) did — the agent's sole output channel.
///
/// `Working`/`Idle` are liveness for the driver loop; the rest are one-shot
/// results reported on the tick that produced them. A single tick yields exactly
/// one of these (tool execution is internal — it shows up only as `Working`).
#[derive(Clone, Debug)]
#[must_use]
pub enum TickOutcome {
    /// Progress was made; call `tick` again promptly.
    Working,
    /// Nothing to do right now (waiting for input, paused, or awaiting approval).
    Idle,
    /// The model returned a user-facing answer and handed control back.
    /// **Non-terminal** — the agent goes idle awaiting the next message.
    Yielded {
        /// The model's user-facing answer.
        text: String,
    },
    /// The agent called `request_approval` and is paused for a human decision.
    /// Resolve it with [`resolve_approval`](BaseAgent::resolve_approval).
    AwaitingApproval {
        /// The id to pass back via [`resolve_approval`](BaseAgent::resolve_approval).
        id: ApprovalId,
        /// A human-readable description of what needs approving.
        summary: String,
    },
    /// Terminal: the agent ended the task itself (via `end_conversation`). The
    /// agent returns to idle and may be re-tasked.
    Ended {
        /// The agent's closing message.
        final_message: String,
    },
    /// Terminal: the task was cancelled by the orchestrator.
    Cancelled {
        /// Why the task was cancelled.
        reason: CancelReason,
    },
    /// Terminal: the task failed.
    Failed(AgentRunError),
}

impl TickOutcome {
    /// True for the terminal outcomes ([`Ended`](Self::Ended) /
    /// [`Cancelled`](Self::Cancelled) / [`Failed`](Self::Failed)) — the task is
    /// over, though the agent stays reusable.
    ///
    /// # Examples
    ///
    /// ```
    /// use claw_core::agent::{CancelReason, TickOutcome};
    ///
    /// assert!(TickOutcome::Ended { final_message: "done".into() }.is_terminal());
    /// assert!(TickOutcome::Cancelled { reason: CancelReason::Shutdown }.is_terminal());
    /// assert!(!TickOutcome::Working.is_terminal());
    /// assert!(!TickOutcome::Yielded { text: "partial answer".into() }.is_terminal());
    /// ```
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TickOutcome::Ended { .. } | TickOutcome::Cancelled { .. } | TickOutcome::Failed(_)
        )
    }
}

/// Cause of a terminal [`TickOutcome::Failed`].
///
/// Wraps the lower-level errors a tick can hit: a failed LLM/tool iteration, or a
/// failure assembling the loaded skills' context before the iteration runs.
#[derive(Clone, Debug, thiserror::Error)]
pub enum AgentRunError {
    /// The LLM/tool iteration itself failed.
    #[error(transparent)]
    Iteration(#[from] IterationLoopError),
    /// Assembling the skill context failed (e.g. a loaded skill's document could
    /// not be read).
    #[error(transparent)]
    Skill(#[from] SkillError),
    /// The model kept calling a tool that soft-hide gating does not permit this
    /// phase, past the allowed retry budget (see
    /// [`BaseAgentBuilder::with_tool_block_retries`]).
    #[error("tool not permitted in the current phase: {name}")]
    ToolNotPermitted {
        /// The name of the refused tool.
        name: String,
    },
}

/// Failure assembling a [`BaseAgent`] in [`BaseAgentBuilder::build`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum BaseAgentBuildError {
    /// Merging the built-in tool group onto the caller's tools hit a name clash.
    #[error(transparent)]
    Tools(#[from] ToolSetError),
    /// Tools were provided but the configured LLM does not support tool calls, so
    /// they could never be used — surfaced rather than silently dropped.
    #[error("tools were provided but the configured LLM does not support tools")]
    ToolsUnsupported,
}

// ===========================================================================
// Internals
// ===========================================================================

/// One item on the agent's inbox: either an external [`AgentCommand`] or an
/// internal [`ControlSignal`] raised by a built-in tool. Both flow through the
/// one reducer, but only `Command` is constructible by outside callers.
enum Inbound {
    Command(AgentCommand),
    Control(ControlSignal),
}

/// A cloneable handle to abort an agent's in-flight LLM/tool round from another
/// task.
///
/// Obtain it via [`BaseAgent::abort_handle`] **before** the tick loop (you cannot
/// borrow the agent while a `tick` holds `&mut self`). It shares the same
/// `Arc<AtomicBool>` the [`IterationLoop`] polls at its checkpoints, so it can
/// stop a `tick` blocked on the LLM HTTP. It is plumbing for stopping a now-stale
/// call — the *content* of the new input still arrives as an [`AgentCommand`].
#[derive(Clone)]
pub struct AgentAbortHandle {
    flag: Arc<AtomicBool>,
}

impl AgentAbortHandle {
    /// Abort the in-flight (or next) iteration at its next checkpoint.
    pub fn abort(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// The agent's own abort flag, fed to the per-tick [`IterationLoop`].
struct AgentInterruption {
    flag: Arc<AtomicBool>,
}

impl InterruptionControl for AgentInterruption {
    fn interrupt_flag(&self) -> &Arc<AtomicBool> {
        &self.flag
    }
}

// ===========================================================================
// BaseAgent
// ===========================================================================

/// A base agent that runs one task at a time as a sequence of iterations.
///
/// Build once via [`BaseAgent::builder`]; then drive it with commands and ticks.
/// The agent is long-lived and reused across tasks — its conversation memory and
/// identity persist, so finishing a task leaves it ready for the next.
///
/// # Examples
///
/// ```ignore
/// let mut agent = BaseAgent::builder(llm, memory)
///     .with_system_prompt("You are a helpful assistant.")
///     .with_tools(tools)
///     .build()?;
///
/// agent.run("summarize today's news");
/// loop {
///     match agent.tick() {
///         TickOutcome::Working => continue,
///         TickOutcome::Yielded { text } => { println!("{text}"); break; }
///         TickOutcome::Ended { final_message } => { println!("{final_message}"); break; }
///         TickOutcome::Failed(error) => return Err(error.into()),
///         _ => break,
///     }
/// }
/// ```
pub struct BaseAgent {
    llm: ClawApi,
    /// Retry policy applied to every per-iteration LLM call.
    retry_policy: RetryPolicy,
    interruption: AgentInterruption,
    memory: ConversationMemory,
    tools: Option<ToolSet>,
    /// Tools allowed to execute this phase ("soft-hide" gating). `None` = ungated
    /// (every tool in `tools` may run). Set per semantic state by an upper layer
    /// via [`set_active_tools`](Self::set_active_tools); the full `tools` schema
    /// is always sent regardless, so the cached prompt prefix stays stable.
    allowed_tools: Option<AllowedTools>,
    /// A transient, per-tick instruction appended to the tail of the messages
    /// sent to the LLM (never persisted to memory). Carries soft-hide phase
    /// guidance (current phase + permitted tools). Set via
    /// [`set_tail_note`](Self::set_tail_note).
    tail_note: Option<String>,
    /// Count of consecutive tool rounds that had at least one gating-blocked
    /// call. Reset to 0 by any clean tool round. When it exceeds
    /// `tool_block_retries`, the task fails with [`AgentRunError::ToolNotPermitted`].
    consecutive_tool_blocks: u32,
    /// How many consecutive blocked rounds to tolerate (with a self-correction
    /// nudge) before failing the task. Default 1.
    tool_block_retries: u32,
    skills: Option<SkillSet>,
    /// Agent-level system prompt (its persona/identity), fixed across tasks.
    system_prompt: String,
    /// Cached `system_prompt` + skills context. Empty means "no skill content,
    /// borrow `system_prompt` directly". Rebuilt only when `effective_prompt_dirty`.
    effective_prompt: String,
    effective_prompt_dirty: bool,
    /// Open group guard from task start until the first response, so the user turn
    /// and the assistant reply commit as one group (compaction never orphans a
    /// reply with no user turn).
    open_turn: Option<GroupGuard>,
    next_iteration: IterationId,
    next_approval: usize,
    pending_approval: Option<ApprovalId>,
    /// The committed lifecycle state, advanced as the inbox is drained in `tick`.
    lifecycle: AgentState,
    /// The lifecycle state the agent *will* be in once every command already on
    /// the inbox is applied. Commands are validated against this (not `lifecycle`)
    /// so a batch enqueued between ticks is checked in order; it is reset to
    /// `lifecycle` at the end of each `tick`. No `tick` can run between two
    /// `send_command` calls (both need `&mut self`), so this is the only thing
    /// that moves the lifecycle between ticks and the projection stays exact.
    projected_lifecycle: AgentState,
    /// The actionable outcome produced during the current tick, if any. Reset at
    /// the start of each tick; a single tick produces at most one.
    outcome: Option<TickOutcome>,
    /// Sink the built-in tools push [`ControlSignal`]s onto; drained each tick.
    control: ControlSink,
    inbox: VecDeque<Inbound>,
}

impl BaseAgent {
    /// Start building an agent over a caller-owned [`ConversationMemory`].
    ///
    /// The caller decides how the memory is built and keyed (via
    /// [`ConversationMemory::new`]) and may keep a clone to inspect the
    /// conversation without going through `BaseAgent`:
    ///
    /// ```ignore
    /// let memory = ConversationMemory::new(agent_id, config, deps);
    /// let view = memory.clone();
    /// let agent = BaseAgent::builder(llm, memory).build()?;
    /// // later: let messages = view.messages();
    /// ```
    pub fn builder(llm: ClawApi, memory: ConversationMemory) -> BaseAgentBuilder {
        BaseAgentBuilder {
            llm,
            memory,
            tools: None,
            skills: None,
            system_prompt: String::new(),
            retry_policy: RetryPolicy::default(),
            tool_block_retries: DEFAULT_TOOL_BLOCK_RETRIES,
        }
    }

    // -- Inbound: the kernel + ergonomic wrappers ---------------------------

    /// Queue a command. The single inbound entry point; everything else wraps it.
    ///
    /// The command is validated against the agent's *projected* state (the state
    /// it will reach once already-queued commands are applied). A valid command
    /// is enqueued for the next [`tick`](Self::tick); an invalid one is rejected
    /// and the agent is left unchanged.
    ///
    /// # Errors
    ///
    /// [`AgentCommandError`] when the command is not legal for the projected
    /// state — e.g. [`Resume`](AgentCommand::Resume) when not paused, or
    /// [`Cancel`](AgentCommand::Cancel) when already idle.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::agent::{AgentCommandError, CancelReason};
    ///
    /// agent.run("summarize the news");            // projected state -> Running
    /// agent.cancel(CancelReason::UserRequested)?; // projected state -> Idle
    ///
    /// // Validated against the *projected* state (the batch so far), before any
    /// // tick runs: resuming the now-idle agent is rejected and the agent is
    /// // left unchanged.
    /// assert!(matches!(agent.resume(), Err(AgentCommandError::CannotResume { .. })));
    /// # Ok::<(), AgentCommandError>(())
    /// ```
    pub fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError> {
        let next = Self::classify(self.projected_lifecycle, &command, self.pending_approval)?;
        self.projected_lifecycle = next;
        self.inbox.push_back(Inbound::Command(command));
        Ok(())
    }

    /// Start (or continue) a task with `goal`. Convenience for
    /// [`AppendMessage`](AgentCommand::AppendMessage).
    ///
    /// Infallible: an append is valid in every state (it starts a fresh task when
    /// idle and joins the current one otherwise), so it can never be rejected.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::agent::TickOutcome;
    ///
    /// agent.run("summarize today's news"); // queue the task, then drive with `tick`
    /// assert!(matches!(agent.tick(), TickOutcome::Working | TickOutcome::Yielded { .. }));
    /// ```
    pub fn run(&mut self, goal: impl Into<String>) {
        // `AppendMessage` is accepted in every state; `send_command` cannot reject it.
        let _ = self.send_command(AgentCommand::AppendMessage(goal.into()));
    }

    /// Append a user message. Convenience for
    /// [`AppendMessage`](AgentCommand::AppendMessage). Infallible — see [`run`](Self::run).
    pub fn append_message(&mut self, message: impl Into<String>) {
        // `AppendMessage` is accepted in every state; `send_command` cannot reject it.
        let _ = self.send_command(AgentCommand::AppendMessage(message.into()));
    }

    /// Abandon the current task. Convenience for [`Cancel`](AgentCommand::Cancel).
    ///
    /// # Errors
    ///
    /// [`AgentCommandError::NothingToCancel`] when the agent is idle.
    pub fn cancel(&mut self, reason: CancelReason) -> Result<(), AgentCommandError> {
        self.send_command(AgentCommand::Cancel { reason })
    }

    /// Pause iteration scheduling. Convenience for [`Pause`](AgentCommand::Pause).
    ///
    /// # Errors
    ///
    /// [`AgentCommandError::CannotPause`] unless the agent is running.
    pub fn pause(&mut self) -> Result<(), AgentCommandError> {
        self.send_command(AgentCommand::Pause)
    }

    /// Resume after a pause. Convenience for [`Resume`](AgentCommand::Resume).
    ///
    /// # Errors
    ///
    /// [`AgentCommandError::CannotResume`] unless the agent is paused.
    pub fn resume(&mut self) -> Result<(), AgentCommandError> {
        self.send_command(AgentCommand::Resume)
    }

    /// Resolve a pending approval request. Convenience for
    /// [`ApprovalResult`](AgentCommand::ApprovalResult); pass
    /// [`ApprovalDecision::Approved`] or [`ApprovalDecision::Rejected`].
    ///
    /// # Errors
    ///
    /// [`AgentCommandError::NotAwaitingApproval`] when no approval is pending, or
    /// [`AgentCommandError::ApprovalMismatch`] when `id` is not the pending one.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::agent::{ApprovalDecision, AgentCommandError, TickOutcome};
    ///
    /// // A tick that stopped on a built-in `request_approval` hands back the id.
    /// if let TickOutcome::AwaitingApproval { id, .. } = agent.tick() {
    ///     agent.resolve_approval(id, ApprovalDecision::Approved)?;
    /// }
    /// # Ok::<(), AgentCommandError>(())
    /// ```
    pub fn resolve_approval(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), AgentCommandError> {
        self.send_command(AgentCommand::ApprovalResult { id, decision })
    }

    // -- Skills (agent config, not conversation drive) ----------------------

    /// Load one skill into context at runtime (no restart).
    ///
    /// # Errors
    ///
    /// [`SkillError::NotFound`] if no skill set is configured or the registry has
    /// no such skill.
    pub fn load_skill(&mut self, group: &'static str, id: SkillId) -> Result<(), SkillError> {
        match self.skills.as_mut() {
            Some(skills) => {
                skills.load(group, id)?;
                self.effective_prompt_dirty = true;
                Ok(())
            }
            None => Err(SkillError::NotFound(id)),
        }
    }

    /// Load a whole [`SkillGroup`] into context at runtime. No-op if the agent has
    /// no skill set configured.
    ///
    /// # Errors
    ///
    /// [`SkillError`] if a skill in the group is not in the registry.
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

    // -- Status -------------------------------------------------------------

    /// True while a task is actively iterating (not idle, paused, or awaiting
    /// approval).
    pub fn is_running(&self) -> bool {
        self.lifecycle == AgentState::Running
    }

    /// A handle to abort this agent's in-flight iteration from another task. Grab
    /// it before the tick loop starts (see [`AgentAbortHandle`]).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let handle = agent.abort_handle();      // clone-and-move to another thread
    /// std::thread::spawn(move || handle.abort());
    /// // The next `tick` is preempted at its first checkpoint and returns Working.
    /// ```
    pub fn abort_handle(&self) -> AgentAbortHandle {
        AgentAbortHandle {
            flag: Arc::clone(&self.interruption.flag),
        }
    }

    // -- Soft-hide gating (set by an upper / semantic layer) ----------------

    /// Restrict which tools may *execute* for the next task ("soft-hide" gating).
    ///
    /// The full tool schema is still sent to the model every iteration (so the
    /// cached prompt prefix never moves), but any tool call whose name is not in
    /// `allowed` is refused at execution time and the model is handed a tool
    /// error instead. Pair the budget with
    /// [`BaseAgentBuilder::with_tool_block_retries`].
    ///
    /// Only valid while [`Idle`](AgentState::Idle): gating is a pre-run policy,
    /// configured before a task starts and held for that task's lifetime, not
    /// mutated mid-flight. Calling it once a task is queued or running returns
    /// [`ToolGatingError`] and changes nothing.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::AllowedTools;
    ///
    /// // Configure gating, then start the task.
    /// agent.set_active_tools(AllowedTools::new(["read_file", "end_conversation"]))?;
    /// agent.run("look something up");
    /// ```
    pub fn set_active_tools(&mut self, allowed: AllowedTools) -> Result<(), ToolGatingError> {
        self.ensure_idle_for_gating()?;
        self.allowed_tools = Some(allowed);
        Ok(())
    }

    /// Remove tool gating so every tool in the set may run again (the default).
    ///
    /// Only valid while [`Idle`](AgentState::Idle); see
    /// [`set_active_tools`](Self::set_active_tools).
    pub fn clear_active_tools(&mut self) -> Result<(), ToolGatingError> {
        self.ensure_idle_for_gating()?;
        self.allowed_tools = None;
        Ok(())
    }

    /// Reject a gating change unless the agent is idle (no task queued or
    /// running). Checks the *projected* state so a queued-but-not-yet-ticked
    /// task also blocks the change.
    fn ensure_idle_for_gating(&self) -> Result<(), ToolGatingError> {
        if self.projected_lifecycle == AgentState::Idle {
            Ok(())
        } else {
            Err(ToolGatingError {
                state: self.projected_lifecycle,
            })
        }
    }

    /// Set the transient phase note appended to the tail of the next LLM
    /// request's messages (never written to memory).
    ///
    /// Mirrors the production "system-reminder" pattern: stable `system`/`tools`
    /// stay frozen for cache reuse while changing, per-phase guidance rides at
    /// the end of the conversation. The note persists across ticks until changed
    /// or cleared. Crate-internal: consumed by the in-crate semantic agents.
    pub(crate) fn set_tail_note(&mut self, note: impl Into<String>) {
        self.tail_note = Some(note.into());
    }

    /// Clear the transient phase note.
    pub(crate) fn clear_tail_note(&mut self) {
        self.tail_note = None;
    }

    // -- The tick -----------------------------------------------------------

    /// Process queued commands, advance at most one iteration, and report what
    /// happened as a [`TickOutcome`].
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::agent::TickOutcome;
    ///
    /// agent.run("summarize today's news");
    /// loop {
    ///     match agent.tick() {
    ///         TickOutcome::Working => continue,            // pump again now
    ///         TickOutcome::Idle => break,                  // nothing to do; await input
    ///         TickOutcome::Yielded { text } => { println!("{text}"); break; }
    ///         TickOutcome::Ended { final_message } => { println!("{final_message}"); break; }
    ///         TickOutcome::Failed(error) => { eprintln!("{error}"); break; }
    ///         _ => break,
    ///     }
    /// }
    /// ```
    pub fn tick(&mut self) -> TickOutcome {
        self.outcome = None;

        // 1. External commands.
        self.drain_inbox();

        // 2. One iteration, if running.
        if self.lifecycle == AgentState::Running {
            let iteration_id = self.next_iteration;
            self.next_iteration = IterationId(iteration_id.0.saturating_add(1));

            if let Err(error) = self.refresh_effective_prompt() {
                self.fail_with(AgentRunError::Skill(error));
            } else {
                let outcome = self.run_iteration(iteration_id);
                self.reduce_outcome(outcome);
                // 3. Internal-tool signals raised during the iteration, folded
                //    back through the same reducer.
                self.drain_control_signals();
                self.drain_inbox();
            }
        }

        // The inbox is now drained; realign the projection with the committed
        // state so the next batch of commands is validated against the truth.
        self.projected_lifecycle = self.lifecycle;

        self.outcome.take().unwrap_or_else(|| match self.lifecycle {
            AgentState::Running => TickOutcome::Working,
            _ => TickOutcome::Idle,
        })
    }

    /// Run exactly one [`IterationLoop`] round over current context.
    fn run_iteration(&self, iteration_id: IterationId) -> IterationResult {
        let system_prompt = self.effective_prompt();
        let mut messages = self.memory.messages();
        // Append the transient phase note at the tail of this request only; it is
        // never committed to memory, so the cached system/tools prefix is untouched.
        // TODO: provisional soft-hide injection — a single user message at the
        // messages tail. Revisit placement/format once the semantic layer lands
        // (e.g. richer system-reminder-style notes, or attaching guidance next to
        // the latest tool result for stronger recency).
        if let Some(note) = &self.tail_note {
            if let Some(items) = messages.as_array_mut() {
                items.push(json!({ "role": "user", "content": note }));
            }
        }
        let iteration_loop = IterationLoop {
            llm: &self.llm,
            interruption: &self.interruption,
            retry: self.retry_policy,
        };
        let step = IterationStep {
            iteration_id,
            system_prompt: SystemPrompt(system_prompt),
            messages: ChatMessages(&messages),
            tools: self.tools.as_ref(),
            allowed_tools: self.allowed_tools.as_ref(),
        };
        iteration_loop.run(step)
    }

    // -- Reducer: the single state-mutation funnel for inbound --------------

    fn drain_inbox(&mut self) {
        while let Some(inbound) = self.inbox.pop_front() {
            self.apply_inbound(inbound);
        }
    }

    /// Move internal-tool signals onto the inbox so they reduce like commands.
    fn drain_control_signals(&mut self) {
        let signals: Vec<ControlSignal> = {
            let mut sink = self.control.lock().unwrap_or_else(|poison| poison.into_inner());
            sink.drain(..).collect()
        };
        for signal in signals {
            self.inbox.push_back(Inbound::Control(signal));
        }
    }

    /// THE reducer: the only place inbound input mutates agent state.
    ///
    /// External [`AgentCommand`]s arrive here already validated by
    /// [`classify`](Self::classify) (at [`send_command`](Self::send_command) time),
    /// so the state transitions below are unconditional — an illegal command never
    /// reaches the inbox. Internal [`ControlSignal`]s are agent-generated and
    /// always legal in the state that raised them.
    fn apply_inbound(&mut self, inbound: Inbound) {
        match inbound {
            Inbound::Command(AgentCommand::AppendMessage(text)) => {
                if self.lifecycle == AgentState::Idle {
                    // A fresh task: reset the iteration counter and open a new turn.
                    self.next_iteration = IterationId(0);
                    self.open_turn = None;
                    self.lifecycle = AgentState::Running;
                }
                self.append_user_message(text);
            }
            Inbound::Command(AgentCommand::Cancel { reason }) => {
                // Cancel is *disruptive* (unlike pause/resume/append/approve, which
                // are normal flow): record why the task was abandoned so the next
                // task does not inherit an unexplained, half-finished exchange.
                self.commit_cancellation(&reason);
                self.pending_approval = None;
                self.lifecycle = AgentState::Idle;
                self.outcome = Some(TickOutcome::Cancelled { reason });
            }
            Inbound::Command(AgentCommand::Pause) => {
                self.lifecycle = AgentState::Paused;
            }
            Inbound::Command(AgentCommand::Resume) => {
                self.lifecycle = AgentState::Running;
            }
            Inbound::Command(AgentCommand::ApprovalResult { id, decision }) => {
                self.commit_approval_decision(id, &decision);
                self.pending_approval = None;
                self.lifecycle = AgentState::Running;
            }
            Inbound::Control(ControlSignal::EndConversation { final_message }) => {
                let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
                turn.append_patch(&json!([{ "role": "assistant", "content": final_message.clone() }]));
                drop(turn);
                self.lifecycle = AgentState::Idle;
                self.outcome = Some(TickOutcome::Ended { final_message });
            }
            Inbound::Control(ControlSignal::ApprovalRequested { summary }) => {
                let id = self.allocate_approval_id();
                self.pending_approval = Some(id);
                self.lifecycle = AgentState::AwaitingApproval;
                self.outcome = Some(TickOutcome::AwaitingApproval { id, summary });
            }
        }
    }

    /// The FSM transition table: the single authority on whether `command` is
    /// legal in `state`, and what state it leads to. Pure (no `&self`) so it is
    /// trivially testable and is the one place command validity is decided —
    /// [`apply_inbound`](Self::apply_inbound) trusts its verdict.
    ///
    /// `pending_approval` is the id the agent is currently waiting on; it is only
    /// consulted in the [`AwaitingApproval`](AgentState::AwaitingApproval) state.
    /// The match is exhaustive over every `(state, command)` pair so a new state
    /// or command cannot be silently mishandled.
    fn classify(
        state: AgentState,
        command: &AgentCommand,
        pending_approval: Option<ApprovalId>,
    ) -> Result<AgentState, AgentCommandError> {
        use AgentCommand as Command;
        use AgentState as State;
        match (state, command) {
            // AppendMessage is accepted in every state: from idle it starts a
            // fresh task (-> Running); otherwise it joins without changing state.
            (State::Idle, Command::AppendMessage(_)) => Ok(State::Running),
            (State::Running, Command::AppendMessage(_)) => Ok(State::Running),
            (State::Paused, Command::AppendMessage(_)) => Ok(State::Paused),
            (State::AwaitingApproval, Command::AppendMessage(_)) => Ok(State::AwaitingApproval),

            // Cancel ends an active task; there is nothing to cancel when idle.
            (State::Idle, Command::Cancel { .. }) => Err(AgentCommandError::NothingToCancel),
            (State::Running | State::Paused | State::AwaitingApproval, Command::Cancel { .. }) => {
                Ok(State::Idle)
            }

            // Pause only makes sense while actively running.
            (State::Running, Command::Pause) => Ok(State::Paused),
            (state @ (State::Idle | State::Paused | State::AwaitingApproval), Command::Pause) => {
                Err(AgentCommandError::CannotPause { state })
            }

            // Resume only from a paused task.
            (State::Paused, Command::Resume) => Ok(State::Running),
            (state @ (State::Idle | State::Running | State::AwaitingApproval), Command::Resume) => {
                Err(AgentCommandError::CannotResume { state })
            }

            // An approval result needs a matching pending request.
            (State::AwaitingApproval, Command::ApprovalResult { id, .. }) => match pending_approval {
                Some(pending) if pending == *id => Ok(State::Running),
                Some(pending) => Err(AgentCommandError::ApprovalMismatch {
                    expected: pending,
                    got: *id,
                }),
                // AwaitingApproval always carries a pending id; a missing one is
                // an impossible invariant, surfaced rather than silently accepted.
                None => Err(AgentCommandError::NotAwaitingApproval { state }),
            },
            (
                state @ (State::Idle | State::Running | State::Paused),
                Command::ApprovalResult { .. },
            ) => Err(AgentCommandError::NotAwaitingApproval { state }),
        }
    }

    /// Reduce one iteration outcome into the tick's outcome and lifecycle. The
    /// second funnel (the first being [`apply_inbound`](Self::apply_inbound)) — its
    /// input is the LLM/tool round result, not a command.
    fn reduce_outcome(&mut self, outcome: IterationResult) {
        match outcome {
            Ok(IterationOutcome::Completed(CompletedOutcome { kind, .. })) => match kind {
                CompletedKind::PlainText(answer) => {
                    self.commit_assistant(&answer);
                    // Non-terminal: hand back to the caller, go idle for next input.
                    self.lifecycle = AgentState::Idle;
                    self.outcome = Some(TickOutcome::Yielded { text: answer.text });
                }
                CompletedKind::Tools(tools) => {
                    // A tool round: merge the messages and keep working. The
                    // per-tool summary (`tools.runs`) stays internal — base_agent
                    // does not surface it as an outcome. The patch is well-formed
                    // even for gating-blocked calls (each got a matched tool
                    // error), so committing here never leaves a dangling call.
                    self.commit_patch(&tools.appended.0);
                    self.apply_tool_block_policy(&tools.runs);
                }
            },
            Ok(IterationOutcome::Preempted(outcome)) => {
                // A preempted iteration is terminal; the task is not. Merge only a
                // well-formed partial patch, then re-iterate next tick.
                self.merge_preempt_patch(outcome);
            }
            Err(error) => self.fail_with(AgentRunError::Iteration(error)),
        }
    }

    /// Apply the soft-hide "retry then fail" policy after a tool round.
    ///
    /// A round with any gating-blocked call bumps the consecutive counter (the
    /// model already received a tool error to self-correct from); once it exceeds
    /// `tool_block_retries` the task fails. A clean round resets the counter.
    fn apply_tool_block_policy(&mut self, runs: &[ToolRun]) {
        let blocked: Vec<&str> = runs
            .iter()
            .filter(|run| run.blocked)
            .map(|run| run.name.as_str())
            .collect();

        if blocked.is_empty() {
            self.consecutive_tool_blocks = 0;
            return;
        }

        self.consecutive_tool_blocks = self.consecutive_tool_blocks.saturating_add(1);
        log::warn!(
            "tool_gate_blocked consecutive={} budget={} tools={:?}",
            self.consecutive_tool_blocks,
            self.tool_block_retries,
            blocked
        );

        if self.consecutive_tool_blocks > self.tool_block_retries {
            let name = blocked.first().map(|name| (*name).to_string()).unwrap_or_default();
            self.fail_with(AgentRunError::ToolNotPermitted { name });
        }
    }

    /// End the task with a failure outcome, leaving the agent idle and reusable.
    fn fail_with(&mut self, error: AgentRunError) {
        log::warn!("base_agent task failed: {error}");
        self.lifecycle = AgentState::Idle;
        self.outcome = Some(TickOutcome::Failed(error));
    }

    // -- Memory helpers -----------------------------------------------------

    /// Append a user message, reusing the open turn group or opening a new one.
    fn append_user_message(&mut self, text: impl Into<String>) {
        let text = text.into();
        match &self.open_turn {
            Some(turn) => turn.append_user(text),
            None => {
                let turn = self.memory.group();
                turn.append_user(text);
                self.open_turn = Some(turn);
            }
        }
    }

    /// Commit the model's plain-text answer, closing the open turn group.
    fn commit_assistant(&mut self, answer: &PlainTextOutcome) {
        let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
        match answer.raw_message_json.as_deref() {
            Some(raw) => turn.append_assistant(raw),
            None => turn.append_patch(&json!([{ "role": "assistant", "content": answer.text }])),
        }
    }

    /// Commit a materialized assistant+tool patch, closing the open turn group.
    fn commit_patch(&mut self, patch: &Value) {
        let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
        turn.append_patch(patch);
    }

    /// Merge a preemption's partial patch only when it is well-formed.
    ///
    /// A mid-tool-round preempt can leave an assistant message whose `tool_calls`
    /// have no matching tool results — committing that would make the next LLM
    /// call ill-formed. So such a patch is dropped (the half-done work simply did
    /// not happen); a clean patch is merged.
    fn merge_preempt_patch(&mut self, outcome: PreemptedOutcome) {
        let Some(produced) = outcome.produced else {
            return;
        };
        if has_dangling_tool_calls(&produced.0) {
            log::info!("base_agent dropping preempted partial patch: unmatched tool_calls");
            return;
        }
        let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
        turn.append_patch(&produced.0);
    }

    /// Record a disruption marker for a cancelled task and commit it — together
    /// with any abandoned (still-open) user turn — as one group.
    ///
    /// Cancel is the one command that ends a task abruptly without the agent
    /// producing a closing message, so it leaves an explicit trace keyed on the
    /// [`CancelReason`]. The buffered turn is *not* lost: dropping the open guard
    /// commits it alongside the marker.
    fn commit_cancellation(&mut self, reason: &CancelReason) {
        let note = match reason {
            CancelReason::UserRequested => "[conversation interrupted: cancelled by the user]",
            CancelReason::Superseded => "[conversation interrupted: superseded by a new task]",
            CancelReason::Shutdown => "[conversation interrupted: the agent is shutting down]",
        };
        self.append_user_message(note);
        // Drop the guard to commit the abandoned turn plus the marker as one group.
        self.open_turn = None;
    }

    /// Record a human approval decision as a message for the next iteration.
    fn commit_approval_decision(&mut self, id: ApprovalId, decision: &ApprovalDecision) {
        let text = match decision {
            ApprovalDecision::Approved => format!("[approval {id}] approved by the human."),
            ApprovalDecision::Rejected(reason) => {
                format!("[approval {id}] rejected by the human: {reason}")
            }
        };
        self.append_user_message(text);
    }

    fn allocate_approval_id(&mut self) -> ApprovalId {
        let id = ApprovalId(self.next_approval);
        self.next_approval = self.next_approval.saturating_add(1);
        id
    }

    // -- Effective prompt cache ---------------------------------------------

    /// Rebuild the cached combined prompt if stale; otherwise a no-op.
    ///
    /// When the loaded skills produce content, the cache holds `system_prompt`
    /// concatenated with it (allocated once per change). When there is none, the
    /// cache is left empty as the signal to borrow `system_prompt` directly.
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
    /// contributed content, else the base prompt borrowed with no copy. Assumes
    /// [`refresh_effective_prompt`](Self::refresh_effective_prompt) ran this tick.
    fn effective_prompt(&self) -> &str {
        if self.effective_prompt.is_empty() {
            &self.system_prompt
        } else {
            &self.effective_prompt
        }
    }
}

/// True when `patch` contains an assistant `tool_calls` id with no matching
/// `tool` message (`tool_call_id`).
fn has_dangling_tool_calls(patch: &Value) -> bool {
    let Some(items) = patch.as_array() else {
        return false;
    };
    let mut expected: Vec<&str> = Vec::new();
    let mut satisfied: HashSet<&str> = HashSet::new();
    for message in items {
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    expected.push(id);
                }
            }
        }
        if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
            satisfied.insert(id);
        }
    }
    expected.iter().any(|id| !satisfied.contains(id))
}

// ===========================================================================
// Builder
// ===========================================================================

/// Builder for a [`BaseAgent`], collecting all construction-time configuration.
///
/// Separates the *build* phase from the *run* phase: system prompt, tools, and
/// skills are set here (optional, any order), and [`build`](Self::build) produces
/// a finished agent that exposes only the runtime command/tick API.
#[must_use = "a BaseAgentBuilder does nothing until `.build()` is called"]
pub struct BaseAgentBuilder {
    llm: ClawApi,
    memory: ConversationMemory,
    tools: Option<ToolSet>,
    skills: Option<SkillSet>,
    system_prompt: String,
    retry_policy: RetryPolicy,
    tool_block_retries: u32,
}

impl BaseAgentBuilder {
    /// Set the tools available to the agent across all tasks.
    ///
    /// Takes a pre-built [`ToolSet`]; the agent's built-in tools
    /// (`end_conversation`, `request_approval`) are merged on at
    /// [`build`](Self::build).
    pub fn with_tools(mut self, tools: ToolSet) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set the agent's skills. The [`SkillSet`] stays mutable after build so
    /// skills can be loaded/unloaded at runtime via [`BaseAgent::load_skill`] /
    /// [`BaseAgent::unload_skill`].
    pub fn with_skills(mut self, skills: SkillSet) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Set the agent's system prompt — its instructions/persona, fixed across all
    /// of its tasks. Defaults to empty.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Override the [`RetryPolicy`] applied to every per-iteration LLM call.
    ///
    /// Defaults to [`RetryPolicy::default`] (2 retries on transient transport
    /// failures). Pass [`RetryPolicy::none`] to fail fast on the first error
    /// (e.g. to make a single transport error surface as
    /// [`TickOutcome::Failed`] without burning the retry budget).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::agent::{BaseAgent, RetryPolicy};
    ///
    /// let agent = BaseAgent::builder(llm, memory)
    ///     .with_retry_policy(RetryPolicy::none()) // no retry: first transport error -> Failed
    ///     .build()?;
    /// # Ok::<(), claw_core::agent::BaseAgentBuildError>(())
    /// ```
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// How many consecutive soft-hide-blocked tool rounds to tolerate before the
    /// task fails with [`AgentRunError::ToolNotPermitted`].
    ///
    /// Only relevant once an upper layer gates tools via
    /// [`BaseAgent::set_active_tools`]. Each blocked round hands the model a tool
    /// error so it can self-correct; this bounds how many such nudges are given.
    /// `0` fails on the first blocked call; the default is `1` (one nudge, then
    /// fail on a second consecutive blocked round). A clean tool round resets the
    /// counter.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use claw_core::agent::BaseAgent;
    ///
    /// let agent = BaseAgent::builder(llm, memory)
    ///     .with_tool_block_retries(0) // fail immediately on a disallowed tool call
    ///     .build()?;
    /// # Ok::<(), claw_core::agent::BaseAgentBuildError>(())
    /// ```
    pub fn with_tool_block_retries(mut self, retries: u32) -> Self {
        self.tool_block_retries = retries;
        self
    }

    /// Finish configuration and produce a runnable [`BaseAgent`].
    ///
    /// The built-in tool group is merged onto the caller's tools when the LLM
    /// supports tool calls.
    ///
    /// # Errors
    ///
    /// - [`BaseAgentBuildError::Tools`] if a built-in tool name clashes with a
    ///   caller tool.
    /// - [`BaseAgentBuildError::ToolsUnsupported`] if tools were provided but the
    ///   configured LLM cannot call tools.
    pub fn build(self) -> Result<BaseAgent, BaseAgentBuildError> {
        let control: ControlSink = Arc::new(Mutex::new(VecDeque::new()));
        let supports_tools = self.llm.profile().supports_tools;

        let tools = if supports_tools {
            let mut tools = self.tools.unwrap_or_else(ToolSet::empty);
            tools.extend_with_group(internal_tool_group(Arc::clone(&control)))?;
            Some(tools)
        } else {
            if self.tools.is_some() {
                return Err(BaseAgentBuildError::ToolsUnsupported);
            }
            None
        };

        Ok(BaseAgent {
            llm: self.llm,
            retry_policy: self.retry_policy,
            interruption: AgentInterruption {
                flag: Arc::new(AtomicBool::new(false)),
            },
            memory: self.memory,
            tools,
            allowed_tools: None,
            tail_note: None,
            consecutive_tool_blocks: 0,
            tool_block_retries: self.tool_block_retries,
            skills: self.skills,
            system_prompt: self.system_prompt,
            effective_prompt: String::new(),
            // Build the combined prompt on the first tick.
            effective_prompt_dirty: true,
            open_turn: None,
            next_iteration: IterationId(0),
            next_approval: 0,
            pending_approval: None,
            lifecycle: AgentState::Idle,
            projected_lifecycle: AgentState::Idle,
            outcome: None,
            control,
            inbox: VecDeque::new(),
        })
    }
}

// ===========================================================================
// Tests: the FSM transition table
// ===========================================================================

#[cfg(test)]
mod transition_tests {
    use super::*;

    fn append() -> AgentCommand {
        AgentCommand::AppendMessage("hi".into())
    }

    fn cancel() -> AgentCommand {
        AgentCommand::Cancel {
            reason: CancelReason::UserRequested,
        }
    }

    fn approval(id: usize) -> AgentCommand {
        AgentCommand::ApprovalResult {
            id: ApprovalId(id),
            decision: ApprovalDecision::Approved,
        }
    }

    fn classify(state: AgentState, command: &AgentCommand) -> Result<AgentState, AgentCommandError> {
        BaseAgent::classify(state, command, None)
    }

    #[test]
    fn append_is_accepted_in_every_state() {
        assert_eq!(classify(AgentState::Idle, &append()), Ok(AgentState::Running));
        assert_eq!(
            classify(AgentState::Running, &append()),
            Ok(AgentState::Running)
        );
        assert_eq!(
            classify(AgentState::Paused, &append()),
            Ok(AgentState::Paused)
        );
        assert_eq!(
            classify(AgentState::AwaitingApproval, &append()),
            Ok(AgentState::AwaitingApproval)
        );
    }

    #[test]
    fn cancel_ends_a_task_but_not_when_idle() {
        assert_eq!(
            classify(AgentState::Running, &cancel()),
            Ok(AgentState::Idle)
        );
        assert_eq!(classify(AgentState::Paused, &cancel()), Ok(AgentState::Idle));
        assert_eq!(
            classify(AgentState::AwaitingApproval, &cancel()),
            Ok(AgentState::Idle)
        );
        assert_eq!(
            classify(AgentState::Idle, &cancel()),
            Err(AgentCommandError::NothingToCancel)
        );
    }

    #[test]
    fn pause_only_from_running() {
        assert_eq!(
            classify(AgentState::Running, &AgentCommand::Pause),
            Ok(AgentState::Paused)
        );
        for state in [
            AgentState::Idle,
            AgentState::Paused,
            AgentState::AwaitingApproval,
        ] {
            assert_eq!(
                classify(state, &AgentCommand::Pause),
                Err(AgentCommandError::CannotPause { state })
            );
        }
    }

    #[test]
    fn resume_only_from_paused() {
        assert_eq!(
            classify(AgentState::Paused, &AgentCommand::Resume),
            Ok(AgentState::Running)
        );
        for state in [
            AgentState::Idle,
            AgentState::Running,
            AgentState::AwaitingApproval,
        ] {
            assert_eq!(
                classify(state, &AgentCommand::Resume),
                Err(AgentCommandError::CannotResume { state })
            );
        }
    }

    /// The motivating case: cancel leaves the agent idle, so a following resume is
    /// rejected instead of being silently dropped.
    #[test]
    fn cancel_then_resume_is_rejected() {
        let after_cancel = classify(AgentState::Running, &cancel()).expect("cancel from running");
        assert_eq!(after_cancel, AgentState::Idle);
        assert_eq!(
            classify(after_cancel, &AgentCommand::Resume),
            Err(AgentCommandError::CannotResume {
                state: AgentState::Idle
            })
        );
    }

    #[test]
    fn approval_requires_awaiting_and_matching_id() {
        // Not awaiting in any other state.
        for state in [AgentState::Idle, AgentState::Running, AgentState::Paused] {
            assert_eq!(
                BaseAgent::classify(state, &approval(1), None),
                Err(AgentCommandError::NotAwaitingApproval { state })
            );
        }
        // Matching id resumes; a mismatch is reported with both ids.
        assert_eq!(
            BaseAgent::classify(AgentState::AwaitingApproval, &approval(7), Some(ApprovalId(7))),
            Ok(AgentState::Running)
        );
        assert_eq!(
            BaseAgent::classify(AgentState::AwaitingApproval, &approval(7), Some(ApprovalId(9))),
            Err(AgentCommandError::ApprovalMismatch {
                expected: ApprovalId(9),
                got: ApprovalId(7),
            })
        );
    }
}
