//! Ephemeral reminders: the agent's per-request "nudge" channel.
//!
//! A reminder is a transient instruction appended to the **tail** of the
//! messages sent to the LLM for one request and **never persisted** to memory.
//! It is the home for volatile, per-request guidance — e.g. the soft-hide phase
//! note naming the tools permitted this phase — that must reach the model
//! without moving the cached system/history prefix (the production
//! "system-reminder" pattern).
//!
//! # Determinism
//!
//! Reminders are one of the three context homes (stable prose -> a context
//! `Block`; real conversation/tool events -> persisted memory; per-request
//! transient nudges -> here). Nothing else may inject into a request tail.
//!
//! # Memory
//!
//! The rendered messages live in a **reused buffer** rebuilt only when the
//! reminder set changes (dirty-gated), so a steady reminder set costs nothing
//! per iteration. Each reminder renders once as a trailing `user` message
//! wrapped in a `<system-reminder>` envelope.

use serde_json::{json, Value};

/// The agent's ephemeral reminder channel. Holds the source texts plus a reused
/// render buffer; call [`refresh`](Self::refresh) once per tick before reading
/// [`as_slice`](Self::as_slice).
pub(crate) struct Reminders {
    /// Source reminder texts, in order. The single source of truth.
    texts: Vec<String>,
    /// Reused render buffer: one trailing `user` message per text, rebuilt only
    /// when `dirty`.
    rendered: Vec<Value>,
    /// The render buffer is stale relative to `texts`.
    dirty: bool,
}

impl Reminders {
    /// An empty reminder channel.
    pub(crate) fn new() -> Self {
        Self {
            texts: Vec::new(),
            rendered: Vec::new(),
            dirty: false,
        }
    }

    /// Replace all reminders with a single text (the common phase-note case).
    pub(crate) fn set_single(&mut self, text: String) {
        self.texts.clear();
        self.texts.push(text);
        self.dirty = true;
    }

    /// Drop all reminders. No-op (and no re-render) when already empty.
    pub(crate) fn clear(&mut self) {
        if self.texts.is_empty() {
            return;
        }
        self.texts.clear();
        self.dirty = true;
    }

    /// Rebuild the rendered buffer if the reminder set changed since the last
    /// render; otherwise a no-op. Reuses the buffer's allocation.
    pub(crate) fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.rendered.clear();
        for text in &self.texts {
            self.rendered.push(json!({
                "role": "user",
                "content": format!("<system-reminder>\n{text}\n</system-reminder>"),
            }));
        }
        self.dirty = false;
    }

    /// The rendered trailing messages for this request. Call
    /// [`refresh`](Self::refresh) earlier this tick so the buffer is current.
    pub(crate) fn as_slice(&self) -> &[Value] {
        &self.rendered
    }
}
