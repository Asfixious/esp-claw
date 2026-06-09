//! Small helpers ported from `claw_core_utils.c`.
//!
//! The C `dup_string`/`dup_printf` C-memory helpers are unnecessary in Rust
//! (we use `String`). The summary/CSV builders keep the C truncation semantics
//! because their output is fed to the LLM and logs.

use crate::consts::{LOG_SNIPPET_LEN, OBS_CSV_MAX, TOOL_SUMMARY_MAX_LEN};
use claw_interfaces::error::{EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_NO_MEM, ESP_OK};

/// `claw_core_now_ms` — milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => 0,
    }
}

/// Number of leading bytes to show for a log snippet, capped at
/// [`LOG_SNIPPET_LEN`] (mirrors `claw_core_log_snippet_len`).
pub fn log_snippet_len(text: &str) -> usize {
    text.len().min(LOG_SNIPPET_LEN)
}

/// `"..."` if the text was truncated, else `""` (`claw_core_log_snippet_suffix`).
pub fn log_snippet_suffix(text: &str) -> &'static str {
    if text.len() > LOG_SNIPPET_LEN {
        "..."
    } else {
        ""
    }
}

/// Render a log snippet: the leading [`LOG_SNIPPET_LEN`] bytes (rounded down to
/// a char boundary) plus the truncation suffix.
pub fn truncate_for_log(text: &str) -> String {
    let mut end = log_snippet_len(text);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], log_snippet_suffix(text))
}

/// Append one tool-result line to a bounded summary buffer, mirroring
/// `claw_core_append_tool_summary_line` (including the `[tool_calls]\n` header
/// on the first line and the [`TOOL_SUMMARY_MAX_LEN`] cap).
pub fn append_tool_summary_line(summary: &mut String, tool_name: &str, ok: bool) -> EspErr {
    if tool_name.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    if summary.len() >= TOOL_SUMMARY_MAX_LEN - 1 {
        return ESP_ERR_NO_MEM;
    }
    let header = if summary.is_empty() { "[tool_calls]\n" } else { "" };
    let line = format!("{header}- {tool_name}: {}\n", if ok { "ok" } else { "failed" });
    if summary.len() + line.len() >= TOOL_SUMMARY_MAX_LEN {
        return ESP_ERR_NO_MEM;
    }
    summary.push_str(&line);
    ESP_OK
}

fn obs_csv_contains(csv: &str, name: &str) -> bool {
    csv.split(',').any(|tok| tok == name)
}

/// Append a provider name to a comma-separated list, optionally de-duplicated,
/// honouring the [`OBS_CSV_MAX`] cap (`claw_core_obs_csv_append`).
pub fn obs_csv_append(csv: &mut String, name: &str, dedup: bool) {
    if name.is_empty() {
        return;
    }
    if dedup && obs_csv_contains(csv, name) {
        return;
    }
    if csv.len() >= OBS_CSV_MAX - 1 {
        return;
    }
    let piece = if csv.is_empty() { name.to_string() } else { format!(",{name}") };
    if csv.len() + piece.len() >= OBS_CSV_MAX {
        // Mirror the C truncation: drop the overflowing append.
        return;
    }
    csv.push_str(&piece);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_summary_header_then_lines() {
        let mut s = String::new();
        assert_eq!(append_tool_summary_line(&mut s, "files", true), ESP_OK);
        assert_eq!(s, "[tool_calls]\n- files: ok\n");
        assert_eq!(append_tool_summary_line(&mut s, "http", false), ESP_OK);
        assert_eq!(s, "[tool_calls]\n- files: ok\n- http: failed\n");
        assert_eq!(append_tool_summary_line(&mut s, "", true), ESP_ERR_INVALID_ARG);
    }

    #[test]
    fn obs_csv_dedup_and_order() {
        let mut csv = String::new();
        obs_csv_append(&mut csv, "memory", true);
        obs_csv_append(&mut csv, "skills", true);
        obs_csv_append(&mut csv, "memory", true);
        assert_eq!(csv, "memory,skills");
        obs_csv_append(&mut csv, "memory", false);
        assert_eq!(csv, "memory,skills,memory");
    }

    #[test]
    fn snippet_len_and_suffix() {
        let short = "hi";
        assert_eq!(log_snippet_len(short), 2);
        assert_eq!(log_snippet_suffix(short), "");
        let long = "x".repeat(LOG_SNIPPET_LEN + 10);
        assert_eq!(log_snippet_len(&long), LOG_SNIPPET_LEN);
        assert_eq!(log_snippet_suffix(&long), "...");
    }
}
