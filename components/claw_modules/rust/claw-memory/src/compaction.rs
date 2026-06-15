//! The compaction seam.
//!
//! Compaction is "fold an aged window of conversation messages into a shorter
//! summary so the working context stays inside the model's budget". *How* that
//! summary is produced (an LLM summarization call, a cheap heuristic, …) is a
//! policy decision that does not belong to the conversation tape, so it is
//! injected as a [`Compactor`].
//!
//! The conversation memory owns the *mechanism* (when to compact, which
//! messages, splicing the result back in, persistence) in
//! [`crate::conversation_memory`]; the `Compactor` owns only the
//! *transformation*. This keeps it free of any LLM dependency and lets future
//! memory types reuse the same trait.

use serde_json::Value;

/// Failure from a [`Compactor`].
///
/// Compaction is best-effort: on error the tape keeps the un-compacted messages
/// and tries again later, so the only thing a caller needs is a displayable
/// reason for logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompactError {
    #[error("compaction backend failed: {0}")]
    Backend(String),
}

/// Turns an aged window of chat messages into a compact summary.
///
/// `window` is a self-contained snapshot (already detached from the tape's
/// lock): the previous summary followed by the oldest raw messages being retired.
/// The returned messages *replace* the tape's summary section, so an
/// implementation is free to re-summarize the whole window into one message or a
/// small handful.
///
/// Implementations must be `Send + Sync`: the call runs on a memory-pool worker,
/// off the agent's tick path, and may block (e.g. on the network).
pub trait Compactor: Send + Sync {
    /// Summarize `window`. Returning an empty `Vec` clears the summary section.
    fn compact(&self, window: &[Value]) -> Result<Vec<Value>, CompactError>;
}
