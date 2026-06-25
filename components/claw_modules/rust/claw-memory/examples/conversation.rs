//! Drive a [`ConversationMemory`] through a few turns and inspect what the
//! model would see.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p claw-memory --example conversation --target x86_64-unknown-linux-gnu
//! ```
//!
//! Summarization is injected via the [`Compactor`] trait, so the example needs
//! no LLM: we supply a tiny local compactor. (Build with
//! `--no-default-features` to confirm the crate is LLM-free without the `llm`
//! feature.) Persistence is an in-memory [`MemFs`] — on device the same code
//! runs over the DATA root.

use std::sync::Arc;

use claw_interface::MemFs;
use claw_memory::{
    CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
    MemoryTaskPool, PoolConfig,
};
use serde_json::{json, Value};

/// A stand-in for the real summarizer: folds an aged window of messages into a
/// single system note. A real compactor would call an LLM here.
struct CountingCompactor;

impl Compactor for CountingCompactor {
    fn compact(&self, window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(vec![json!({
            "role": "system",
            "content": format!("[summary of {} earlier messages]", window.len()),
        })])
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // One pool is shared by every memory type; create it once at boot.
    let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default())?);

    let conversation_id = 42;
    let memory = ConversationMemory::new(
        conversation_id,
        ConversationConfig::new("/data/conversations"),
        ConversationDeps {
            fs: Arc::new(MemFs::new()),
            pool: Arc::clone(&pool),
            compactor: Arc::new(CountingCompactor),
        },
    );

    // One turn = one `group()`. The whole turn commits as a single record when
    // the guard drops at the end of the scope.
    {
        let turn = memory.group();
        turn.append_user("what's the weather in Shanghai?");
        turn.append_assistant(r#"{"role":"assistant","content":"Let me check."}"#);
        turn.append_tool_result("call_1", r#"{"temp_c":21,"sky":"clear"}"#, false);
    }

    {
        let turn = memory.group();
        turn.append_user("and tomorrow?");
        turn.append_assistant(r#"{"role":"assistant","content":"Sunny, around 23C."}"#);
    }

    // `messages()` is what you feed to the model — committed turns plus any open
    // one, with compaction (if it fired) already spliced in.
    let messages = memory.messages();
    let count = messages.as_array().map_or(0, Vec::len);
    println!("conversation has {count} message(s) to send to the model:\n");
    println!("{}", serde_json::to_string_pretty(&messages)?);

    // Force a checkpoint, e.g. on a clean shutdown.
    memory.flush();
    Ok(())
}
