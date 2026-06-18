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
/// Compaction is best-effort: on error the tape keeps the un-compacted groups
/// and tries again later, so the only thing a caller needs is a displayable
/// reason for logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompactError {
    /// The summarization backend (e.g. the LLM client) failed; carries a
    /// displayable reason for logging.
    #[error("compaction backend failed: {0}")]
    Backend(String),
}

/// Turns one chunk of aged chat messages into a compact summary.
///
/// `window` is a self-contained snapshot (already detached from the tape's
/// lock) of the oldest groups being retired in this round — bounded by the
/// tape's `segment_token_budget`. It contains only raw messages; the tape does
/// **not** fold a previous summary in, because each chunk becomes its own
/// independent, non-overlapping compact segment covering a distinct id range.
///
/// The returned messages *are* that segment, so an implementation is free to
/// fold the chunk into a single message or a small handful. Returning an empty
/// `Vec` is allowed but produces an empty segment; prefer a concise summary.
///
/// Implementations must be `Send + Sync`: the call runs on a memory-pool worker,
/// off the agent's tick path, and may block (e.g. on the network).
///
/// # Examples
///
/// A trivial compactor that records how many messages it folded:
///
/// ```
/// use claw_memory::{CompactError, Compactor};
/// use serde_json::{json, Value};
///
/// struct CountingCompactor;
///
/// impl Compactor for CountingCompactor {
///     fn compact(&self, window: &[Value]) -> Result<Vec<Value>, CompactError> {
///         Ok(vec![json!({
///             "role": "system",
///             "content": format!("summary of {} earlier messages", window.len()),
///         })])
///     }
/// }
///
/// let summary = CountingCompactor
///     .compact(&[json!({ "role": "user", "content": "hi" })])
///     .unwrap();
/// assert_eq!(summary[0]["content"], "summary of 1 earlier messages");
/// ```
pub trait Compactor: Send + Sync {
    /// Summarize one chunk of aged messages into the messages of a single
    /// compact segment.
    fn compact(&self, window: &[Value]) -> Result<Vec<Value>, CompactError>;
}

/// A [`Compactor`] that never compacts: every call yields an empty segment.
///
/// For wiring where background summarization is undesired or irrelevant — host
/// CLIs that keep the full transcript, and tests that need a memory without an
/// LLM. Behind the `compactor-stub` feature so it is never built into firmware
/// unless explicitly opted in.
#[cfg(feature = "compactor-stub")]
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCompactor;

#[cfg(feature = "compactor-stub")]
impl Compactor for NoopCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(Vec::new())
    }
}
