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
use claw_interface::ClawFs;
use claw_memory::{ConversationMemory, GroupGuard};
use serde_json::{json, Value};

use crate::agent::internal_tools::{internal_tool_group, ControlSignal, ControlSink};
use crate::iteration_loop::{
    ChatMessages, CompletedKind, CompletedOutcome, InterruptionControl, IterationId, IterationLoop,
    IterationLoopError, IterationOutcome, IterationResult, IterationStep, PlainTextOutcome,
    PreemptedOutcome, SystemPrompt, ToolRun,
};
use crate::tool_runner::ToolGate;
use crate::tools::{AllowedTools, ToolSet, ToolSetError};
use claw_context::{Block, BlockKind, ContextBuilder};
use claw_permission::{Action, Grant, GrantStore, PermissionDecision, PermissionPolicy, PermissionRequest};
use claw_skill::{SkillError, SkillGroup, SkillId, SkillSet};

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
    /// Paused on a permission-policy `Ask`, awaiting an
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
    /// A tool call's permission policy returned `Ask`; the agent is paused for a
    /// human decision. Resolve it with [`resolve_approval`](BaseAgent::resolve_approval).
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

/// The agent's permission gate: a policy, the acting agent's identity, and the
/// grant store of human decisions, implementing [`ToolGate`] for the tool runner.
///
/// [`decide`](ToolGate::decide) is read-only — it answers from a recorded
/// [`Grant`] first (so a previously approved/denied action resolves without
/// asking again, which also prevents an ask/retry loop), then falls back to the
/// policy. Recording a decision happens separately, on an
/// [`ApprovalResult`](AgentCommand::ApprovalResult), via
/// [`record_decision`](Self::record_decision).
struct PermissionGate {
    policy: Arc<dyn PermissionPolicy>,
    agent_id: u64,
    agent_kind: String,
    grants: GrantStore,
}

impl PermissionGate {
    /// Record a human decision against `signatures` (the actions that were asked
    /// about), so the matching retried calls resolve directly.
    fn record_decision(&mut self, signatures: &[String], decision: &ApprovalDecision) {
        for signature in signatures {
            match decision {
                ApprovalDecision::Approved => self.grants.grant(signature.clone()),
                ApprovalDecision::Rejected(reason) => {
                    self.grants.deny(signature.clone(), reason.clone())
                }
            }
        }
    }
}

impl ToolGate for PermissionGate {
    fn decide(&self, action: &Action) -> PermissionDecision {
        // A recorded decision wins over the policy: it both honors the human and
        // breaks the ask → retry → ask loop.
        match self.grants.lookup(&action.signature()) {
            Some(Grant::Granted) => return PermissionDecision::Allow,
            Some(Grant::Denied(reason)) => {
                return PermissionDecision::Deny {
                    reason: reason.clone(),
                }
            }
            None => {}
        }
        self.policy.evaluate(&PermissionRequest::new(
            self.agent_id,
            &self.agent_kind,
            action,
        ))
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
pub struct BaseAgent<F: ClawFs + 'static> {
    llm: ClawApi,
    /// Retry policy applied to every per-iteration LLM call.
    retry_policy: RetryPolicy,
    interruption: AgentInterruption,
    memory: ConversationMemory<F>,
    tools: Option<ToolSet>,
    /// Tools allowed to execute this phase ("soft-hide" gating). `None` = ungated
    /// (every tool in `tools` may run). Set per semantic state by an upper layer
    /// via [`set_active_tools`](Self::set_active_tools); the full `tools` schema
    /// is always sent regardless, so the cached prompt prefix stays stable.
    allowed_tools: Option<AllowedTools>,
    /// A transient instruction appended to the tail of the messages sent to the
    /// LLM (never persisted to memory). Carries the soft-hide phase note (the
    /// permitted tools), generated from the allow-set by
    /// [`set_active_tools`](Self::set_active_tools) and dropped by
    /// [`clear_active_tools`](Self::clear_active_tools).
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
    /// The tool-policy prompt section — every tool's usage prose, produced once
    /// from the final [`ToolSet`] at build (`None` when no tool carries usage).
    /// Placed into the assembled prompt by `claw-context`; fixed across tasks.
    tool_context: Option<String>,
    /// Cached assembled prompt (instruction + tool policy + skills context). Empty
    /// means "nothing to assemble, borrow `system_prompt` directly". Rebuilt only
    /// when `effective_prompt_dirty`.
    effective_prompt: String,
    effective_prompt_dirty: bool,
    /// Open group guard from task start until the first response, so the user turn
    /// and the assistant reply commit as one group (compaction never orphans a
    /// reply with no user turn).
    open_turn: Option<GroupGuard<F>>,
    /// The permission gate consulted per tool call (`None` = no permission layer:
    /// every call that passes soft-hide runs). Owns the grant store of human
    /// decisions; mutated when an [`ApprovalResult`](AgentCommand::ApprovalResult)
    /// resolves a pending ask.
    gate: Option<PermissionGate>,
    /// Action signatures awaiting the current human decision — the calls the
    /// permission policy asked about this tick. Recorded into the gate's grant
    /// store when the [`ApprovalResult`](AgentCommand::ApprovalResult) arrives,
    /// then cleared.
    pending_grant_signatures: Vec<String>,
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

impl<F: ClawFs + 'static> BaseAgent<F> {
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
    pub fn builder(llm: ClawApi, memory: ConversationMemory<F>) -> BaseAgentBuilder<F> {
        BaseAgentBuilder {
            llm,
            memory,
            tools: None,
            skills: None,
            system_prompt: String::new(),
            retry_policy: RetryPolicy::default(),
            tool_block_retries: DEFAULT_TOOL_BLOCK_RETRIES,
            permission_policy: None,
            agent_id: 0,
            agent_kind: String::new(),
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
        let next = classify(self.projected_lifecycle, &command, self.pending_approval)?;
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
    /// // A tick that paused on a permission `Ask` hands back the id.
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
    //
    // Consumed by the in-crate semantic agents: `ConversationAgent` re-gates on
    // each phase change via these.

    /// Restrict which tools may *execute* until changed ("soft-hide" gating).
    ///
    /// Two coupled effects, from the one allow-set so they cannot desync:
    /// - **Enforcement:** the full tool schema is still sent every iteration (so
    ///   the cached prompt prefix never moves), but a call to a tool not in
    ///   `allowed` is refused at execution time — see
    ///   [`BaseAgentBuilder::with_tool_block_retries`].
    /// - **Prevention:** a transient phase note listing the permitted tools is
    ///   appended to the tail of the next request's messages (the production
    ///   "system-reminder" pattern), so the model avoids blocked calls up front.
    ///   The note is never written to memory, keeping the cached prefix intact.
    ///
    /// Crate-internal: the in-crate semantic agents set this automatically on
    /// each semantic state change (including mid-task), so gating tracks the
    /// FSM. It is not part of the public boundary — external callers drive the
    /// agent through semantic commands, not by toggling gating directly.
    ///
    /// Reserved soft-tools seam: no in-crate driver toggles phase gating today
    /// (only the gating tests exercise it), so it is `dead_code` until a phased
    /// agent reattaches; the enforcement path in [`ToolRunner`] stays wired.
    ///
    /// [`ToolRunner`]: crate::tool_runner::ToolRunner
    #[allow(dead_code)]
    pub(crate) fn set_active_tools(&mut self, allowed: AllowedTools) {
        self.tail_note = Some(Self::phase_note(&allowed));
        self.allowed_tools = Some(allowed);
    }

    /// Remove tool gating: every tool in the set may run again (the default),
    /// and drop the accompanying phase note.
    ///
    /// Reserved soft-tools seam (see [`set_active_tools`](Self::set_active_tools)).
    #[allow(dead_code)]
    pub(crate) fn clear_active_tools(&mut self) {
        self.allowed_tools = None;
        self.tail_note = None;
    }

    /// Build the transient phase note from the allow-set: a single
    /// "system-reminder" line naming the tools the model may use this phase, so
    /// its wording stays in lock-step with what enforcement will actually allow.
    ///
    /// Reserved soft-tools seam (see [`set_active_tools`](Self::set_active_tools)).
    #[allow(dead_code)]
    fn phase_note(allowed: &AllowedTools) -> String {
        let names = allowed.sorted_names();
        if names.is_empty() {
            "[system] No tools are available in the current phase; do not call any \
             tool."
                .to_string()
        } else {
            format!(
                "[system] Tools available in the current phase: {}. Other tools \
                 are temporarily unavailable — do not call them.",
                names.join(", ")
            )
        }
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

    /// The permission gate to consult this iteration, as a trait object (`None`
    /// when no permission policy is configured).
    fn tool_gate(&self) -> Option<&dyn ToolGate> {
        self.gate.as_ref().map(|gate| gate as &dyn ToolGate)
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
            gate: self.tool_gate(),
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
            let mut sink = self
                .control
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
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
                self.pending_grant_signatures.clear();
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
                // Record the decision against the asked-about actions so the
                // retried tool calls resolve without asking again.
                self.record_grants(&decision);
                self.pending_approval = None;
                self.lifecycle = AgentState::Running;
            }
            Inbound::Control(ControlSignal::EndConversation { final_message }) => {
                let turn = self.open_turn.take().unwrap_or_else(|| self.memory.group());
                turn.append_patch(
                    &json!([{ "role": "assistant", "content": final_message.clone() }]),
                );
                drop(turn);
                self.lifecycle = AgentState::Idle;
                self.outcome = Some(TickOutcome::Ended { final_message });
            }
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
                    // even for gating-blocked / permission-refused calls (each got
                    // a matched tool error), so committing never leaves a dangling
                    // call.
                    self.commit_patch(&tools.appended.0);
                    self.apply_tool_block_policy(&tools.runs);
                    // A permission `Ask` pauses the agent for a human decision
                    // (unless the round already failed the task via the block
                    // policy above).
                    self.maybe_raise_approval(&tools.runs);
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
        tracing::warn!(
            consecutive = self.consecutive_tool_blocks,
            budget = self.tool_block_retries,
            tools = ?blocked,
            "tool gate blocked"
        );

        if self.consecutive_tool_blocks > self.tool_block_retries {
            let name = blocked
                .first()
                .map(|name| (*name).to_string())
                .unwrap_or_default();
            self.fail_with(AgentRunError::ToolNotPermitted { name });
        }
    }

    /// Pause for a human decision when the permission policy asked about any call
    /// this round. No-op if the round already produced an outcome (e.g. the block
    /// policy failed the task) or no call needs approval.
    ///
    /// The asked-about action signatures are remembered so the
    /// [`ApprovalResult`](AgentCommand::ApprovalResult) can grant/deny them; the
    /// approver sees the first call's reason as the summary.
    fn maybe_raise_approval(&mut self, runs: &[ToolRun]) {
        if self.outcome.is_some() {
            return;
        }
        let pending: Vec<(String, String)> = runs
            .iter()
            .filter_map(|run| {
                run.approval
                    .as_ref()
                    .map(|approval| (approval.summary.clone(), approval.signature.clone()))
            })
            .collect();
        let Some((summary, _)) = pending.first().cloned() else {
            return;
        };
        self.pending_grant_signatures = pending.into_iter().map(|(_, sig)| sig).collect();
        let id = self.allocate_approval_id();
        self.pending_approval = Some(id);
        self.lifecycle = AgentState::AwaitingApproval;
        self.outcome = Some(TickOutcome::AwaitingApproval { id, summary });
    }

    /// Record a human decision against the actions that were asked about, so the
    /// retried calls resolve directly. No-op without a permission gate.
    fn record_grants(&mut self, decision: &ApprovalDecision) {
        let signatures = std::mem::take(&mut self.pending_grant_signatures);
        if let Some(gate) = self.gate.as_mut() {
            gate.record_decision(&signatures, decision);
        }
    }

    /// End the task with a failure outcome, leaving the agent idle and reusable.
    fn fail_with(&mut self, error: AgentRunError) {
        tracing::warn!(%error, "base_agent task failed");
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
            tracing::info!("dropping preempted partial patch: unmatched tool_calls");
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

    /// Rebuild the cached assembled prompt if stale; otherwise a no-op.
    ///
    /// Assembles the agent's system prompt from its sections via `claw-context`,
    /// which owns placement (the Static instruction + tool-policy band, then the
    /// Durable active-skills band). When the agent has neither tool-usage prose nor
    /// active skill content, the cache is left empty as the signal to borrow
    /// `system_prompt` directly (no allocation).
    fn refresh_effective_prompt(&mut self) -> Result<(), SkillError> {
        if !self.effective_prompt_dirty {
            return Ok(());
        }
        let skill_context = match self.skills.as_mut() {
            Some(skills) => skills.context()?,
            None => "",
        };
        self.effective_prompt.clear();
        if self.tool_context.is_some() || !skill_context.is_empty() {
            self.effective_prompt = assemble_system_prompt(
                &self.system_prompt,
                self.tool_context.as_deref(),
                skill_context,
            );
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

/// Assemble the per-iteration system prompt from its sections, letting
/// `claw-context` own placement: the Static instruction and tool-policy blocks,
/// then the Durable active-skills block. Absent sections are omitted.
fn assemble_system_prompt(
    instruction: &str,
    tool_context: Option<&str>,
    skill_context: &str,
) -> String {
    let mut builder =
        ContextBuilder::new().with(Block::new(BlockKind::AgentInstruction, instruction));
    if let Some(tool_context) = tool_context {
        builder = builder.with(Block::new(BlockKind::ToolPolicy, tool_context));
    }
    if !skill_context.is_empty() {
        builder = builder.with(Block::new(BlockKind::ActiveSkills, skill_context));
    }
    builder
        .build()
        .map(|context| context.into_string())
        .unwrap_or_else(|error| {
            // The only build error is a duplicate canonical block; the kinds added
            // above are distinct, so this branch is unreachable. Degrade to the raw
            // instruction rather than panic if that invariant is ever broken.
            tracing::error!(%error, "system-prompt assembly failed; using base instruction");
            instruction.to_string()
        })
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
pub struct BaseAgentBuilder<F: ClawFs + 'static> {
    llm: ClawApi,
    memory: ConversationMemory<F>,
    tools: Option<ToolSet>,
    skills: Option<SkillSet>,
    system_prompt: String,
    retry_policy: RetryPolicy,
    tool_block_retries: u32,
    permission_policy: Option<Arc<dyn PermissionPolicy>>,
    agent_id: u64,
    agent_kind: String,
}

impl<F: ClawFs + 'static> BaseAgentBuilder<F> {
    /// Set the tools available to the agent across all tasks.
    ///
    /// Takes a pre-built [`ToolSet`]; the agent's built-in control tool
    /// (`end_conversation`) is merged on at [`build`](Self::build).
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

    /// Install a permission policy that gates every tool call. Each classified
    /// call is evaluated to `Allow` / `Ask` / `Deny`; `Ask` pauses the agent for a
    /// human decision (reusing the approval flow), and the decision is remembered
    /// so the retried call resolves directly.
    ///
    /// Without this, the agent has no permission layer: every call that passes
    /// soft-hide gating runs. Pair with [`with_identity`](Self::with_identity) so
    /// the policy sees the acting agent.
    pub fn with_permission_policy(mut self, policy: Arc<dyn PermissionPolicy>) -> Self {
        self.permission_policy = Some(policy);
        self
    }

    /// Set the acting agent's identity (numeric id + kind), passed to the
    /// permission policy on each evaluation. Defaults to `(0, "")`; only relevant
    /// when a [permission policy](Self::with_permission_policy) is installed.
    pub fn with_identity(mut self, agent_id: u64, agent_kind: impl Into<String>) -> Self {
        self.agent_id = agent_id;
        self.agent_kind = agent_kind.into();
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
    pub fn build(self) -> Result<BaseAgent<F>, BaseAgentBuildError> {
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

        // Produce the tool-policy prompt section once from the final tool set (the
        // assembler places it each iteration). Fixed for the agent's lifetime.
        let tool_context = tools.as_ref().and_then(ToolSet::tool_context);

        let gate = self.permission_policy.map(|policy| PermissionGate {
            policy,
            agent_id: self.agent_id,
            agent_kind: self.agent_kind,
            grants: GrantStore::new(),
        });

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
            tool_context,
            effective_prompt: String::new(),
            // Build the combined prompt on the first tick.
            effective_prompt_dirty: true,
            open_turn: None,
            gate,
            pending_grant_signatures: Vec::new(),
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

/// The FSM transition table: the single authority on whether `command` is legal
/// in `state`, and what state it leads to. A free function (no `&self`, and
/// independent of the memory backend `F`) so it is trivially testable and is the
/// one place command validity is decided — [`BaseAgent::apply_inbound`] trusts
/// its verdict.
///
/// `pending_approval` is the id the agent is currently waiting on; it is only
/// consulted in the [`AwaitingApproval`](AgentState::AwaitingApproval) state.
/// The match is exhaustive over every `(state, command)` pair so a new state or
/// command cannot be silently mishandled.
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

    fn classify(
        state: AgentState,
        command: &AgentCommand,
    ) -> Result<AgentState, AgentCommandError> {
        super::classify(state, command, None)
    }

    #[test]
    fn append_is_accepted_in_every_state() {
        assert_eq!(
            classify(AgentState::Idle, &append()),
            Ok(AgentState::Running)
        );
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
        assert_eq!(
            classify(AgentState::Paused, &cancel()),
            Ok(AgentState::Idle)
        );
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
                super::classify(state, &approval(1), None),
                Err(AgentCommandError::NotAwaitingApproval { state })
            );
        }
        // Matching id resumes; a mismatch is reported with both ids.
        assert_eq!(
            super::classify(
                AgentState::AwaitingApproval,
                &approval(7),
                Some(ApprovalId(7))
            ),
            Ok(AgentState::Running)
        );
        assert_eq!(
            super::classify(
                AgentState::AwaitingApproval,
                &approval(7),
                Some(ApprovalId(9))
            ),
            Err(AgentCommandError::ApprovalMismatch {
                expected: ApprovalId(9),
                got: ApprovalId(7),
            })
        );
    }
}

// ===========================================================================
// Tests: soft-hide tool gating (drives the pub(crate) gating hooks, so it must
// live in-crate; a small self-contained harness keeps it hermetic)
// ===========================================================================

#[cfg(test)]
mod gating_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::sync::Arc;

    use claw_api::{ClawApi, ClawApiConfig};
    use claw_interface::{CapturingHttp, ClawHttp, MemFs, ScriptedHttp, StdThread};
    use claw_memory::{
        ConversationConfig, ConversationDeps, ConversationMemory, MemoryTaskPool, NoopCompactor,
        PoolConfig,
    };
    use serde_json::{json, Value};

    use crate::agent::{AgentId, AgentRunError, BaseAgent, BaseAgentBuilder, TickOutcome};
    use crate::tools::{
        AllowedTools, Tool, ToolError, ToolHandler, ToolInvocation, ToolOutput, ToolSet,
    };

    // HTTP doubles (ScriptedHttp / CapturingHttp, httpmock feature) and the
    // never-compacts `NoopCompactor` (compactor-stub feature) are shared from
    // claw_interface / claw-memory.

    // Test tools ----------------------------------------------------------------

    /// Echoes its arguments back; used as the "allowed" tool.
    struct EchoTool;

    impl ToolHandler for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn schema(&self) -> &'static str {
            r#"{"type":"function","function":{"name":"echo","description":"Echo"}}"#
        }
        fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: format!("echo:{}", call.arguments_json),
                ok: true,
            })
        }
    }

    /// Writes something; used as the "disallowed" tool.
    struct WriterTool;

    impl ToolHandler for WriterTool {
        fn name(&self) -> &'static str {
            "writer"
        }
        fn schema(&self) -> &'static str {
            r#"{"type":"function","function":{"name":"writer","description":"Write"}}"#
        }
        fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: "wrote".into(),
                ok: true,
            })
        }
    }

    fn caller_tools() -> ToolSet {
        ToolSet::new([Tool::new(EchoTool), Tool::new(WriterTool)]).expect("tool set")
    }

    // Builders / drivers --------------------------------------------------------

    fn build_llm(http: Arc<dyn ClawHttp>) -> ClawApi {
        ClawApi::init(
            ClawApiConfig {
                api_key: Some("sk-test".into()),
                backend_type: "openai_compatible".into(),
                model: Some("gpt-test".into()),
                base_url: Some("https://example.invalid".into()),
                supports_tools: true,
                ..Default::default()
            },
            http,
        )
        .expect("init llm")
    }

    fn scripted_llm(bodies: Vec<String>) -> ClawApi {
        build_llm(Arc::new(ScriptedHttp::new(bodies)))
    }

    fn test_memory(agent_id: AgentId) -> ConversationMemory<Arc<MemFs>> {
        let pool = Arc::new(
            MemoryTaskPool::new(PoolConfig::default(), StdThread::default()).expect("memory pool"),
        );
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

    /// A builder plus a cloned read-only view of the same memory.
    fn builder_with_view(
        llm: ClawApi,
        agent_id: AgentId,
    ) -> (BaseAgentBuilder<Arc<MemFs>>, ConversationMemory<Arc<MemFs>>) {
        let memory = test_memory(agent_id);
        let view = memory.clone();
        (BaseAgent::builder(llm, memory), view)
    }

    fn body_plain_text(text: &str) -> String {
        json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }

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

    fn body_end_conversation(final_message: &str) -> String {
        body_tool_call(
            "e1",
            "end_conversation",
            &json!({ "final_message": final_message }).to_string(),
        )
    }

    fn run_to_completion(agent: &mut BaseAgent<Arc<MemFs>>) -> String {
        loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                TickOutcome::Yielded { text } => return text,
                TickOutcome::Ended { final_message } => return final_message,
                TickOutcome::Failed(error) => panic!("unexpected agent failure: {error}"),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    fn transcript_contents(view: &ConversationMemory<Arc<MemFs>>) -> Vec<String> {
        view.messages()
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

    fn first_tool_message(messages: &Value) -> Option<Value> {
        messages
            .as_array()?
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .cloned()
    }

    /// True when every assistant `tool_calls[].id` has a matching `tool` message.
    fn no_dangling_tool_calls(messages: &Value) -> bool {
        let Some(items) = messages.as_array() else {
            return false;
        };
        let mut expected: Vec<String> = Vec::new();
        let mut satisfied: Vec<String> = Vec::new();
        for message in items {
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        expected.push(id.to_string());
                    }
                }
            }
            if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
                satisfied.push(id.to_string());
            }
        }
        expected.iter().all(|id| satisfied.contains(id))
    }

    // Tests ---------------------------------------------------------------------

    /// A disallowed tool call is refused with a *matched* tool error (no dangling
    /// call) and, within the retry budget, the agent keeps working and self-corrects.
    #[test]
    fn disallowed_tool_is_refused_with_matched_error() {
        let (builder, view) = builder_with_view(
            scripted_llm(vec![
                body_tool_call("t1", "writer", "{}"),
                body_end_conversation("done"),
            ]),
            AgentId(1),
        );
        let mut agent = builder.with_tools(caller_tools()).build().expect("build");
        agent.set_active_tools(AllowedTools::new(["end_conversation"]));

        agent.run("go");
        assert_eq!(run_to_completion(&mut agent), "done");

        let messages = view.messages();
        assert!(
            no_dangling_tool_calls(&messages),
            "blocked call left dangling"
        );
        let tool_message = first_tool_message(&messages).expect("a tool message was committed");
        assert_eq!(tool_message["tool_call_id"], "t1");
        assert_eq!(tool_message["is_error"], true);
        let content = tool_message["content"].as_str().unwrap_or_default();
        assert!(
            content.contains("not available in the current phase"),
            "unexpected blocked-tool content: {content}"
        );
    }

    /// With the default budget (1), a second *consecutive* blocked round fails the
    /// task with `ToolNotPermitted` naming the refused tool.
    #[test]
    fn two_consecutive_blocks_fail_the_task() {
        let (builder, _view) = builder_with_view(
            scripted_llm(vec![
                body_tool_call("t1", "writer", "{}"),
                body_tool_call("t2", "writer", "{}"),
            ]),
            AgentId(1),
        );
        let mut agent = builder.with_tools(caller_tools()).build().expect("build");
        agent.set_active_tools(AllowedTools::new(["end_conversation"]));

        agent.run("go");
        // First block: nudged, still working.
        assert!(matches!(agent.tick(), TickOutcome::Working));
        // Second consecutive block: budget exhausted -> failed.
        match agent.tick() {
            TickOutcome::Failed(AgentRunError::ToolNotPermitted { name }) => {
                assert_eq!(name, "writer")
            }
            other => panic!("expected ToolNotPermitted, got {other:?}"),
        }
        // Failed leaves the agent idle and reusable.
        assert!(!agent.is_running());
    }

    /// A clean tool round between two blocks resets the counter, so the budget of 1
    /// is never exceeded and the task completes.
    #[test]
    fn clean_round_resets_block_counter() {
        let (builder, _view) = builder_with_view(
            scripted_llm(vec![
                body_tool_call("t1", "writer", "{}"), // block (count 1)
                body_tool_call("t2", "echo", "{}"),   // clean (reset to 0)
                body_tool_call("t3", "writer", "{}"), // block (count 1 again)
                body_end_conversation("done"),
            ]),
            AgentId(1),
        );
        let mut agent = builder.with_tools(caller_tools()).build().expect("build");
        // echo is permitted (the clean round); writer is not.
        agent.set_active_tools(AllowedTools::new(["echo", "end_conversation"]));

        agent.run("go");
        assert_eq!(run_to_completion(&mut agent), "done");
    }

    /// `0` retries fails on the very first disallowed call.
    #[test]
    fn zero_retries_fails_on_first_block() {
        let (builder, _view) = builder_with_view(
            scripted_llm(vec![body_tool_call("t1", "writer", "{}")]),
            AgentId(1),
        );
        let mut agent = builder
            .with_tools(caller_tools())
            .with_tool_block_retries(0)
            .build()
            .expect("build");
        agent.set_active_tools(AllowedTools::new(["end_conversation"]));

        agent.run("go");
        match agent.tick() {
            TickOutcome::Failed(AgentRunError::ToolNotPermitted { name }) => {
                assert_eq!(name, "writer")
            }
            other => panic!("expected ToolNotPermitted, got {other:?}"),
        }
    }

    /// With no allow-set, gating is off: a tool that gating *would* block runs
    /// normally (the pre-gating behaviour is preserved).
    #[test]
    fn ungated_when_no_allow_set() {
        let (builder, view) = builder_with_view(
            scripted_llm(vec![
                body_tool_call("t1", "writer", "{}"),
                body_end_conversation("done"),
            ]),
            AgentId(1),
        );
        // Note: set_active_tools is intentionally NOT called.
        let mut agent = builder.with_tools(caller_tools()).build().expect("build");

        agent.run("go");
        assert_eq!(run_to_completion(&mut agent), "done");
        // The writer tool actually executed.
        assert!(transcript_contents(&view).iter().any(|c| c == "wrote"));
    }

    /// Clearing the gating restores the defaults: the previously blocked tool runs
    /// again and no phase note is appended to the request.
    #[test]
    fn clearing_gating_restores_ungated_and_no_note() {
        let http = CapturingHttp::new(vec![
            body_tool_call("t1", "writer", "{}"),
            body_end_conversation("done"),
        ]);
        let llm = build_llm(Arc::clone(&http) as Arc<dyn ClawHttp>);
        let (builder, view) = builder_with_view(llm, AgentId(1));
        let mut agent = builder.with_tools(caller_tools()).build().expect("build");

        // Gate (which also sets the phase note), then immediately ungate before
        // running — clearing must drop both the allow-set and the note.
        agent.set_active_tools(AllowedTools::new(["end_conversation"]));
        agent.clear_active_tools();

        agent.run("go");
        assert_eq!(run_to_completion(&mut agent), "done");

        // The writer tool executed (gating was cleared).
        assert!(transcript_contents(&view).iter().any(|c| c == "wrote"));

        // No request carried a phase note (the "[system] Tools available" reminder).
        for body in http.captured_bodies() {
            if let Some(messages) = body["messages"].as_array() {
                assert!(
                    messages.iter().all(|m| {
                        m.get("content")
                            .and_then(Value::as_str)
                            .is_none_or(|c| !c.contains("Tools available in the current phase"))
                    }),
                    "a phase note reached the model after gating was cleared"
                );
            }
        }
    }

    /// Gating auto-generates a phase note that is appended to the request the model
    /// sees (last message, naming the allowed tools) but is never written to memory.
    #[test]
    fn gating_phase_note_reaches_model_but_not_memory() {
        let http = CapturingHttp::new(vec![body_plain_text("hi there")]);
        let llm = build_llm(Arc::clone(&http) as Arc<dyn ClawHttp>);
        let (builder, view) = builder_with_view(llm, AgentId(1));
        let mut agent = builder.with_tools(caller_tools()).build().expect("build");
        // Gating sets the note as a side effect; no separate note API.
        agent.set_active_tools(AllowedTools::new(["echo", "end_conversation"]));

        agent.run("hello");
        assert_eq!(run_to_completion(&mut agent), "hi there");

        // The request carried the auto note as the final (user) message, naming the
        // permitted tools in stable order.
        let body = http.captured_bodies().pop().expect("one captured request");
        let messages = body["messages"].as_array().expect("messages array");
        let last = messages.last().expect("at least one message");
        assert_eq!(last["role"], "user");
        let note = last["content"].as_str().expect("note content");
        assert!(note.contains("Tools available in the current phase"));
        assert!(note.contains("echo"));
        assert!(note.contains("end_conversation"));

        // Memory holds the real turn but not the transient note.
        let committed = transcript_contents(&view);
        assert!(committed.iter().any(|c| c == "hello"));
        assert!(committed.iter().any(|c| c == "hi there"));
        assert!(
            !committed
                .iter()
                .any(|c| c.contains("Tools available in the current phase")),
            "phase note leaked into memory"
        );
    }
}
