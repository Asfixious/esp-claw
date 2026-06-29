//! The memory event bus: the [`AgentEvent`] vocabulary an agent emits and the
//! [`Memory`] trait a pluggable memory subscribes through.
//!
//! # Why an event bus
//!
//! An agent's memory-relevant data has exactly three sources — the user's
//! messages, the model's own output (plain answers and tool activity), and
//! lifecycle boundaries (a new iteration, a cancellation). Rather than wiring
//! each memory type into the agent's reducer by hand (which couples the agent to
//! every memory implementation and makes a second memory type a merge conflict),
//! the agent emits a single [`AgentEvent`] at each boundary and every registered
//! [`Memory`] observes it. New memory types plug in without touching the agent.
//!
//! # The three facets of a memory
//!
//! A [`Memory`] is a unified object with three roles, all optional:
//! - **observe** — [`on_event`](Memory::on_event) ingests [`AgentEvent`]s (e.g. to
//!   schedule background extraction);
//! - **contribute context** — [`render_context`](Memory::render_context) emits
//!   [`Block`]s into the agent's [`Context`](claw_context::Context) each iteration
//!   (pull, not push: only the agent's own tick thread may read shared memory into
//!   its private context);
//! - **provide tools** — [`tools`](Memory::tools) hands the agent model-callable
//!   tools (e.g. `memory_recall`) registered alongside the agent's own.
//!
//! Keeping all three on one trait means registering a memory is a single call
//! that wires up everything it needs.

use claw_context::Block;
use claw_tool::ToolGroup;
use serde_json::Value;

use crate::iteration_loop::IterationId;

/// A boundary event an agent emits for its memories to observe.
///
/// Borrows its payload from the agent's transient state, so observing is
/// allocation-free on the emit path — a memory that needs to retain anything
/// copies what it wants. `#[non_exhaustive]` so new boundaries can be added
/// without breaking existing memories (they match with a `_` arm).
#[derive(Debug)]
#[non_exhaustive]
pub enum AgentEvent<'a> {
    /// A genuine user message was appended (not an internal marker).
    UserMessage {
        /// The user's text.
        text: &'a str,
    },
    /// A new iteration is about to run.
    IterationStarted {
        /// The iteration's id.
        iteration: IterationId,
    },
    /// The model returned a user-facing plain-text answer (a turn boundary).
    AssistantMessage {
        /// The model's answer text.
        text: &'a str,
    },
    /// A committed assistant+tool patch (model reasoning plus tool results).
    ToolActivity {
        /// The materialized patch (a JSON array of messages).
        patch: &'a Value,
    },
    /// The task was cancelled; carries the human-readable reason marker.
    Cancelled {
        /// Why the task was abandoned.
        reason: &'a str,
    },
}

/// Failure from a [`Memory`] observing an [`AgentEvent`].
///
/// Observation is best-effort: the agent logs a returned error and carries on
/// (a memory must never break the agent), so the only thing a caller needs is a
/// displayable reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// The memory could not process the event; carries a displayable reason.
    #[error("memory observation failed: {0}")]
    Observation(String),
}

/// A pluggable memory: an observer of [`AgentEvent`]s that can also contribute
/// context blocks and model-callable tools. See the module docs for the three
/// facets.
///
/// Implementations are shared as `Arc<dyn Memory>` and driven from the agent's
/// single tick thread, but must be `Send + Sync` because a memory typically owns
/// state also written by background workers (e.g. an extraction pool job).
pub trait Memory: Send + Sync {
    /// A stable identifier for this memory, used in logs.
    fn id(&self) -> &str;

    /// Observe one agent boundary event.
    ///
    /// The default ignores every event (for memories that only contribute
    /// context or tools). Best-effort: a returned [`MemoryError`] is logged by the
    /// agent and never interrupts the tick.
    fn on_event(&self, event: &AgentEvent<'_>) -> Result<(), MemoryError> {
        let _ = event;
        Ok(())
    }

    /// Contribute context blocks for the current iteration by calling `emit` once
    /// per block.
    ///
    /// Called by the agent's tick thread just before it assembles the request, so
    /// a memory shared across agents (and written by background workers) is read
    /// into *this* agent's [`Context`](claw_context::Context) only here, on the
    /// thread that owns it. Emitting an empty block removes that block (the
    /// context drops blank content), so a memory can clear a section by emitting
    /// it empty. The default emits nothing.
    fn render_context(&self, emit: &mut dyn FnMut(Block<'_>)) {
        let _ = emit;
    }

    /// The model-callable tools this memory provides, if any.
    ///
    /// Merged into the agent's tool set when the memory is registered. Tool names
    /// must be globally unique across the agent's tools (a clash is rejected at
    /// registration). The default provides none.
    fn tools(&self) -> Option<ToolGroup> {
        None
    }
}
