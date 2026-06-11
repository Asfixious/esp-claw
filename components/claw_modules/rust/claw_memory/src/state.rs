//! Process-global memory state (`claw_memory_state_t`), guarded by a single
//! mutex. The C code accesses `s_memory` directly and is effectively
//! single-threaded for these operations (the async-extract worker only reads
//! the index path); a single lock serializing state + file operations is safe.

use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::consts::BackendFormat;

#[derive(Debug, Default)]
pub struct MemoryState {
    pub initialized: bool,
    pub session_root_dir: String,
    pub memory_root_dir: String,
    pub markdown_path: String,
    pub records_path: String,
    pub index_path: String,
    pub digest_path: String,
    pub soul_path: String,
    pub identity_path: String,
    pub user_path: String,
    pub max_message_chars: usize,
    pub max_tool_iterations: u32,
    pub backend_format: BackendFormat,
    pub write_changes_since_compact: u32,
    pub next_memory_seq: u32,
}

static MEMORY: OnceLock<Mutex<MemoryState>> = OnceLock::new();

fn cell() -> &'static Mutex<MemoryState> {
    MEMORY.get_or_init(|| Mutex::new(MemoryState::default()))
}

/// Acquire the global memory state lock (recovering from poisoning).
pub fn lock() -> MutexGuard<'static, MemoryState> {
    cell().lock().unwrap_or_else(|e| e.into_inner())
}
