//! Shared helpers: byte-bounded copy, UTF-8 helpers, line lists, file IO, path
//! joining, timestamps, and C-string interop. Ports `claw_memory_utils.c` and
//! the UTF-8 / normalization helpers used across the component.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::consts::MAX_PATH;
use crate::error::{MemoryError, MemoryResult};

// --- byte-bounded copy ----------------------------------------------------

/// Mirror of C `safe_copy(dst, dst_size, src)`: copy at most `dst_size - 1`
/// bytes, but back off to a UTF-8 char boundary so the resulting Rust `String`
/// stays valid. (C truncates raw bytes; we never split a multibyte sequence.)
pub fn safe_copy(src: &str, dst_size: usize) -> String {
    if dst_size == 0 {
        return String::new();
    }
    truncate_to_bytes(src, dst_size - 1)
}

/// Truncate `s` to at most `max_bytes`, on a char boundary.
pub fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// --- UTF-8 helpers --------------------------------------------------------

/// Mirror of `utf8_copy_chars`: copy up to `max_chars` characters into a buffer
/// of `dst_size` bytes (leaving room for NUL: `off + seq_len < dst_size`).
pub fn utf8_copy_chars(src: &str, dst_size: usize, max_chars: usize) -> String {
    let mut out = String::new();
    let mut bytes = 0usize;
    for ch in src.chars().take(max_chars) {
        let l = ch.len_utf8();
        if bytes + l >= dst_size {
            break;
        }
        out.push(ch);
        bytes += l;
    }
    out
}

/// Mirror of `claw_memory_normalize_session_text`.
pub fn normalize_session_text(src: &str, dst_size: usize, max_chars: usize) -> String {
    let mut out = String::new();
    let mut bytes = 0usize;
    let mut chars = 0usize;
    for ch in src.chars() {
        if bytes + 1 >= dst_size || chars >= max_chars {
            break;
        }
        if ch.is_ascii() {
            out.push(ch);
            bytes += 1;
            chars += 1;
            continue;
        }
        let l = ch.len_utf8();
        if bytes + l >= dst_size {
            // C advances one byte and retries; with our oversized buffer this
            // branch is unreachable before chars hits max_chars.
            continue;
        }
        out.push(ch);
        bytes += l;
        chars += 1;
    }
    out
}

/// Mirror of `utf8_is_common_punctuation` (same CJK punctuation list).
pub fn is_common_punctuation(ch: char) -> bool {
    const PUNCTS: &[char] = &[
        '。', '，', '！', '？', '；', '：', '、', '（', '）', '【', '】', '《', '》', '“', '”',
        '‘', '’', '「', '」', '『', '』', '·', '…', '—', '－', '～', '　', '．',
    ];
    PUNCTS.contains(&ch)
}

/// Mirror of `normalize_text_for_key`.
pub fn normalize_text_for_key(src: &str, dst_size: usize) -> String {
    let mut out = String::new();
    let mut bytes = 0usize;
    for ch in src.chars() {
        if bytes + 1 >= dst_size {
            break;
        }
        if ch.is_ascii() {
            let lc = ch.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() {
                out.push(lc);
                bytes += 1;
            }
            continue;
        }
        if is_common_punctuation(ch) {
            continue;
        }
        let l = ch.len_utf8();
        if bytes + l >= dst_size {
            break;
        }
        out.push(ch);
        bytes += l;
    }
    out
}

/// Mirror of C `isspace` (ASCII) used by `trim_whitespace`.
fn is_ascii_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// Mirror of `trim_whitespace` (ASCII whitespace only, both ends).
pub fn trim_whitespace(s: &str) -> &str {
    s.trim_matches(is_ascii_space)
}

/// Mirror of `text_contains_ascii_ci`.
pub fn text_contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay = haystack.as_bytes();
    let need = needle.as_bytes();
    if need.len() > hay.len() {
        return false;
    }
    hay.windows(need.len()).any(|window| window.eq_ignore_ascii_case(need))
}

/// Mirror of `text_contains_token`: case-sensitive substring OR ASCII-CI.
pub fn text_contains_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.contains(needle) || text_contains_ascii_ci(haystack, needle)
}

/// Split a CSV string on any of `,;/|` (mirrors `strtok_r(s, ",;/|", ...)`,
/// which collapses consecutive delimiters and drops empty tokens).
pub fn csv_split(s: &str) -> Vec<&str> {
    s.split([',', ';', '/', '|'])
        .filter(|t| !t.is_empty())
        .collect()
}

// --- line lists -----------------------------------------------------------

pub fn line_list_contains(list: &str, item: &str) -> bool {
    if item.is_empty() {
        return false;
    }
    list.split('\n').any(|line| line == item)
}

/// Mirror of `line_list_append_unique`. Returns `MemoryError::InvalidArg` for an
/// empty item (matching the C `ESP_ERR_INVALID_ARG`).
pub fn line_list_append_unique(list: &mut Option<String>, item: &str) -> MemoryResult<()> {
    if item.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    match list {
        Some(existing) => {
            if line_list_contains(existing, item) {
                return Ok(());
            }
            existing.push('\n');
            existing.push_str(item);
        }
        None => {
            *list = Some(item.to_string());
        }
    }
    Ok(())
}

/// Mirror of `line_list_merge_unique` (each source line is truncated to 255
/// bytes before being appended).
pub fn line_list_merge_unique(dst: &mut Option<String>, src: Option<&str>) -> MemoryResult<()> {
    let src = match src {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    for line in src.split('\n') {
        let truncated = truncate_to_bytes(line, 255);
        if let Err(e) = line_list_append_unique(dst, &truncated) {
            // C skips empty lines via append returning INVALID_ARG only when
            // item is empty; replicate by ignoring empty truncations.
            if truncated.is_empty() {
                continue;
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Mirror of `claw_memory_format_update_stage_note`.
pub fn format_update_stage_note(summary_list: Option<&str>) -> Option<String> {
    let list = match summary_list {
        Some(s) if !s.is_empty() => s,
        _ => return None,
    };
    let joined = list.replace('\n', ";");
    Some(format!("Memory update: [{}]", joined))
}

// --- paths ----------------------------------------------------------------

/// Mirror of `claw_memory_join_path`.
pub fn join_path(dir: &str, name: &str) -> MemoryResult<String> {
    if dir.len() + 1 + name.len() + 1 > MAX_PATH {
        return Err(MemoryError::InvalidSize);
    }
    Ok(format!("{}/{}", dir, name))
}

/// Mirror of `ensure_dir_recursive`.
pub fn ensure_dir_recursive(path: &str) -> MemoryResult<()> {
    if path.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    if path.len() >= MAX_PATH {
        return Err(MemoryError::InvalidSize);
    }
    fs::create_dir_all(path).map_err(|_| MemoryError::Fail)
}

/// Mirror of `ensure_parent_dir`.
pub fn ensure_parent_dir(path: &str) -> MemoryResult<()> {
    if path.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    if path.len() >= MAX_PATH {
        return Err(MemoryError::InvalidSize);
    }
    match path.rfind('/') {
        None => Ok(()),
        Some(0) => Ok(()),
        Some(idx) => ensure_dir_recursive(&path[..idx]),
    }
}

// --- file IO --------------------------------------------------------------

/// Mirror of `read_file_dup`. Any open failure maps to `MemoryError::NotFound`
/// (as the C code returns `ESP_ERR_NOT_FOUND` for a failed `fopen`).
pub fn read_file_dup(path: &str) -> MemoryResult<String> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Err(MemoryError::NotFound),
    };
    let mut buf = Vec::new();
    match file.read_to_end(&mut buf) {
        Ok(_) => Ok(String::from_utf8_lossy(&buf).into_owned()),
        Err(_) => Err(MemoryError::Fail),
    }
}

pub fn write_file_text(path: &str, text: &str) -> MemoryResult<()> {
    ensure_parent_dir(path).map_err(|_| MemoryError::Fail)?;
    fs::write(path, text.as_bytes()).map_err(|_| MemoryError::Fail)
}

pub fn append_file_text(path: &str, text: &str) -> MemoryResult<()> {
    use std::io::Write;
    ensure_parent_dir(path).map_err(|_| MemoryError::Fail)?;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|_| MemoryError::Fail)?;
    file.write_all(text.as_bytes()).map_err(|_| MemoryError::Fail)
}

pub fn file_size_bytes(path: &str) -> usize {
    match fs::metadata(path) {
        Ok(m) => m.len() as usize,
        Err(_) => 0,
    }
}

pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Mirror of `ensure_file_with_default`.
pub fn ensure_file_with_default(path: &str, default_text: &str) -> MemoryResult<()> {
    if fs::File::open(path).is_ok() {
        return Ok(());
    }
    write_file_text(path, default_text)
}

// --- time -----------------------------------------------------------------

/// Mirror of `claw_memory_now_sec` (gettimeofday tv_sec truncated to u32).
pub fn now_sec() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Mirror of `claw_memory_format_timestamp`: localtime + strftime, with the
/// numeric fallback. Uses libc so it matches the C output byte-for-byte.
pub fn format_timestamp(ts: u32) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    unsafe {
        let raw: libc::time_t = ts as libc::time_t;
        let mut tm: libc::tm = core::mem::zeroed();
        if libc::localtime_r(&raw, &mut tm).is_null() {
            return ts.to_string();
        }
        // Format "%Y-%m-%d %H:%M:%S" from the broken-down time fields. `strftime`
        // is not bound by `libc` for the espidf target, so build it directly
        // (tm_year is years since 1900, tm_mon is 0-based).
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

