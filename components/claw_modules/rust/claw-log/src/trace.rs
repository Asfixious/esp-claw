//! A minimal, platform-agnostic `tracing` [`Subscriber`] that flattens the
//! *current thread's* span stack onto every record and writes one line per
//! event / span-boundary to an injected [`TraceSink`].
//!
//! This is pure Rust (only `tracing`'s re-exported core traits): no
//! `tracing-subscriber` (so no `sharded-slab`/`regex`), and no platform/FFI. The
//! actual output target is a [`TraceSink`] supplied by the caller — this crate's
//! [`init_tracing`](crate::init_tracing) wires it to `claw_sys`'s `ESP_LOGx`
//! sink; tests inject a capturing one.
//!
//! ## What it solves
//!
//! The `log` bridge (`tracing/log-always`) drops span context: a child event
//! prints none of its ancestors' ids, and there is no stable per-span instance
//! id, so a flat log cannot be reassembled into a tree — and concurrent sessions
//! interleave irrecoverably. This subscriber fixes both **without** plumbing ids
//! through every layer:
//!
//! - Span context propagates automatically via the thread-local stack (the whole
//!   point of `tracing` spans), so call sites pass no extra arguments.
//! - Each line carries a `task=` label (the OS thread / FreeRTOS task), so
//!   concurrent drives stay separable.
//! - Span lifetimes are emitted as `>>` (enter) / `<<` (exit) lines keyed by a
//!   process-unique `sp=` id with a `parent=` edge, so an offline tool rebuilds
//!   the tree from `(sp, parent)` and computes per-span timing by differencing
//!   the sink's timestamps for the matching `>>`/`<<` lines.
//!
//! ## Line grammar (for the offline visualizer)
//!
//! All lines start with `TR ` and a kind token, then space-separated `key=value`
//! tokens (values never contain spaces); events append a free-text message after
//! ` | `:
//!
//! ```text
//! TR >> sp=<id> parent=<id|0> task=<label> name=<span-name> tgt=<target> <field=val,...>
//! TR << sp=<id> task=<label>
//! TR ev sp=<id|0> task=<label> tgt=<target> <field=val,...> | <message>
//! ```
//!
//! `sp=0` on an event means it fired outside any span. `<field=val,...>` is a
//! single comma-joined token (no spaces).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// `tracing` re-exports the core traits/types this subscriber needs, so no
// separate `tracing-core` dependency is required.
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Where a [`FlatTreeSubscriber`] writes its formatted lines. Implemented by the
/// platform sink wiring (this crate's `ClawTraceSink`, which writes through
/// `claw_sys`'s `ESP_LOGx` sink) and by tests (a capturing sink), keeping this
/// module free of any output/FFI concern.
pub trait TraceSink: Send + Sync {
    /// Write one already-formatted, single-line record at `level`, tagged `tag`
    /// (the record's `target`/module path). The line never contains a newline.
    fn write_line(&self, level: Level, tag: &str, line: &str);
}

/// Immutable per-span data captured at creation and reused on enter/exit.
struct SpanRecord {
    /// The span's declared name (`info_span!("agent", …)` → `"agent"`).
    name: &'static str,
    /// The span's `target` (module path), used as the sink tag.
    target: &'static str,
    /// The span's level (forwarded to the sink for each lifecycle line).
    level: Level,
    /// Rendered `"k=v,k=v"` of the span's fields (no spaces; offline-parseable).
    fields: String,
}

/// Refcounted slot in the global span map (`tracing` may `clone_span`).
struct SpanSlot {
    record: Arc<SpanRecord>,
    refs: u64,
}

thread_local! {
    /// The active span stack for *this* thread: `(id, record)` pushed on enter,
    /// popped on exit. The hot event path only reads the top, so it never locks
    /// the global map.
    static STACK: RefCell<Vec<(u64, Arc<SpanRecord>)>> = const { RefCell::new(Vec::new()) };

    /// This thread's label, computed once (thread name, else `ThreadId(n)`).
    static TASK: String = task_label();
}

fn task_label() -> String {
    let current = std::thread::current();
    match current.name() {
        Some(name) => name.to_string(),
        // `ThreadId`'s Debug is `ThreadId(n)` — no spaces, safe as a token.
        None => format!("{:?}", current.id()),
    }
}

/// Collects fields into a comma-joined `k=v` token and captures the special
/// `message` field separately (events carry their text there).
struct FieldVisitor {
    fields: String,
    message: Option<String>,
}

impl FieldVisitor {
    fn new() -> Self {
        Self {
            fields: String::new(),
            message: None,
        }
    }

    fn push(&mut self, name: &str, value: fmt::Arguments<'_>) {
        if name == "message" {
            self.message = Some(format!("{value}"));
            return;
        }
        if !self.fields.is_empty() {
            self.fields.push(',');
        }
        self.fields.push_str(name);
        self.fields.push('=');
        let _ = write!(self.fields, "{value}");
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field.name(), format_args!("{value}"));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field.name(), format_args!("{value}"));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field.name(), format_args!("{value}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field.name(), format_args!("{value}"));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field.name(), format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%x` (Display) and `?x` (Debug) span/event values land here.
        self.push(field.name(), format_args!("{value:?}"));
    }
}

/// Replace characters that would split or confuse a single log line.
fn sanitize(input: &str) -> String {
    input.replace(['\n', '\r'], " ")
}

/// A `tracing` subscriber that emits the flat-tree line grammar to `sink`.
///
/// Generic over the sink so it stays unit-testable (inject a capturing sink) and
/// platform-agnostic (the caller supplies `ESP_LOGx`/stderr/UART/file output).
pub struct FlatTreeSubscriber<S: TraceSink> {
    next_id: AtomicU64,
    spans: Mutex<HashMap<u64, SpanSlot>>,
    sink: S,
}

impl<S: TraceSink> FlatTreeSubscriber<S> {
    /// Build a subscriber writing to `sink`.
    pub fn with_sink(sink: S) -> Self {
        Self {
            // `span::Id` must be non-zero; start at 1 and reserve 0 for "no span".
            next_id: AtomicU64::new(1),
            spans: Mutex::new(HashMap::new()),
            sink,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, SpanSlot>> {
        self.spans
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write(&self, level: Level, tag: &str, line: &str) {
        self.sink.write_line(level, tag, &sanitize(line));
    }
}

impl<S: TraceSink + 'static> Subscriber for FlatTreeSubscriber<S> {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        // Compile-time filtering (the `tracing` `max_level_*` features) and the
        // runtime sink ceiling already gate output; accept everything here.
        true
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let metadata = attributes.metadata();
        let mut visitor = FieldVisitor::new();
        attributes.record(&mut visitor);

        let record = Arc::new(SpanRecord {
            name: metadata.name(),
            target: metadata.target(),
            level: *metadata.level(),
            fields: visitor.fields,
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, SpanSlot { record, refs: 1 });
        Id::from_u64(id)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        // `Span::record` after creation: merge late fields into the stored set.
        let mut visitor = FieldVisitor::new();
        values.record(&mut visitor);
        if visitor.fields.is_empty() {
            return;
        }
        let mut map = self.lock();
        if let Some(slot) = map.get_mut(&span.into_u64()) {
            // The record is shared (cloned onto the stack); rebuild it with the
            // extra fields appended so future lines see them.
            let mut fields = slot.record.fields.clone();
            if !fields.is_empty() {
                fields.push(',');
            }
            fields.push_str(&visitor.fields);
            slot.record = Arc::new(SpanRecord {
                name: slot.record.name,
                target: slot.record.target,
                level: slot.record.level,
                fields,
            });
        }
    }

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::new();
        event.record(&mut visitor);

        let span_id = STACK.with(|stack| stack.borrow().last().map(|(id, _)| *id).unwrap_or(0));
        let message = visitor.message.unwrap_or_default();

        let line = TASK.with(|task| {
            format!(
                "TR ev sp={span_id} task={task} tgt={target} {fields} | {message}",
                target = metadata.target(),
                fields = visitor.fields,
            )
        });
        self.write(*metadata.level(), metadata.target(), &line);
    }

    fn enter(&self, span: &Id) {
        let id = span.into_u64();
        let record = self.lock().get(&id).map(|slot| slot.record.clone());
        let Some(record) = record else {
            return;
        };

        let parent = STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            let parent = stack.last().map(|(id, _)| *id).unwrap_or(0);
            stack.push((id, record.clone()));
            parent
        });

        let line = TASK.with(|task| {
            format!(
                "TR >> sp={id} parent={parent} task={task} name={name} tgt={target} {fields}",
                name = record.name,
                target = record.target,
                fields = record.fields,
            )
        });
        self.write(record.level, record.target, &line);
    }

    fn exit(&self, span: &Id) {
        let id = span.into_u64();
        let popped = STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            // Spans are entered/exited LIFO per thread (RAII guards), so the top
            // is this span; pop it. Guard against a mismatch just in case.
            match stack.last() {
                Some((top, _)) if *top == id => stack.pop(),
                _ => None,
            }
        });
        if let Some((_, record)) = popped {
            let line = TASK.with(|task| format!("TR << sp={id} task={task}"));
            self.write(record.level, record.target, &line);
        }
    }

    fn clone_span(&self, id: &Id) -> Id {
        if let Some(slot) = self.lock().get_mut(&id.into_u64()) {
            slot.refs = slot.refs.saturating_add(1);
        }
        id.clone()
    }

    fn try_close(&self, id: Id) -> bool {
        let mut map = self.lock();
        let raw = id.into_u64();
        let Some(slot) = map.get_mut(&raw) else {
            return false;
        };
        slot.refs = slot.refs.saturating_sub(1);
        if slot.refs == 0 {
            map.remove(&raw);
            return true;
        }
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A sink that records every `(level, tag, line)` for assertions.
    #[derive(Clone, Default)]
    struct VecSink(Arc<Mutex<Vec<(Level, String, String)>>>);

    impl VecSink {
        fn lines(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(_, _, line)| line.clone())
                .collect()
        }
    }

    impl TraceSink for VecSink {
        fn write_line(&self, level: Level, tag: &str, line: &str) {
            self.0
                .lock()
                .unwrap()
                .push((level, tag.to_string(), line.to_string()));
        }
    }

    /// Extract the value of `key=` from a space-tokenized `TR …` line.
    fn token<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        line.split(' ')
            .find_map(|tok| tok.strip_prefix(key)?.strip_prefix('='))
    }

    #[test]
    fn sanitize_collapses_newlines() {
        assert_eq!(sanitize("a\nb\rc"), "a b c");
    }

    #[test]
    fn field_visitor_separates_message_from_fields() {
        let mut visitor = FieldVisitor::new();
        visitor.push("a", format_args!("1"));
        visitor.push("message", format_args!("hello world"));
        visitor.push("b", format_args!("2"));
        assert_eq!(visitor.fields, "a=1,b=2");
        assert_eq!(visitor.message.as_deref(), Some("hello world"));
    }

    #[test]
    fn nested_spans_and_event_carry_ids_and_parent_edges() {
        let sink = VecSink::default();
        let subscriber = FlatTreeSubscriber::with_sink(sink.clone());

        tracing::subscriber::with_default(subscriber, || {
            let turn = tracing::info_span!("turn", session = "s-1", turn = 7u64);
            let _turn = turn.enter();
            tracing::info!("root thinking");
            {
                let agent = tracing::info_span!("agent", agent = "a-2", depth = 1u64);
                let _agent = agent.enter();
                tracing::info!(tool = "files", "calling tool");
            }
        });

        let lines = sink.lines();

        let turn_enter = lines
            .iter()
            .find(|l| l.starts_with("TR >>") && token(l, "name") == Some("turn"))
            .expect("turn enter line");
        assert_eq!(token(turn_enter, "parent"), Some("0"));
        assert!(turn_enter.contains("session=s-1"));
        assert!(turn_enter.contains("turn=7"));
        let turn_id = token(turn_enter, "sp").expect("turn sp");

        let agent_enter = lines
            .iter()
            .find(|l| l.starts_with("TR >>") && token(l, "name") == Some("agent"))
            .expect("agent enter line");
        // The agent span nests under the turn span.
        assert_eq!(token(agent_enter, "parent"), Some(turn_id));
        let agent_id = token(agent_enter, "sp").expect("agent sp");

        // The root event fired while `turn` was current.
        let root_event = lines
            .iter()
            .find(|l| l.starts_with("TR ev") && l.ends_with("root thinking"))
            .expect("root event");
        assert_eq!(token(root_event, "sp"), Some(turn_id));

        // The nested event fired while `agent` was current and kept its field.
        let tool_event = lines
            .iter()
            .find(|l| l.starts_with("TR ev") && l.contains("calling tool"))
            .expect("tool event");
        assert_eq!(token(tool_event, "sp"), Some(agent_id));
        assert!(tool_event.contains("tool=files"));

        // Exits are emitted (so an offline tool can time each span).
        assert!(lines
            .iter()
            .any(|l| l.starts_with("TR <<") && token(l, "sp") == Some(agent_id)));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("TR <<") && token(l, "sp") == Some(turn_id)));
    }

    #[test]
    fn event_outside_any_span_reports_sp_zero() {
        let sink = VecSink::default();
        let subscriber = FlatTreeSubscriber::with_sink(sink.clone());

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("no span here");
        });

        let event = sink
            .lines()
            .into_iter()
            .find(|l| l.starts_with("TR ev"))
            .expect("event line");
        assert_eq!(token(&event, "sp"), Some("0"));
    }
}
