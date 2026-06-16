//! `claw-memory` — the agent memory subsystem.
//!
//! Today this holds the short-term conversation memory ([`ConversationMemory`])
//! and the shared worker pool ([`MemoryTaskPool`]) that runs its background jobs
//! (compaction now; profile / long-term extraction later). Every memory type
//! submits its background work to the same pool so FreeRTOS task allocation stays
//! amortized.
//!
//! At its core the crate depends only on the cycle-breaking [`claw_interfaces`]
//! (for the [`ClawFs`](claw_interfaces::ClawFs) persistence seam) and
//! [`claw_sys`] (for worker spawning); summarization is injected via the
//! [`Compactor`] trait. The default `llm` feature adds a ready-made
//! `LlmCompactor` backed by `claw-api`; turn the feature off to keep the crate
//! LLM-free and supply your own [`Compactor`].
//!
//! # Using the conversation memory
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use claw_interfaces::{ClawFs, FsError};
//! use claw_memory::{
//!     CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
//!     MemoryTaskPool, PoolConfig,
//! };
//! use serde_json::{json, Value};
//!
//! // 1. A filesystem for persistence. On device this is backed by the DATA root;
//! //    here it is a stub.
//! struct MyFs;
//! impl ClawFs for MyFs {
//!     fn read(&self, _path: &str) -> Result<Vec<u8>, FsError> { Err(FsError::NotFound) }
//!     fn read_at(&self, _path: &str, _off: u64, _len: usize) -> Result<Vec<u8>, FsError> {
//!         Err(FsError::NotFound)
//!     }
//!     fn len(&self, _path: &str) -> Result<u64, FsError> { Err(FsError::NotFound) }
//!     fn write_atomic(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> { Ok(()) }
//!     fn append(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> { Ok(()) }
//!     fn exists(&self, _path: &str) -> bool { false }
//!     fn remove(&self, _path: &str) -> Result<(), FsError> { Ok(()) }
//! }
//!
//! // 2. A compactor that folds an aged window into a summary (e.g. an LLM call).
//! struct MyCompactor;
//! impl Compactor for MyCompactor {
//!     fn compact(&self, window: &[Value]) -> Result<Vec<Value>, CompactError> {
//!         Ok(vec![json!({
//!             "role": "system",
//!             "content": format!("summary of {} earlier messages", window.len()),
//!         })])
//!     }
//! }
//!
//! // 3. One pool is shared by every memory type — create it once at boot.
//! let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default())?);
//!
//! // 4. Build the conversation memory for one conversation id. Typically one
//! //    memory per agent instance; all of them share the single pool above.
//! let conversation_id = 42;
//! let mut memory = ConversationMemory::new(
//!     conversation_id,
//!     ConversationConfig::new("/data/conversations"),
//!     ConversationDeps {
//!         fs: Arc::new(MyFs),
//!         pool: Arc::clone(&pool),
//!         compactor: Arc::new(MyCompactor),
//!     },
//! );
//!
//! // 5. Drive it from the agent loop. One turn = one `group()`; the whole turn
//! //    is committed as a single record when the guard drops.
//! {
//!     let turn = memory.group();
//!     turn.append_user("what's the weather?");
//!     turn.append_assistant(r#"{"role":"assistant","content":"Sunny."}"#);
//!     turn.append_tool_result("call_1", "{\"temp_c\":21}", false);
//!
//!     // memory.messages() includes the open turn; feed to the model.
//!     let messages = memory.messages();
//!     let _ = messages;
//! } // drop → the turn is committed
//!
//! memory.flush(); // force a checkpoint, e.g. on a clean shutdown
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Compaction is automatic and invisible: when committing a turn grows the
//! transcript past [`ConversationConfig::compact_threshold_tokens`], the memory
//! schedules a background job on the pool that calls your [`Compactor`] and
//! splices in the result. Callers never trigger it.

pub mod compaction;
pub mod conversation_memory;
#[cfg(feature = "llm")]
pub mod llm_compactor;
pub mod pool;

pub use compaction::{CompactError, Compactor};
pub use conversation_memory::{
    ConversationConfig, ConversationDeps, ConversationMemory, GroupGuard,
};
#[cfg(feature = "llm")]
pub use llm_compactor::LlmCompactor;
pub use pool::{MemoryJob, MemoryTaskPool, PoolConfig};
