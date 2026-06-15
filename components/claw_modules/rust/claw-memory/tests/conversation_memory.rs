//! Integration tests for [`ConversationMemory`], exercising the public API the
//! way an agent would: append turns, read the messages, persist/reload, and let
//! background auto-compaction run on the shared pool.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use claw_interfaces::{ClawFs, FsError};
use claw_memory::{
    CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
    MemoryTaskPool, PoolConfig,
};
use serde_json::{json, Value};

// --- test doubles --------------------------------------------------------

/// In-memory [`ClawFs`] so tests stay hermetic.
#[derive(Default)]
struct MemFs {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl ClawFs for MemFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        self.lock().get(path).cloned().ok_or(FsError::NotFound)
    }
    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let files = self.lock();
        let bytes = files.get(path).ok_or(FsError::NotFound)?;
        let start = offset as usize;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| FsError::Io("read_at past end of file".into()))?;
        Ok(bytes[start..end].to_vec())
    }
    fn len(&self, path: &str) -> Result<u64, FsError> {
        self.lock()
            .get(path)
            .map(|b| b.len() as u64)
            .ok_or(FsError::NotFound)
    }
    fn write_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.lock().insert(path.to_string(), data.to_vec());
        Ok(())
    }
    fn append(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.lock()
            .entry(path.to_string())
            .or_default()
            .extend_from_slice(data);
        Ok(())
    }
    fn exists(&self, path: &str) -> bool {
        self.lock().contains_key(path)
    }
    fn remove(&self, path: &str) -> Result<(), FsError> {
        self.lock().remove(path);
        Ok(())
    }
}

impl MemFs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.files.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A [`Compactor`] that records how many times it ran and returns a fixed
/// one-message summary, so tests can observe the background compaction.
struct StubCompactor {
    calls: AtomicUsize,
    marker: String,
}

impl StubCompactor {
    fn new(marker: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            marker: marker.to_string(),
        }
    }
}

impl Compactor for StubCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![json!({ "role": "system", "content": self.marker })])
    }
}

// --- helpers -------------------------------------------------------------

fn pool() -> Arc<MemoryTaskPool> {
    Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("spawn pool"))
}

fn deps(fs: Arc<dyn ClawFs>, pool: Arc<MemoryTaskPool>, marker: &str) -> ConversationDeps {
    ConversationDeps {
        fs,
        pool,
        compactor: Arc::new(StubCompactor::new(marker)),
    }
}

/// Config with an instant persist (no debounce) so appends hit disk per turn.
fn instant_config(dir: &str, threshold: usize, keep_recent: usize) -> ConversationConfig {
    let mut config = ConversationConfig::new(dir);
    config.compact_threshold_tokens = threshold;
    config.keep_recent = keep_recent;
    config.persist_debounce = Duration::ZERO;
    config
}

/// A memory with default tuning, conversation id 1, and fresh doubles.
fn memory_with(fs: Arc<dyn ClawFs>) -> ConversationMemory {
    ConversationMemory::new(
        1,
        ConversationConfig::new("/conversations"),
        ConversationDeps {
            fs,
            pool: pool(),
            compactor: Arc::new(StubCompactor::new("S")),
        },
    )
}

/// Poll `predicate` until it holds or a generous deadline passes (background
/// jobs run on the pool, so observable state settles asynchronously).
fn wait_until(predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

fn messages(memory: &ConversationMemory) -> Vec<Value> {
    memory
        .messages()
        .as_array()
        .cloned()
        .expect("messages() returns an array")
}

/// Drive turn boundaries until `predicate` holds or a deadline passes.
///
/// Compaction is computed in the background and only *applied* on the next turn
/// boundary, so a real agent reaches the compacted state by continuing to commit
/// turns (or flushing). An empty `group()` commit is exactly such a boundary: it
/// applies any finished summary and reschedules the next one if still over
/// budget. This mirrors that loop deterministically in tests.
fn pump_until(
    memory: &mut ConversationMemory,
    predicate: impl Fn(&ConversationMemory) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        drop(memory.group()); // apply a finished summary / reschedule
        if predicate(memory) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate(memory)
}

// --- tests ---------------------------------------------------------------

#[test]
fn appends_render_in_order() {
    let mut memory = memory_with(Arc::new(MemFs::default()));

    {
        let turn = memory.group();
        turn.append_user("hello");
        turn.append_assistant(r#"{"role":"assistant","content":"hi"}"#);
        turn.append_tool_result("call_1", "42", false);
    }

    let messages = messages(&memory);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hello");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "hi");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_1");
    assert_eq!(messages[2]["content"], "42");
    assert_eq!(messages[2]["is_error"], false);
}

#[test]
fn pinned_messages_render_first() {
    let mut memory = memory_with(Arc::new(MemFs::default()));

    {
        let turn = memory.group();
        turn.append_user("body");
        turn.pin(json!({ "role": "system", "content": "task brief" }));
    }

    let messages = messages(&memory);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], "task brief"); // pinned, ahead of the turn
    assert_eq!(messages[1]["content"], "body");
}

#[test]
fn append_patch_expands_a_batch() {
    let mut memory = memory_with(Arc::new(MemFs::default()));

    let batch = json!([
        { "role": "assistant", "content": "calling tool" },
        { "role": "tool", "tool_call_id": "c1", "content": "ok" },
    ]);
    {
        let turn = memory.group();
        turn.append_patch(&batch);
    }

    let messages = messages(&memory);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[1]["tool_call_id"], "c1");
}

#[test]
fn distinct_ids_persist_independently() {
    let fs = Arc::new(MemFs::default());
    let shared_pool = pool();
    let dir = "/sessions";

    let make = |id: usize| {
        ConversationMemory::new(
            id,
            ConversationConfig::new(dir),
            ConversationDeps {
                fs: Arc::clone(&fs) as Arc<dyn ClawFs>,
                pool: Arc::clone(&shared_pool),
                compactor: Arc::new(StubCompactor::new("S")),
            },
        )
    };

    let mut one = make(1);
    let mut two = make(2);
    one.group().append_user("from one");
    two.group().append_user("from two");
    one.flush();
    two.flush();

    // Each id wrote its own data log (and, after flush, its index manifest).
    assert!(fs.exists("/sessions/conversation-1.jsonl"));
    assert!(fs.exists("/sessions/conversation-2.jsonl"));
    assert!(fs.exists("/sessions/conversation-1.json"));
    assert!(fs.exists("/sessions/conversation-2.json"));

    // Reloading by id restores only that conversation's transcript.
    let reloaded_one = make(1);
    assert_eq!(messages(&reloaded_one).len(), 1);
    assert_eq!(messages(&reloaded_one)[0]["content"], "from one");
}

#[test]
fn missing_persist_file_starts_empty() {
    let memory = ConversationMemory::new(
        7,
        ConversationConfig::new("/empty"),
        ConversationDeps {
            fs: Arc::new(MemFs::default()),
            pool: pool(),
            compactor: Arc::new(StubCompactor::new("S")),
        },
    );
    assert!(messages(&memory).is_empty());
}

#[test]
fn crosses_threshold_and_auto_compacts_in_background() {
    let compactor = Arc::new(StubCompactor::new("SUMMARY"));

    let mut config = ConversationConfig::new("/conversations");
    config.compact_threshold_tokens = 5; // tiny, so a few turns trip it
    config.keep_recent = 2; // keep the 2 newest raw turns verbatim

    let mut memory = ConversationMemory::new(
        1,
        config,
        ConversationDeps {
            fs: Arc::new(MemFs::default()),
            pool: pool(),
            compactor: Arc::clone(&compactor) as Arc<dyn Compactor>,
        },
    );

    // Each message is its own turn, so they age out as distinct groups.
    for i in 0..6 {
        memory.group().append_user(format!("message number {i}"));
    }

    // The summary is computed on a pool worker and applied at a later turn
    // boundary, so drive boundaries until it settles: summary + the `keep_recent`
    // newest verbatim turns.
    assert!(
        pump_until(&mut memory, |m| {
            let messages = messages(m);
            messages.len() == 3 && messages[0]["content"] == "SUMMARY"
        }),
        "expected summary + 2 recent messages, got {:?}",
        messages(&memory),
    );

    // The Compactor ran on the pool, off the append path.
    assert!(compactor.calls.load(Ordering::SeqCst) >= 1);

    let messages = messages(&memory);
    assert_eq!(messages[0]["content"], "SUMMARY");
    assert_eq!(messages[1]["content"], "message number 4");
    assert_eq!(messages[2]["content"], "message number 5");
}

#[test]
fn reloads_after_compaction_via_manifest() {
    let fs: Arc<dyn ClawFs> = Arc::new(MemFs::default());
    let shared = pool();

    let mut memory = ConversationMemory::new(
        1,
        instant_config("/c", 5, 2),
        deps(Arc::clone(&fs), Arc::clone(&shared), "SUMMARY"),
    );
    for i in 0..6 {
        memory.group().append_user(format!("m{i}"));
    }
    assert!(pump_until(&mut memory, |m| {
        let m = messages(m);
        m.len() == 3 && m[0]["content"] == "SUMMARY"
    }));
    memory.flush();

    // A fresh memory restores the same view from the manifest + data log.
    let reloaded = ConversationMemory::new(
        1,
        instant_config("/c", 5, 2),
        deps(Arc::clone(&fs), Arc::clone(&shared), "SUMMARY"),
    );
    let m = messages(&reloaded);
    assert_eq!(m.len(), 3);
    assert_eq!(m[0]["content"], "SUMMARY");
    assert_eq!(m[1]["content"], "m4");
    assert_eq!(m[2]["content"], "m5");
}

#[test]
fn reloads_from_data_log_without_manifest() {
    let fs: Arc<dyn ClawFs> = Arc::new(MemFs::default());
    let shared = pool();

    let mut memory = ConversationMemory::new(
        3,
        instant_config("/c", 100_000, 8),
        deps(Arc::clone(&fs), Arc::clone(&shared), "S"),
    );
    memory.group().append_user("a");
    memory.group().append_user("b");
    memory.group().append_user("c");

    // Appends are append-only; without a flush/compaction no manifest is written.
    // A fresh load must reconstruct purely from a full data-log scan.
    let fs_for_reload = Arc::clone(&fs);
    let pool_for_reload = Arc::clone(&shared);
    assert!(wait_until(move || {
        let reloaded = ConversationMemory::new(
            3,
            ConversationConfig::new("/c"),
            deps(Arc::clone(&fs_for_reload), Arc::clone(&pool_for_reload), "S"),
        );
        messages(&reloaded).len() == 3
    }));
    assert!(fs.exists("/c/conversation-3.jsonl"));
    assert!(!fs.exists("/c/conversation-3.json"));
}

#[test]
fn reload_tail_scans_appends_after_manifest() {
    let fs: Arc<dyn ClawFs> = Arc::new(MemFs::default());
    let shared = pool();

    let mut memory = ConversationMemory::new(
        4,
        instant_config("/c", 100_000, 8),
        deps(Arc::clone(&fs), Arc::clone(&shared), "S"),
    );
    memory.group().append_user("a");
    memory.group().append_user("b");
    memory.flush(); // manifest now covers a, b
    memory.group().append_user("c"); // appended past covered_len; manifest left stale

    let fs_for_reload = Arc::clone(&fs);
    let pool_for_reload = Arc::clone(&shared);
    assert!(wait_until(move || {
        let reloaded = ConversationMemory::new(
            4,
            ConversationConfig::new("/c"),
            deps(Arc::clone(&fs_for_reload), Arc::clone(&pool_for_reload), "S"),
        );
        let m = messages(&reloaded);
        m.len() == 3 && m[2]["content"] == "c"
    }));
}

#[test]
fn collapse_reclaims_dead_bytes() {
    let memfs = Arc::new(MemFs::default());
    let fs: Arc<dyn ClawFs> = Arc::clone(&memfs) as Arc<dyn ClawFs>;

    let mut memory = ConversationMemory::new(
        5,
        instant_config("/c", 5, 2),
        deps(Arc::clone(&fs), pool(), "SUMMARY"),
    );
    let big = "x".repeat(1024);
    for i in 0..60 {
        memory.group().append_user(format!("{big}{i}"));
    }

    // Append-only growth would push the data log well past 60 KiB; collapse must
    // rewrite it from the small live set, keeping it bounded. Pump turn boundaries
    // so the background summaries get applied (and dead bytes reclaimed).
    assert!(pump_until(&mut memory, |_| {
        memfs.len("/c/conversation-5.jsonl").is_ok_and(|n| n < 30 * 1024)
    }));
    let m = messages(&memory);
    assert_eq!(m[0]["content"], "SUMMARY");
}

#[test]
fn torn_trailing_line_is_ignored_on_load() {
    let fs: Arc<dyn ClawFs> = Arc::new(MemFs::default());

    // One complete record line, then a torn (newline-less) partial record.
    let mut data = serde_json::to_vec(
        &json!({ "t": "group", "id": 0, "msgs": [{ "role": "user", "content": "ok" }] }),
    )
    .unwrap();
    data.push(b'\n');
    data.extend_from_slice(br#"{"t":"group","id":1,"msgs":[{"role":"#);
    fs.append("/c/conversation-9.jsonl", &data).unwrap();

    let memory = ConversationMemory::new(
        9,
        ConversationConfig::new("/c"),
        deps(Arc::clone(&fs), pool(), "S"),
    );
    let m = messages(&memory);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["content"], "ok");
}

#[test]
fn invalid_assistant_json_is_dropped_not_panicking() {
    let mut memory = memory_with(Arc::new(MemFs::default()));

    {
        let turn = memory.group();
        turn.append_assistant("this is not json");
        turn.append_user("valid");
    }

    let messages = messages(&memory);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], "valid");
}
