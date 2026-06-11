//! Lightweight long-term memory provider (raw `MEMORY.md`), port of
//! `claw_memory_lightweight.c`.

use crate::error::{MemoryError, MemoryResult};
use crate::state;
use crate::util;

/// `claw_memory_long_term_lightweight_collect`.
pub fn lightweight_collect_content() -> MemoryResult<String> {
    let st = state::lock();
    let content = util::read_file_dup(&st.markdown_path)?;
    if content.is_empty() {
        return Err(MemoryError::NotFound);
    }
    Ok(content)
}
