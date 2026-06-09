//! Lightweight long-term memory provider (raw `MEMORY.md`), port of
//! `claw_memory_lightweight.c`.

use claw_interfaces::error::{EspErr, ESP_ERR_NOT_FOUND};

use crate::state;
use crate::util;

/// `claw_memory_long_term_lightweight_collect`.
pub fn lightweight_collect_content() -> Result<String, EspErr> {
    let st = state::lock();
    let content = util::read_file_dup(&st.markdown_path)?;
    if content.is_empty() {
        return Err(ESP_ERR_NOT_FOUND);
    }
    Ok(content)
}
