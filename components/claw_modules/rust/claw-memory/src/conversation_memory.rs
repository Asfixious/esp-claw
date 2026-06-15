//! `ConversationMemory` — the agent's short-term conversation memory.
//!
//! This is the running transcript the model needs for short-term continuity. Its
//! natural unit is a **group**: one turn's worth of messages, produced together
//! by the agent loop:
//!
//! ```text
//! group {                       // one turn
//!   user message
//!   assistant message (text and/or tool_calls)
//!   tool result(s)
//!   assistant message
//! }
//! ```
//!
//! The memory owns, oldest-to-newest when rendered:
//! - `pinned`  — standalone messages never compacted (e.g. a task brief).
//! - `summary` — the folded summary of retired groups.
//! - `groups`  — committed turns, verbatim.
//! - the **open group** — the in-progress turn (see [`GroupGuard`]).
//!
//! Every group and pin is stamped with a monotonic `id` so its chronological
//! position is stable across compaction and reloads.
//!
//! # Turns are built through a guard
//!
//! Conversation content is appended through a [`GroupGuard`] from
//! [`group`](ConversationMemory::group). The guard buffers the open turn and
//! commits it as a single record when it drops, so a turn can never be left
//! half-open and compaction never splits a tool round (it cuts only on group
//! boundaries):
//!
//! ```ignore
//! {
//!     let mut turn = memory.group();
//!     turn.append_user("hi");
//!     let reply = agent.chat(&turn.messages()); // includes the open turn
//!     turn.append_assistant(reply.raw_json());
//!     turn.append_tool_result("call_1", &out, false);
//! } // drop → the whole turn is committed as one record
//! ```
//!
//! [`group`](ConversationMemory::group) takes `&mut self`, so the borrow checker
//! guarantees **at most one open turn at a time**, and — together with the
//! threading model below — that the foreground is the only writer of the state.
//!
//! # Threading: one writer, the pool only computes
//!
//! All state mutation (appends, commits, persistence, applying a summary) happens
//! on the **foreground** thread that owns the memory. The shared
//! [`MemoryTaskPool`] is a pure *compute* helper: a worker only takes a read
//! snapshot of the aged window and runs the (slow, possibly networked)
//! [`Compactor`], then parks the resulting summary in `parked_summary`. It never
//! touches `groups` / `pending` / the data files. The next
//! [`group`](ConversationMemory::group) / [`flush`](ConversationMemory::flush)
//! applies that parked summary on the foreground.
//!
//! Because there is exactly one mutator thread, a single `state` mutex is
//! sufficient and can be held across the (debounced, small) filesystem writes
//! without a second lock or a release-and-reacquire window — so there is no
//! check-then-act race between persistence and compaction. The cost is that a
//! flush happens on the agent thread and that a finished summary is spliced in at
//! the next turn boundary rather than the instant it is ready. **A single memory
//! must therefore be driven from one thread.**
//!
//! # Identity and persistence
//!
//! Each memory is keyed by a `conversation_id`. The id selects two files under
//! [`ConversationConfig::dir`]:
//!
//! - `conversation-<id>.jsonl` — the **data log**: one JSON record per line
//!   (`group` / `pin` / `compact`), **append-only**. The source of truth.
//! - `conversation-<id>.json` — the **index manifest**: a single JSON object
//!   listing the byte `(off, len)` of every *live* record plus `covered_len` and
//!   `next_id`. A rebuildable cache, rewritten atomically only when the live/dead
//!   structure changes (compaction, collapse, [`flush`](ConversationMemory::flush)).
//!
//! Committing a turn appends one `group` record. Compaction is an appended
//! `compact` record carrying the group-id range it supersedes, so retired groups
//! become dead bytes; a **collapse** (atomic rewrite of both files from the live
//! set) reclaims them once dead bytes dominate. Load reads the manifest,
//! `read_at`s the live records (skipping dead ones), then tail-scans for records
//! appended since the last manifest write; a missing/short manifest falls back to
//! a full data-log scan.
//!
//! # Hidden auto-compaction
//!
//! When committing a turn pushes the size estimate past the threshold, the memory
//! submits a one-shot job to the shared [`MemoryTaskPool`]; a worker summarizes
//! the aged groups via the injected [`Compactor`] and parks the summary. The next
//! foreground turn boundary splices it in (retire the covered groups, append a
//! `compact` record). The state lock is never held across the [`Compactor`] call.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use claw_interfaces::ClawFs;

use crate::compaction::Compactor;
use crate::pool::MemoryTaskPool;

/// Token budget past which a background compaction is scheduled.
const DEFAULT_COMPACT_THRESHOLD_TOKENS: usize = 6000;
/// How many of the newest groups (turns) are always kept verbatim.
const DEFAULT_KEEP_RECENT: usize = 8;
/// Minimum gap between persistence writes (flash-wear debounce).
const DEFAULT_PERSIST_DEBOUNCE: Duration = Duration::from_secs(5);
/// Rough bytes-per-token divisor for the size estimate. See [`estimate_message_tokens`].
const CHARS_PER_TOKEN: usize = 4;
/// Per-conversation filenames: `{dir}/{FILE_PREFIX}{id}{DATA_EXT|INDEX_EXT}`.
const FILE_PREFIX: &str = "conversation-";
const DATA_EXT: &str = ".jsonl";
const INDEX_EXT: &str = ".json";
/// Manifest schema version, so a future layout change can be detected on load.
const MANIFEST_VERSION: u32 = 1;
/// Don't collapse below this data size — tiny files aren't worth rewriting.
const COLLAPSE_FLOOR_BYTES: ByteLen = ByteLen(8 * 1024);

/// A monotonic logical identifier for a group or pin.
///
/// This fixes chronological order and is the *only* thing compaction supersedes
/// (see [`apply_record`]). It is deliberately a distinct type from byte
/// offsets/lengths so the two can never be swapped: supersession is keyed on
/// `RecordId`, addressing on [`ByteOffset`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
struct RecordId(u64);

impl RecordId {
    /// The id that immediately follows this one.
    fn next(self) -> RecordId {
        RecordId(self.0.saturating_add(1))
    }
}

/// A byte position within the data log. Addressing only — never compared for
/// chronology (that is [`RecordId`]'s job).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
struct ByteOffset(usize);

impl ByteOffset {
    /// The offset `len` bytes further on.
    fn advance(self, len: ByteLen) -> ByteOffset {
        ByteOffset(self.0.saturating_add(len.0))
    }
    /// As the `u64` the [`ClawFs`] read API expects (widening, lossless).
    fn as_u64(self) -> u64 {
        self.0 as u64
    }
    /// This position viewed as the length of the region `[0, self)`.
    fn as_len(self) -> ByteLen {
        ByteLen(self.0)
    }
}

/// A number of bytes: one record line, the whole data log, a covered prefix, …
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
struct ByteLen(usize);

impl ByteLen {
    /// The length of a serialized line.
    fn of(bytes: &[u8]) -> ByteLen {
        ByteLen(bytes.len())
    }
    /// As the `usize` the [`ClawFs`] read API expects.
    fn as_usize(self) -> usize {
        self.0
    }
    /// As the starting position just past a region of this size.
    fn as_offset(self) -> ByteOffset {
        ByteOffset(self.0)
    }
    fn saturating_add(self, other: ByteLen) -> ByteLen {
        ByteLen(self.0.saturating_add(other.0))
    }
    fn saturating_sub(self, other: ByteLen) -> ByteLen {
        ByteLen(self.0.saturating_sub(other.0))
    }
    fn saturating_mul(self, factor: usize) -> ByteLen {
        ByteLen(self.0.saturating_mul(factor))
    }
    /// From a [`ClawFs::len`] result, clamping if it somehow exceeds `usize` (a
    /// 32-bit device can only address `usize` bytes; conversation files are tiny).
    fn from_file_len(len: u64) -> ByteLen {
        ByteLen(usize::try_from(len).unwrap_or(usize::MAX))
    }
}

/// Tuning for a [`ConversationMemory`].
pub struct ConversationConfig {
    /// Base directory for per-conversation files, already resolved against the
    /// DATA root by the caller. The filenames are derived from the conversation id.
    pub dir: String,
    /// When the estimate exceeds this, compaction is scheduled in the background.
    pub compact_threshold_tokens: usize,
    /// Newest groups kept out of compaction so recent context stays exact.
    pub keep_recent: usize,
    /// Minimum interval between filesystem writes.
    pub persist_debounce: Duration,
}

impl ConversationConfig {
    /// Config for conversation files under `dir`, with default tuning otherwise.
    pub fn new(dir: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            compact_threshold_tokens: DEFAULT_COMPACT_THRESHOLD_TOKENS,
            keep_recent: DEFAULT_KEEP_RECENT,
            persist_debounce: DEFAULT_PERSIST_DEBOUNCE,
        }
    }
}

/// Injected collaborators for a [`ConversationMemory`].
pub struct ConversationDeps {
    /// Persistence backend (read on construction, written on the foreground).
    pub fs: Arc<dyn ClawFs>,
    /// Shared worker pool that runs the summarization compute off the tick path.
    pub pool: Arc<MemoryTaskPool>,
    /// How aged windows are summarized.
    pub compactor: Arc<dyn Compactor>,
}

/// One record as stored on a line of the data `.jsonl`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum LogRecord {
    /// A committed turn: its messages in order.
    Group { id: RecordId, msgs: Vec<Value> },
    /// A standalone pinned message (never compacted).
    Pin { id: RecordId, msg: Value },
    /// A summary that supersedes groups in `[id_start, id_end]`.
    Compact {
        id_start: RecordId,
        id_end: RecordId,
        summary: Vec<Value>,
    },
}

/// One live record's location inside the data `.jsonl`, as stored in the manifest.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum IndexEntry {
    Group {
        off: ByteOffset,
        len: ByteLen,
        id: RecordId,
    },
    Pin {
        off: ByteOffset,
        len: ByteLen,
        id: RecordId,
    },
    Compact {
        off: ByteOffset,
        len: ByteLen,
        id_start: RecordId,
        id_end: RecordId,
    },
}

/// The index manifest: the live layout of the data log, rewritten atomically.
#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// Data-log byte length this manifest describes; load tail-scans past it.
    covered_len: ByteLen,
    next_id: RecordId,
    live: Vec<IndexEntry>,
}

/// A pinned message plus its byte location in the data log (once flushed).
struct StoredPin {
    id: RecordId,
    value: Value,
    loc: Option<(ByteOffset, ByteLen)>,
}

/// A committed turn plus its byte location in the data log (once flushed).
struct StoredGroup {
    id: RecordId,
    msgs: Vec<Value>,
    loc: Option<(ByteOffset, ByteLen)>,
}

/// Which in-memory record a pending data line belongs to, so its `loc` can be
/// written back once the line is appended.
#[derive(Clone, Copy)]
enum PendingTarget {
    Group(RecordId),
    Pin(RecordId),
    Compact,
}

/// A serialized data line awaiting its append.
struct Pending {
    line: Vec<u8>,
    target: PendingTarget,
}

/// A summary computed by a pool worker, parked until the foreground applies it.
///
/// The pool only *produces* this; the foreground *consumes* it (retire the
/// covered groups, append the `compact` record). It carries the group-id range
/// it covers so application is order-independent and stable even though more
/// turns may have been committed since the snapshot was taken.
struct CompactionResult {
    id_start: RecordId,
    id_end: RecordId,
    summary: Vec<Value>,
}

/// The lock-protected contents of the memory.
#[derive(Default)]
struct MemoryState {
    pinned: Vec<StoredPin>,
    summary: Vec<Value>,
    /// Group-id range the current summary supersedes, `(id_start, id_end)`.
    summary_range: Option<(RecordId, RecordId)>,
    /// Byte location of the live `compact` record, once flushed.
    summary_loc: Option<(ByteOffset, ByteLen)>,
    groups: Vec<StoredGroup>,
    /// The in-progress turn's messages — not yet committed (volatile, no id).
    open_group: Vec<Value>,
    next_id: RecordId,

    /// Records appended in memory but not yet written to the `.jsonl`.
    pending: Vec<Pending>,
    /// Current byte length of the `.jsonl` (also the next append offset).
    data_len: ByteLen,
    /// Data length the on-disk manifest currently describes.
    manifest_covered_len: ByteLen,
    /// The manifest must be rewritten (a compaction/collapse changed live/dead).
    index_dirty: bool,

    /// A summary a pool worker finished but the foreground has not yet applied.
    /// Single-slot: single-flight + consume-before-reschedule keep it from being
    /// overwritten before it is applied.
    parked_summary: Option<CompactionResult>,

    approx_tokens: usize,
    last_persist: Option<Instant>,
}

/// Shared inner state — held behind an `Arc` so the pool worker and the agent
/// reference the same memory.
struct MemoryInner {
    /// Independent identity / storage key for this conversation.
    conversation_id: usize,
    /// Append-only data log path, derived once from `config.dir` + the id.
    data_path: String,
    /// Index manifest path.
    index_path: String,
    /// The single lock: the foreground holds it across mutations and the
    /// (debounced, small) filesystem writes. The pool only takes it briefly to
    /// read a snapshot and to park a result.
    state: Mutex<MemoryState>,
    /// Single-flight guard: at most one compaction job in the pool at a time.
    compaction_in_flight: AtomicBool,
    config: ConversationConfig,
    deps: ConversationDeps,
}

/// The agent's short-term conversation memory. See the module docs.
pub struct ConversationMemory {
    inner: Arc<MemoryInner>,
}

impl ConversationMemory {
    /// Build the memory for `conversation_id`, restoring its persisted contents
    /// if present (missing or unreadable files start empty).
    ///
    /// Different ids map to different files under [`ConversationConfig::dir`], so
    /// each conversation is stored independently.
    pub fn new(conversation_id: usize, config: ConversationConfig, deps: ConversationDeps) -> Self {
        let data_path = conversation_path(&config.dir, conversation_id, DATA_EXT);
        let index_path = conversation_path(&config.dir, conversation_id, INDEX_EXT);
        let state = load_state(deps.fs.as_ref(), &data_path, &index_path);
        Self {
            inner: Arc::new(MemoryInner {
                conversation_id,
                data_path,
                index_path,
                state: Mutex::new(state),
                compaction_in_flight: AtomicBool::new(false),
                config,
                deps,
            }),
        }
    }

    /// This memory's conversation id.
    pub fn conversation_id(&self) -> usize {
        self.inner.conversation_id
    }

    /// Open a turn. Append its messages through the returned [`GroupGuard`]; the
    /// whole turn is committed as one group when the guard drops.
    ///
    /// Takes `&mut self` so the borrow checker permits only one open turn at a
    /// time. Reach [`messages`](Self::messages) / [`flush`](Self::flush) through
    /// the guard while it is alive (it derefs to `&ConversationMemory`).
    pub fn group(&mut self) -> GroupGuard<'_> {
        GroupGuard { memory: self }
    }

    /// The current messages, ready to send to the model: `pinned ++ summary ++
    /// committed groups ++ open turn`, oldest first.
    ///
    /// Returns an owned, internally consistent snapshot (see the module docs for
    /// why it is not a borrow). Compaction state is already folded in; the caller
    /// neither sees nor triggers it.
    // todo: finalize the return shape — owned `Value` snapshot (current), a
    // cached `Arc<Value>` snapshot (cheap clone, invalidated on mutation), or a
    // borrowing guard. The choice trades clone cost against how long the caller
    // may hold the result while the background compactor wants to mutate.
    pub fn messages(&self) -> Value {
        let state = self.lock_state();
        let mut out = Vec::new();
        out.extend(state.pinned.iter().map(|p| p.value.clone()));
        out.extend(state.summary.iter().cloned());
        for group in &state.groups {
            out.extend(group.msgs.iter().cloned());
        }
        out.extend(state.open_group.iter().cloned());
        Value::Array(out)
    }

    /// Apply any finished summary, force any pending changes to disk now
    /// (ignoring the debounce), refresh the index manifest, and reclaim space.
    ///
    /// Persists only **committed** turns; an open turn is committed when its
    /// [`GroupGuard`] drops. The clean-shutdown order is therefore "drop the
    /// guard, then `flush`". This is the manual, immediate form of the automatic
    /// debounced persistence — same writes, just now.
    pub fn flush(&self) {
        apply_parked_summary(&self.inner);
        persist(&self.inner, true);
        maybe_collapse(&self.inner);
    }

    // -----------------------------------------------------------------------
    // Internals — never exposed.
    // -----------------------------------------------------------------------

    /// Queue a summarization-compute job, unless one is already queued/running
    /// (single-flight).
    ///
    /// The worker only reads a snapshot and runs the [`Compactor`]; it parks the
    /// result for the foreground to apply. The `keep_recent` work-gate lives in
    /// [`compute_summary`].
    fn schedule_compaction(&self) {
        if self.inner.compaction_in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        self.inner.deps.pool.submit(Box::new(move || {
            compute_summary(&inner);
            inner.compaction_in_flight.store(false, Ordering::Release);
        }));
    }

    fn lock_state(&self) -> MutexGuard<'_, MemoryState> {
        lock_state(&self.inner)
    }
}

/// An open turn. Append messages through it; the turn is committed as one group
/// record when the guard drops (or on an explicit [`commit`](Self::commit)).
///
/// Holds `&mut ConversationMemory`, so only one can be live at a time. It derefs
/// to `&ConversationMemory`, so `turn.messages()` / `turn.flush()` work while the
/// turn is open.
pub struct GroupGuard<'a> {
    memory: &'a mut ConversationMemory,
}

impl GroupGuard<'_> {
    /// Append a user / addon message to the open turn.
    pub fn append_user(&self, content: impl Into<String>) {
        self.push_open(json!({ "role": "user", "content": content.into() }));
    }

    /// Append a raw assistant message (plain text and/or `tool_calls`).
    ///
    /// `raw_message_json` is the backend-shaped assistant message object; an
    /// unparseable value is logged and dropped rather than corrupting the turn.
    pub fn append_assistant(&self, raw_message_json: &str) {
        match serde_json::from_str::<Value>(raw_message_json) {
            Ok(message) => self.push_open(message),
            Err(err) => log::warn!(
                "conversation {}: invalid assistant json: {err}",
                self.memory.inner.conversation_id
            ),
        }
    }

    /// Append a tool result for the call `tool_call_id` to the open turn.
    pub fn append_tool_result(&self, tool_call_id: &str, content: &str, is_error: bool) {
        self.push_open(json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
            "is_error": is_error,
        }));
    }

    /// Append a whole batch of messages to the open turn (e.g. one tool round).
    pub fn append_patch(&self, messages: &Value) {
        let Some(items) = messages.as_array() else {
            log::warn!(
                "conversation {}: append_patch expected a JSON array",
                self.memory.inner.conversation_id
            );
            return;
        };
        for message in items {
            self.push_open(message.clone());
        }
    }

    /// Pin a message: a standalone record, never compacted, always rendered
    /// first. Independent of the open turn — persisted on its own.
    pub fn pin(&self, message: Value) {
        let inner = &self.memory.inner;
        let should_persist = {
            let mut state = lock_state(inner);
            let id = next_id(&mut state);
            state.approx_tokens = state
                .approx_tokens
                .saturating_add(estimate_message_tokens(&message));
            enqueue(
                &mut state,
                &LogRecord::Pin {
                    id,
                    msg: message.clone(),
                },
                PendingTarget::Pin(id),
                inner.conversation_id,
            );
            state.pinned.push(StoredPin {
                id,
                value: message,
                loc: None,
            });
            persist_due(inner, &state)
        };
        if should_persist {
            persist(inner, false);
        }
    }

    /// Commit the open turn now as one group record and start a fresh one.
    ///
    /// Called automatically on drop; use it to seal a turn early without leaving
    /// scope. A no-op when the open turn is empty and nothing else is due.
    ///
    /// Order matters: a summary a worker finished earlier is applied *first* (so
    /// the new `compact` record lands before this turn's `group` record), then
    /// the turn is committed, then we (maybe) schedule the next summarization and
    /// persist on this foreground thread.
    pub fn commit(&self) {
        let inner = &self.memory.inner;
        let applied = apply_parked_summary(inner);
        let (should_compact, due) = {
            let mut state = lock_state(inner);
            if !state.open_group.is_empty() {
                let msgs = std::mem::take(&mut state.open_group);
                let id = next_id(&mut state);
                enqueue(
                    &mut state,
                    &LogRecord::Group {
                        id,
                        msgs: msgs.clone(),
                    },
                    PendingTarget::Group(id),
                    inner.conversation_id,
                );
                state.groups.push(StoredGroup { id, msgs, loc: None });
            }
            // Compaction is triggered only by committing a turn, only when the
            // estimate exceeds the budget, and only when no summary is already
            // parked. Single-flight in `schedule_compaction` and the `keep_recent`
            // work-gate in `compute_summary` enforce the rest.
            let should_compact = state.approx_tokens > inner.config.compact_threshold_tokens
                && state.parked_summary.is_none();
            (should_compact, persist_due(inner, &state))
        };
        if should_compact {
            self.memory.schedule_compaction();
        }
        if applied {
            // Applying a summary is significant and creates dead bytes: checkpoint
            // it and reclaim space, ignoring the debounce.
            persist(inner, true);
            maybe_collapse(inner);
        } else if due {
            persist(inner, false);
        }
    }

    /// Buffer one message into the open turn (committed later, as a unit).
    fn push_open(&self, message: Value) {
        let mut state = lock_state(&self.memory.inner);
        state.approx_tokens = state
            .approx_tokens
            .saturating_add(estimate_message_tokens(&message));
        state.open_group.push(message);
    }
}

impl Drop for GroupGuard<'_> {
    fn drop(&mut self) {
        // todo: this commits whatever was buffered even on a panic-unwind, so an
        // aborted turn is still persisted. We deliberately do not special-case
        // `std::thread::panicking()`; revisit if half-turn persistence matters.
        self.commit();
    }
}

impl Deref for GroupGuard<'_> {
    type Target = ConversationMemory;
    fn deref(&self) -> &ConversationMemory {
        self.memory
    }
}

/// Allocate the next monotonic id (shared by groups and pins).
fn next_id(state: &mut MemoryState) -> RecordId {
    let id = state.next_id;
    state.next_id = id.next();
    id
}

/// Serialize `record` to a data line and queue it for the next append.
fn enqueue(
    state: &mut MemoryState,
    record: &LogRecord,
    target: PendingTarget,
    conversation_id: usize,
) {
    match serde_json::to_vec(record) {
        Ok(mut line) => {
            line.push(b'\n');
            state.pending.push(Pending { line, target });
        }
        Err(err) => log::warn!("conversation {conversation_id}: serialize record failed: {err}"),
    }
}

/// Compute a summary on a pool worker — the *only* compaction work the pool
/// does. It reads a snapshot and runs the [`Compactor`]; it does **not** mutate
/// `groups` / `pending` or write files. The result is parked for the foreground
/// to apply (see [`apply_parked_summary`]).
///
/// Strategy: **rolling (fold-forward) summarization** over whole groups. The
/// window is the aged set — every committed group except the newest
/// `keep_recent` (`pinned` and the open turn are never compacted) — with the
/// *existing* summary folded in: `previous_summary ++ flattened aged groups`.
///
/// Cutting on **group boundaries** is what makes this tool-safe: a tool round
/// lives inside one group, so compaction can never split an assistant
/// `tool_calls` from its `tool` results. (A group can still be *internally*
/// unbalanced if a turn was aborted mid-round — that is append discipline, not
/// compaction's concern.) The covered range `[id_start, id_end]` is stable
/// regardless of turns committed after this snapshot, because ids are monotonic.
fn compute_summary(inner: &MemoryInner) {
    let keep = inner.config.keep_recent;

    // Snapshot the window to summarize (brief lock; no mutation).
    let (window, id_start, id_end) = {
        let state = lock_state(inner);
        // Work-gate: only summarize when groups have aged out past `keep_recent`.
        if state.groups.len() <= keep {
            return;
        }
        let aged_count = state.groups.len().saturating_sub(keep);
        let aged = state.groups.get(..aged_count).unwrap_or(&[]);
        let (Some(first), Some(last)) = (aged.first(), aged.last()) else {
            return;
        };
        // Fold-forward: the summary keeps covering everything from the original
        // start, so its range start stays put across rounds.
        let id_start = state.summary_range.map(|(a, _)| a).unwrap_or(first.id);
        let id_end = last.id;
        let mut window = Vec::new();
        window.extend(state.summary.iter().cloned());
        for group in aged {
            window.extend(group.msgs.iter().cloned());
        }
        (window, id_start, id_end)
    };

    // Summarize with the lock released (may hit the network).
    let summary = match inner.deps.compactor.compact(&window) {
        Ok(summary) => summary,
        Err(err) => {
            log::warn!(
                "conversation {}: compaction skipped: {err}",
                inner.conversation_id
            );
            return;
        }
    };

    // Park the result; the foreground applies it at the next turn boundary.
    lock_state(inner).parked_summary = Some(CompactionResult {
        id_start,
        id_end,
        summary,
    });
}

/// Apply a parked summary on the foreground: retire the covered groups, append a
/// `compact` record, fold the summary in. Returns whether anything was applied.
///
/// Application is by id range, so it is correct even though the foreground may
/// have committed more turns since the worker took its snapshot (those have
/// higher ids and are untouched). Covered groups that are still unflushed are
/// dropped from `pending` (their content is in the summary, so they must never
/// be written); covered groups already in the data log stay there as dead bytes
/// before the new `compact` record — the replay order that keeps supersession
/// correct.
fn apply_parked_summary(inner: &MemoryInner) -> bool {
    let mut state = lock_state(inner);
    let Some(result) = state.parked_summary.take() else {
        return false;
    };
    let (id_start, id_end) = (result.id_start, result.id_end);

    state.groups.retain(|g| g.id < id_start || g.id > id_end);
    state.pending.retain(|p| match p.target {
        PendingTarget::Group(id) => id < id_start || id > id_end,
        _ => true,
    });
    state.summary = result.summary;
    state.summary_range = Some((id_start, id_end));
    state.summary_loc = None;
    let record = LogRecord::Compact {
        id_start,
        id_end,
        summary: state.summary.clone(),
    };
    enqueue(&mut state, &record, PendingTarget::Compact, inner.conversation_id);
    state.approx_tokens = estimate_state_tokens(&state);
    // Dead records now exist; the manifest must be refreshed to exclude them.
    state.index_dirty = true;
    true
}

/// Flush pending records (one `append`) and, when needed, rewrite the manifest.
///
/// Foreground-only: the single `state` lock is held across the filesystem writes,
/// so offsets, appends, and manifest stay consistent with no second lock and no
/// release-and-reacquire window. Data is always appended *before* the manifest
/// is updated, so the manifest's `covered_len` never points past the end of the
/// data log. On a failed data append the pending records are left in place to
/// retry on the next persist — nothing is committed.
fn persist(inner: &MemoryInner, force_manifest: bool) {
    let mut state = lock_state(inner);
    let want_manifest = force_manifest || state.index_dirty;
    if state.pending.is_empty() && !want_manifest {
        return;
    }

    // Append the pending data lines, then record each record's byte location.
    if !state.pending.is_empty() {
        let mut data_buf = Vec::new();
        let mut locs = Vec::with_capacity(state.pending.len());
        let mut off = state.data_len.as_offset();
        for pending in &state.pending {
            let len = ByteLen::of(&pending.line);
            data_buf.extend_from_slice(&pending.line);
            locs.push((pending.target, off, len));
            off = off.advance(len);
        }
        if let Err(err) = inner.deps.fs.append(&inner.data_path, &data_buf) {
            log::warn!(
                "conversation {}: data append failed: {err}",
                inner.conversation_id
            );
            return; // keep pending; retry next persist
        }
        state.data_len = off.as_len();
        for (target, off, len) in locs {
            set_loc(&mut state, target, off, len);
        }
        state.pending.clear();
    }
    state.last_persist = Some(Instant::now());

    // Manifest second, so it never describes more than the data log holds.
    if want_manifest {
        if let Some(bytes) = build_manifest_bytes(&state, inner.conversation_id) {
            match inner.deps.fs.write_atomic(&inner.index_path, &bytes) {
                Ok(()) => {
                    state.manifest_covered_len = state.data_len;
                    state.index_dirty = false;
                }
                Err(err) => {
                    log::warn!(
                        "conversation {}: index write failed: {err}",
                        inner.conversation_id
                    );
                    state.index_dirty = true; // rebuilt on next persist or load
                }
            }
        }
    }
}

/// Rewrite both files from the live set when dead bytes dominate, reclaiming the
/// space left by superseded records. Foreground-only, like [`persist`].
fn maybe_collapse(inner: &MemoryInner) {
    let collapse = {
        let state = lock_state(inner);
        let live = live_bytes(&state);
        state.data_len > COLLAPSE_FLOOR_BYTES && state.data_len > live.saturating_mul(2)
    };
    if collapse {
        collapse_locked(inner);
    }
}

/// Rewrite `.jsonl` + `.json` from the in-memory live set.
///
/// Rare and small (the live set is `pinned + one summary + keep_recent` groups),
/// so this holds the single state lock across the writes for simplicity.
fn collapse_locked(inner: &MemoryInner) {
    let mut state = lock_state(inner);

    let mut data_buf = Vec::new();
    let mut live = Vec::new();
    let mut locs: Vec<(PendingTarget, ByteOffset, ByteLen)> = Vec::new();
    let mut off = ByteOffset::default();

    // Order: pins, then the summary, then groups — all in id order.
    for pin in &state.pinned {
        let record = LogRecord::Pin {
            id: pin.id,
            msg: pin.value.clone(),
        };
        let Some(len) = append_line(&mut data_buf, &record, inner.conversation_id) else {
            return;
        };
        live.push(IndexEntry::Pin {
            off,
            len,
            id: pin.id,
        });
        locs.push((PendingTarget::Pin(pin.id), off, len));
        off = off.advance(len);
    }
    if let (false, Some((id_start, id_end))) = (state.summary.is_empty(), state.summary_range) {
        let record = LogRecord::Compact {
            id_start,
            id_end,
            summary: state.summary.clone(),
        };
        let Some(len) = append_line(&mut data_buf, &record, inner.conversation_id) else {
            return;
        };
        live.push(IndexEntry::Compact {
            off,
            len,
            id_start,
            id_end,
        });
        locs.push((PendingTarget::Compact, off, len));
        off = off.advance(len);
    }
    for group in &state.groups {
        let record = LogRecord::Group {
            id: group.id,
            msgs: group.msgs.clone(),
        };
        let Some(len) = append_line(&mut data_buf, &record, inner.conversation_id) else {
            return;
        };
        live.push(IndexEntry::Group {
            off,
            len,
            id: group.id,
        });
        locs.push((PendingTarget::Group(group.id), off, len));
        off = off.advance(len);
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        covered_len: off.as_len(),
        next_id: state.next_id,
        live,
    };
    let manifest_bytes = match serde_json::to_vec(&manifest) {
        Ok(bytes) => bytes,
        Err(err) => {
            log::warn!(
                "conversation {}: collapse manifest serialize failed: {err}",
                inner.conversation_id
            );
            return;
        }
    };

    if let Err(err) = inner.deps.fs.write_atomic(&inner.data_path, &data_buf) {
        log::warn!(
            "conversation {}: collapse data write failed: {err}",
            inner.conversation_id
        );
        return;
    }
    if let Err(err) = inner.deps.fs.write_atomic(&inner.index_path, &manifest_bytes) {
        log::warn!(
            "conversation {}: collapse index write failed: {err}",
            inner.conversation_id
        );
        // The data file is the fresh truth; the stale manifest is rebuilt on load.
    }

    // Commit: every live record now lives at its fresh offset; pending lines were
    // duplicates of in-memory records and are subsumed by the rewrite.
    state.pending.clear();
    state.data_len = off.as_len();
    state.manifest_covered_len = off.as_len();
    state.index_dirty = false;
    state.summary_loc = None;
    for pin in &mut state.pinned {
        pin.loc = None;
    }
    for group in &mut state.groups {
        group.loc = None;
    }
    for (target, off, len) in locs {
        set_loc(&mut state, target, off, len);
    }
}

/// Serialize `record` into `buf` with a trailing newline; returns the line length.
fn append_line(buf: &mut Vec<u8>, record: &LogRecord, conversation_id: usize) -> Option<ByteLen> {
    match serde_json::to_vec(record) {
        Ok(mut line) => {
            line.push(b'\n');
            let len = ByteLen::of(&line);
            buf.extend_from_slice(&line);
            Some(len)
        }
        Err(err) => {
            log::warn!("conversation {conversation_id}: serialize record failed: {err}");
            None
        }
    }
}

/// Record a flushed record's byte location back onto its in-memory entry.
fn set_loc(state: &mut MemoryState, target: PendingTarget, off: ByteOffset, len: ByteLen) {
    match target {
        PendingTarget::Group(id) => {
            if let Some(group) = state.groups.iter_mut().find(|g| g.id == id) {
                group.loc = Some((off, len));
            }
        }
        PendingTarget::Pin(id) => {
            if let Some(pin) = state.pinned.iter_mut().find(|p| p.id == id) {
                pin.loc = Some((off, len));
            }
        }
        PendingTarget::Compact => state.summary_loc = Some((off, len)),
    }
}

/// Build the manifest of the current live records (those already on disk).
fn build_manifest_bytes(state: &MemoryState, conversation_id: usize) -> Option<Vec<u8>> {
    let mut live = Vec::new();
    for pin in &state.pinned {
        if let Some((off, len)) = pin.loc {
            live.push(IndexEntry::Pin {
                off,
                len,
                id: pin.id,
            });
        }
    }
    if let (false, Some((off, len)), Some((id_start, id_end))) = (
        state.summary.is_empty(),
        state.summary_loc,
        state.summary_range,
    ) {
        live.push(IndexEntry::Compact {
            off,
            len,
            id_start,
            id_end,
        });
    }
    for group in &state.groups {
        if let Some((off, len)) = group.loc {
            live.push(IndexEntry::Group {
                off,
                len,
                id: group.id,
            });
        }
    }
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        covered_len: state.data_len,
        next_id: state.next_id,
        live,
    };
    match serde_json::to_vec(&manifest) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            log::warn!("conversation {conversation_id}: manifest serialize failed: {err}");
            None
        }
    }
}

/// Total bytes of records currently considered live and present on disk.
fn live_bytes(state: &MemoryState) -> ByteLen {
    let mut total = ByteLen::default();
    for pin in &state.pinned {
        if let Some((_, len)) = pin.loc {
            total = total.saturating_add(len);
        }
    }
    if !state.summary.is_empty() {
        if let Some((_, len)) = state.summary_loc {
            total = total.saturating_add(len);
        }
    }
    for group in &state.groups {
        if let Some((_, len)) = group.loc {
            total = total.saturating_add(len);
        }
    }
    total
}

/// Whether a memory has unwritten work past its debounce window.
fn persist_due(inner: &MemoryInner, state: &MemoryState) -> bool {
    if state.pending.is_empty() && !state.index_dirty {
        return false;
    }
    match state.last_persist {
        None => true,
        Some(at) => at.elapsed() >= inner.config.persist_debounce,
    }
}

/// Load and rehydrate persisted state: manifest live records by `read_at`, then a
/// tail-scan for records appended since the manifest was written. A missing or
/// short manifest degrades to a full data-log scan.
fn load_state(fs: &dyn ClawFs, data_path: &str, index_path: &str) -> MemoryState {
    let mut state = MemoryState::default();
    let mut covered_len = ByteLen::default();
    let mut manifest_next_id = RecordId::default();

    if let Ok(bytes) = fs.read(index_path) {
        if let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) {
            covered_len = manifest.covered_len;
            manifest_next_id = manifest.next_id;
            for entry in &manifest.live {
                let (off, len) = entry_loc(entry);
                match fs.read_at(data_path, off.as_u64(), len.as_usize()) {
                    Ok(buf) => {
                        if let Some(record) = parse_record(&buf) {
                            apply_record(&mut state, record, Some((off, len)));
                        }
                    }
                    Err(err) => log::warn!(
                        "conversation load: read_at offset {} failed: {err}",
                        off.as_u64()
                    ),
                }
            }
        }
    }

    let data_len = ByteLen::from_file_len(fs.len(data_path).unwrap_or(0));
    if data_len > covered_len {
        let extra = data_len.saturating_sub(covered_len);
        if let Ok(tail) = fs.read_at(data_path, covered_len.as_offset().as_u64(), extra.as_usize())
        {
            scan_tail(&mut state, &tail, covered_len.as_offset());
        }
        // The manifest no longer reflects the live/dead structure; refresh it on
        // the next persist so future loads can skip dead records again.
        state.index_dirty = true;
    }

    state.data_len = data_len;
    state.manifest_covered_len = covered_len;
    state.pinned.sort_by_key(|p| p.id);
    state.groups.sort_by_key(|g| g.id);
    state.next_id = manifest_next_id.max(max_seen_id(&state).next());
    state.approx_tokens = estimate_state_tokens(&state);
    state
}

/// Parse a newline-delimited tail buffer, applying each complete record. A torn
/// trailing line (no newline) is discarded.
fn scan_tail(state: &mut MemoryState, tail: &[u8], base_off: ByteOffset) {
    let mut start = 0usize;
    let mut pos = base_off;
    for (i, byte) in tail.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = tail.get(start..i).unwrap_or(&[]);
        let line_len = ByteLen(i.saturating_sub(start).saturating_add(1));
        if let Some(record) = parse_record(line) {
            apply_record(state, record, Some((pos, line_len)));
        }
        pos = pos.advance(line_len);
        start = i.saturating_add(1);
    }
}

/// Fold one decoded record into the live state, applying supersession.
///
/// Supersession is keyed on [`RecordId`] only; `loc` is the byte address of the
/// record and is never used to decide order. Keep it that way — making this
/// positional would make correctness depend on physical offset order.
fn apply_record(state: &mut MemoryState, record: LogRecord, loc: Option<(ByteOffset, ByteLen)>) {
    match record {
        LogRecord::Group { id, msgs } => state.groups.push(StoredGroup { id, msgs, loc }),
        LogRecord::Pin { id, msg } => state.pinned.push(StoredPin {
            id,
            value: msg,
            loc,
        }),
        LogRecord::Compact {
            id_start,
            id_end,
            summary,
        } => {
            // The summary supersedes groups in its range; pins are never in it.
            state.groups.retain(|g| g.id < id_start || g.id > id_end);
            state.summary = summary;
            state.summary_range = Some((id_start, id_end));
            state.summary_loc = loc;
        }
    }
}

/// Highest id currently represented (live group/pin or summary range end).
fn max_seen_id(state: &MemoryState) -> RecordId {
    let mut max = RecordId::default();
    for pin in &state.pinned {
        max = max.max(pin.id);
    }
    for group in &state.groups {
        max = max.max(group.id);
    }
    if let Some((_, id_end)) = state.summary_range {
        max = max.max(id_end);
    }
    max
}

fn entry_loc(entry: &IndexEntry) -> (ByteOffset, ByteLen) {
    match *entry {
        IndexEntry::Group { off, len, .. }
        | IndexEntry::Pin { off, len, .. }
        | IndexEntry::Compact { off, len, .. } => (off, len),
    }
}

fn parse_record(bytes: &[u8]) -> Option<LogRecord> {
    if bytes.is_empty() {
        return None;
    }
    match serde_json::from_slice::<LogRecord>(bytes) {
        Ok(record) => Some(record),
        Err(err) => {
            log::warn!("conversation load: skipping unparseable record: {err}");
            None
        }
    }
}

/// Build a per-conversation path from the base dir, id, and extension.
fn conversation_path(dir: &str, conversation_id: usize, ext: &str) -> String {
    format!(
        "{}/{FILE_PREFIX}{conversation_id}{ext}",
        dir.trim_end_matches('/')
    )
}

/// Lock the state, recovering the guard if a worker panicked while holding it.
fn lock_state(inner: &MemoryInner) -> MutexGuard<'_, MemoryState> {
    inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// todo: replace this byte-length heuristic with a real tokenizer estimate that
// matches the active backend's accounting. It only needs to be monotonic and
// roughly proportional for the compaction trigger to behave.
fn estimate_message_tokens(message: &Value) -> usize {
    message.to_string().len() / CHARS_PER_TOKEN + 1
}

fn estimate_state_tokens(state: &MemoryState) -> usize {
    let pinned: usize = state
        .pinned
        .iter()
        .map(|p| estimate_message_tokens(&p.value))
        .sum();
    let summary: usize = state.summary.iter().map(estimate_message_tokens).sum();
    let groups: usize = state
        .groups
        .iter()
        .flat_map(|g| g.msgs.iter())
        .map(estimate_message_tokens)
        .sum();
    let open: usize = state.open_group.iter().map(estimate_message_tokens).sum();
    pinned
        .saturating_add(summary)
        .saturating_add(groups)
        .saturating_add(open)
}
