//! Session history provider plus persist / gate / start / stage-note callbacks,
//! raw record writing, the binary index format, compaction, and deletion.
//! Ports `claw_memory_session.c`.

use core::ffi::c_char;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use claw_core::cabi::claw_core_request_t;
use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_SIZE, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND,
    ESP_FAIL, ESP_OK,
};

use crate::consts::{
    BackendFormat, CONTEXT_RECORD_ASSISTANT_FINAL, CONTEXT_RECORD_ASSISTANT_TOOL,
    CONTEXT_RECORD_TOOL_RESULT, CONTEXT_RECORD_USER, DEFAULT_MAX_MESSAGE_CHARS, SESSION_COMPACT_TOOL_TURNS,
    SESSION_IDX_ENTRY_SIZE, SESSION_IDX_HEADER_SIZE, SESSION_IDX_MAGIC, SESSION_IDX_VERSION,
    SESSION_SIZE_LIMIT, SESSION_SIZE_WARNING,
};
use crate::state;
use crate::util;

extern "C" {
    fn claw_core_publish_stage_text(request: *const claw_core_request_t, text: *const c_char) -> EspErr;
}

// --- pending summaries / request states -----------------------------------

struct PendingSummary {
    session_id: String,
    summary_list: Option<String>,
}

fn pending() -> &'static Mutex<Vec<PendingSummary>> {
    static P: OnceLock<Mutex<Vec<PendingSummary>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

fn request_states() -> &'static Mutex<Vec<(u32, bool)>> {
    static R: OnceLock<Mutex<Vec<(u32, bool)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// `claw_memory_note_session_summary`.
pub fn note_session_summary(session_id: &str, summary_list: &str) -> EspErr {
    if session_id.is_empty() || summary_list.is_empty() {
        return ESP_OK;
    }
    let mut guard = pending().lock().unwrap_or_else(|e| e.into_inner());
    let idx = match guard.iter().position(|p| p.session_id == session_id) {
        Some(i) => i,
        None => {
            guard.push(PendingSummary {
                session_id: session_id.to_string(),
                summary_list: None,
            });
            guard.len() - 1
        }
    };
    util::line_list_merge_unique(&mut guard[idx].summary_list, Some(summary_list))
}

fn pending_take_summary_list(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let mut guard = pending().lock().unwrap_or_else(|e| e.into_inner());
    let idx = guard.iter().position(|p| p.session_id == session_id)?;
    let node = guard.remove(idx);
    node.summary_list
}

/// `claw_memory_request_mark_manual_write`.
pub fn request_mark_manual_write(request_id: u32) -> EspErr {
    if request_id == 0 {
        return ESP_ERR_INVALID_ARG;
    }
    let mut guard = request_states().lock().unwrap_or_else(|e| e.into_inner());
    match guard.iter_mut().find(|(id, _)| *id == request_id) {
        Some(entry) => entry.1 = true,
        None => guard.push((request_id, true)),
    }
    ESP_OK
}

fn request_take_manual_write(request_id: u32) -> bool {
    if request_id == 0 {
        return false;
    }
    let mut guard = request_states().lock().unwrap_or_else(|e| e.into_inner());
    match guard.iter().position(|(id, _)| *id == request_id) {
        Some(idx) => guard.remove(idx).1,
        None => false,
    }
}

// --- paths ----------------------------------------------------------------

fn sanitize_session_id(session_id: &str) -> String {
    // Mirror claw_memory_sanitize_session_id: iterate raw bytes into buf[48]
    // (up to 47 bytes), mapping every non-[alnum,-,_] byte to '_'. Output is
    // always ASCII, so byte counting matches the C buffer offset exactly.
    let mut out = String::new();
    for &b in session_id.as_bytes() {
        if out.len() + 1 >= 48 {
            break;
        }
        let c = b as char;
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// `claw_memory_session_path_dup`.
fn session_path_dup(state: &state::MemoryState, session_id: &str) -> String {
    let mut hash: u32 = 2166136261;
    for &b in session_id.as_bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    let mut sanitized = sanitize_session_id(session_id);
    if sanitized.len() > 24 {
        // Output is ASCII so byte 24 is always a char boundary.
        sanitized.truncate(24);
    }
    let name = if sanitized.is_empty() { "default" } else { &sanitized };
    format!("{}/s_{}_{:08x}.log", state.session_root_dir, name, hash)
}

fn idx_path_dup(data_path: &str) -> String {
    format!("{}.idx", data_path)
}

fn blocked_path_dup(data_path: &str) -> String {
    format!("{}.blocked", data_path)
}

// --- record/backend validity ----------------------------------------------

fn record_type_valid(record_type: u8) -> bool {
    matches!(record_type as i32, x if x == CONTEXT_RECORD_USER
        || x == CONTEXT_RECORD_ASSISTANT_FINAL
        || x == CONTEXT_RECORD_ASSISTANT_TOOL
        || x == CONTEXT_RECORD_TOOL_RESULT)
}

fn backend_format_valid(v: u8) -> bool {
    BackendFormat::from_u8(v).is_some()
}

fn record_text_role(record_type: i32) -> Option<&'static str> {
    match record_type {
        x if x == CONTEXT_RECORD_USER => Some("user"),
        x if x == CONTEXT_RECORD_ASSISTANT_FINAL => Some("assistant"),
        _ => None,
    }
}

// --- binary index ---------------------------------------------------------

#[derive(Clone, Copy)]
struct IndexEntry {
    offset: u32,
    length: u32,
    record_type: u8,
    backend_format: u8,
}

fn pack_header() -> [u8; SESSION_IDX_HEADER_SIZE] {
    let mut buf = [0u8; SESSION_IDX_HEADER_SIZE];
    buf[0..4].copy_from_slice(&SESSION_IDX_MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&SESSION_IDX_VERSION.to_le_bytes());
    buf[6..8].copy_from_slice(&(SESSION_IDX_ENTRY_SIZE as u16).to_le_bytes());
    buf
}

fn header_valid(buf: &[u8]) -> bool {
    if buf.len() < SESSION_IDX_HEADER_SIZE {
        return false;
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let version = u16::from_le_bytes([buf[4], buf[5]]);
    let entry_size = u16::from_le_bytes([buf[6], buf[7]]);
    if magic != SESSION_IDX_MAGIC {
        log::warn!("Invalid session history idx magic");
        return false;
    }
    if version != SESSION_IDX_VERSION {
        log::warn!("Unsupported session history idx version {}", version);
        return false;
    }
    if entry_size as usize != SESSION_IDX_ENTRY_SIZE {
        log::warn!("Invalid session history idx entry size {}", entry_size);
        return false;
    }
    true
}

fn pack_entry(entry: &IndexEntry) -> [u8; SESSION_IDX_ENTRY_SIZE] {
    let mut buf = [0u8; SESSION_IDX_ENTRY_SIZE];
    buf[0..4].copy_from_slice(&entry.offset.to_le_bytes());
    buf[4..8].copy_from_slice(&entry.length.to_le_bytes());
    buf[8] = entry.record_type;
    buf[9] = entry.backend_format;
    buf
}

fn parse_entry(buf: &[u8]) -> IndexEntry {
    IndexEntry {
        offset: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        length: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        record_type: buf[8],
        backend_format: buf[9],
    }
}

/// `session_history_read_index_file`.
fn read_index_file(idx_path: &str) -> Result<Vec<IndexEntry>, EspErr> {
    let bytes = match fs::read(idx_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::error!("open session history idx {} failed", idx_path);
            return Err(ESP_ERR_NOT_FOUND);
        }
        Err(_) => {
            log::error!("open session history idx {} failed", idx_path);
            return Err(ESP_FAIL);
        }
    };

    if bytes.len() < SESSION_IDX_HEADER_SIZE {
        log::warn!("read session history idx header failed");
        return Err(ESP_ERR_INVALID_STATE);
    }
    if !header_valid(&bytes[..SESSION_IDX_HEADER_SIZE]) {
        return Err(ESP_ERR_INVALID_STATE);
    }

    let payload = bytes.len() - SESSION_IDX_HEADER_SIZE;
    if payload % SESSION_IDX_ENTRY_SIZE != 0 {
        log::warn!("Session history idx file has a partial entry");
        return Err(ESP_ERR_INVALID_STATE);
    }
    let entry_count = payload / SESSION_IDX_ENTRY_SIZE;

    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let off = SESSION_IDX_HEADER_SIZE + i * SESSION_IDX_ENTRY_SIZE;
        let entry = parse_entry(&bytes[off..off + SESSION_IDX_ENTRY_SIZE]);
        if !record_type_valid(entry.record_type) {
            log::warn!(
                "Invalid session history record_type entry={} type={}",
                i,
                entry.record_type
            );
            return Err(ESP_ERR_INVALID_STATE);
        }
        if !backend_format_valid(entry.backend_format) {
            log::warn!(
                "Invalid session history backend_format entry={} format={}",
                i,
                entry.backend_format
            );
            return Err(ESP_ERR_INVALID_STATE);
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// `session_history_validate_pair`.
fn validate_pair(data_path: &str, idx_path: &str) -> Result<Vec<IndexEntry>, EspErr> {
    let entries = read_index_file(idx_path)?;
    if !util::path_exists(data_path) {
        return Err(ESP_ERR_NOT_FOUND);
    }
    let data_size = util::file_size_bytes(data_path);
    if entries.is_empty() && data_size > 0 {
        log::warn!("Session history data has no matching idx entries");
        return Err(ESP_ERR_INVALID_STATE);
    }
    for (i, entry) in entries.iter().enumerate() {
        let offset = entry.offset as usize;
        let length = entry.length as usize;
        if length == 0 || offset > data_size || length > data_size - offset {
            log::warn!(
                "Invalid session history entry={} offset={} length={} data_size={}",
                i,
                entry.offset,
                entry.length,
                data_size
            );
            return Err(ESP_ERR_INVALID_STATE);
        }
    }
    Ok(entries)
}

fn record_object_len(entry: &IndexEntry) -> usize {
    if entry.length == 0 {
        0
    } else {
        (entry.length - 1) as usize
    }
}

// --- file (re)creation -----------------------------------------------------

/// `session_history_recreate_file`.
fn recreate_file(data_path: &str, idx_path: &str) -> EspErr {
    if util::ensure_parent_dir(data_path) != ESP_OK {
        return ESP_FAIL;
    }
    if fs::File::create(data_path).is_err() {
        log::error!("create session history {} failed", data_path);
        return ESP_FAIL;
    }
    match fs::File::create(idx_path) {
        Ok(mut f) => {
            if f.write_all(&pack_header()).is_err() {
                log::error!("write session history idx header failed");
                return ESP_FAIL;
            }
            ESP_OK
        }
        Err(_) => {
            log::error!("create session history idx {} failed", idx_path);
            ESP_FAIL
        }
    }
}

fn mark_blocked(data_path: &str) -> EspErr {
    let blocked = blocked_path_dup(data_path);
    match fs::File::create(&blocked) {
        Ok(_) => ESP_OK,
        Err(_) => {
            log::error!("create session history blocked marker {} failed", blocked);
            ESP_FAIL
        }
    }
}

fn session_blocked_path(data_path: &str) -> bool {
    util::path_exists(&blocked_path_dup(data_path))
}

/// `session_history_session_blocked`.
pub fn session_blocked(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let st = state::lock();
    if !st.initialized {
        return false;
    }
    let data_path = session_path_dup(&st, session_id);
    session_blocked_path(&data_path)
}

// --- degrade / load --------------------------------------------------------

fn backend_mismatch(state: &state::MemoryState, record_format: u8) -> bool {
    if state.backend_format == BackendFormat::Unknown
        || record_format == BackendFormat::Unknown.as_u8()
    {
        return false;
    }
    record_format != state.backend_format.as_u8()
}

/// `session_history_degrade_assistant_final`.
fn degrade_assistant_final(record: Value) -> Result<Value, EspErr> {
    let content = record.get("content");
    match content {
        Some(Value::String(_)) => return Ok(record),
        Some(Value::Array(_)) => {}
        _ => return Err(ESP_ERR_NOT_FOUND),
    }

    let mut text = String::new();
    if let Some(blocks) = record.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            let is_text = block.get("type").and_then(|v| v.as_str()) == Some("text");
            if is_text {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }
    if text.is_empty() {
        return Err(ESP_ERR_NOT_FOUND);
    }

    let mut fallback = Map::new();
    fallback.insert("role".into(), Value::from("assistant"));
    fallback.insert("content".into(), Value::from(text));
    Ok(Value::Object(fallback))
}

/// `session_history_load_indexed_json`.
fn load_indexed_json(
    state: &state::MemoryState,
    data: &[u8],
    entries: &[IndexEntry],
) -> Result<String, EspErr> {
    if entries.is_empty() {
        return Err(ESP_ERR_INVALID_ARG);
    }
    let mut records: Vec<Value> = Vec::new();

    for entry in entries {
        let object_len = record_object_len(entry);
        if object_len == 0 {
            log::warn!(
                "Invalid session history entry offset={} length={}",
                entry.offset,
                entry.length
            );
            return Err(ESP_ERR_INVALID_STATE);
        }
        let offset = entry.offset as usize;
        if offset + object_len >= data.len() || data[offset + object_len] != b'\n' {
            log::warn!("session history record missing newline separator");
            return Err(ESP_ERR_INVALID_STATE);
        }
        let slice = &data[offset..offset + object_len];
        let record: Value = match serde_json::from_slice(slice) {
            Ok(v) => v,
            Err(_) => return Err(ESP_ERR_INVALID_STATE),
        };

        let record_type = entry.record_type as i32;
        let mut record = record;
        if backend_mismatch(state, entry.backend_format) {
            if record_type == CONTEXT_RECORD_ASSISTANT_TOOL
                || record_type == CONTEXT_RECORD_TOOL_RESULT
            {
                continue;
            }
            if record_type == CONTEXT_RECORD_ASSISTANT_FINAL {
                match degrade_assistant_final(record) {
                    Ok(v) => record = v,
                    Err(e) if e == ESP_ERR_NOT_FOUND => continue,
                    Err(e) => return Err(e),
                }
            }
        }

        let expand = record_type == CONTEXT_RECORD_TOOL_RESULT;
        if expand {
            if let Value::Array(items) = record {
                for item in items {
                    records.push(item);
                }
                continue;
            } else {
                records.push(record);
            }
        } else {
            records.push(record);
        }
    }

    serde_json::to_string(&Value::Array(records)).map_err(|_| claw_interfaces::error::ESP_ERR_NO_MEM)
}

/// `claw_memory_session_load_json_alloc`. Returns the rendered messages JSON,
/// or an `ESP_ERR_*`.
fn session_load_json_alloc(session_id: &str) -> Result<String, EspErr> {
    let (data_path, idx_path) = {
        let st = state::lock();
        if !st.initialized {
            return Err(ESP_ERR_INVALID_STATE);
        }
        let dp = session_path_dup(&st, session_id);
        let ip = idx_path_dup(&dp);
        (dp, ip)
    };

    let data_exists = util::path_exists(&data_path);
    let idx_exists = util::path_exists(&idx_path);

    let mut err: EspErr = ESP_OK;
    let mut reset_file = false;
    let mut reset_reason = "";
    let mut json: Option<String> = None;

    'block: {
        if !data_exists && !idx_exists {
            err = ESP_ERR_NOT_FOUND;
            break 'block;
        }
        if !data_exists || !idx_exists {
            reset_file = true;
            reset_reason = "missing data/index pair";
            err = ESP_ERR_NOT_FOUND;
            break 'block;
        }

        let entries = match validate_pair(&data_path, &idx_path) {
            Ok(e) => e,
            Err(_) => {
                reset_file = true;
                reset_reason = "invalid data/index pair";
                err = ESP_ERR_NOT_FOUND;
                break 'block;
            }
        };
        if entries.is_empty() {
            err = ESP_ERR_NOT_FOUND;
            break 'block;
        }

        let data = match fs::read(&data_path) {
            Ok(b) => b,
            Err(_) => {
                log::error!("open session history {} failed", data_path);
                err = ESP_FAIL;
                break 'block;
            }
        };

        let st = state::lock();
        match load_indexed_json(&st, &data, &entries) {
            Ok(s) => json = Some(s),
            Err(_) => {
                reset_file = true;
                reset_reason = "read indexed records failed";
                err = ESP_ERR_NOT_FOUND;
            }
        }
    }

    if reset_file {
        log::warn!("Resetting session history {}: {}", data_path, reset_reason);
        let reset_err = recreate_file(&data_path, &idx_path);
        if reset_err != ESP_OK {
            log::error!("reset session history {} failed", data_path);
            err = reset_err;
        }
    }

    if err != ESP_OK {
        return Err(err);
    }
    Ok(json.unwrap_or_default())
}

/// `claw_memory_session_history_collect` content.
pub fn session_history_collect_content(session_id: Option<&str>) -> Result<String, EspErr> {
    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return Err(ESP_ERR_NOT_FOUND),
    };
    let content = session_load_json_alloc(session_id)?;
    if content.is_empty() || content == "[]" {
        return Err(ESP_ERR_NOT_FOUND);
    }
    Ok(content)
}

// --- append ----------------------------------------------------------------

fn text_buffer_size(mut max_chars: usize) -> usize {
    if max_chars == 0 || max_chars > DEFAULT_MAX_MESSAGE_CHARS {
        max_chars = DEFAULT_MAX_MESSAGE_CHARS;
    }
    max_chars * 4 + 1
}

/// `claw_memory_write_session_raw_record`: append `json_text` + '\n' and report
/// the byte offset and the written length (json bytes + 1).
fn write_session_raw_record(data_file: &mut fs::File, json_text: &str) -> Result<(u32, u32), EspErr> {
    let offset = match data_file.seek(SeekFrom::End(0)) {
        Ok(o) if o <= u32::MAX as u64 => o as u32,
        _ => return Err(ESP_FAIL),
    };
    let record_len = json_text.len();
    if record_len as u64 + 1 > u32::MAX as u64 {
        return Err(ESP_ERR_INVALID_SIZE);
    }
    if data_file.write_all(json_text.as_bytes()).is_err() || data_file.write_all(b"\n").is_err() {
        return Err(ESP_FAIL);
    }
    Ok((offset, (record_len + 1) as u32))
}

/// `session_history_append_indexed_record`.
fn append_indexed_record(
    state: &state::MemoryState,
    data_file: &mut fs::File,
    idx_file: &mut fs::File,
    record_type: i32,
    message_json: Option<&str>,
    role: Option<&str>,
    text: Option<&str>,
) -> EspErr {
    let has_json = message_json.map(|s| !s.is_empty()).unwrap_or(false);
    if (!has_json && (role.is_none() || text.is_none())) || !record_type_valid(record_type as u8) {
        return ESP_ERR_INVALID_ARG;
    }

    let owned_json;
    let json_text: &str = if has_json {
        message_json.unwrap()
    } else {
        let max_chars = state.max_message_chars;
        let normalized = util::normalize_session_text(text.unwrap(), text_buffer_size(max_chars), max_chars);
        let mut record = Map::new();
        record.insert("role".into(), Value::from(role.unwrap()));
        record.insert("content".into(), Value::from(normalized));
        owned_json = match serde_json::to_string(&Value::Object(record)) {
            Ok(s) => s,
            Err(_) => return claw_interfaces::error::ESP_ERR_NO_MEM,
        };
        &owned_json
    };

    let (offset, length) = match write_session_raw_record(data_file, json_text) {
        Ok(v) => v,
        Err(e) => {
            log::error!("write session history {} record failed", role.unwrap_or("raw"));
            return e;
        }
    };

    let entry = IndexEntry {
        offset,
        length,
        record_type: record_type as u8,
        backend_format: state.backend_format.as_u8(),
    };
    if idx_file.write_all(&pack_entry(&entry)).is_err() {
        log::error!("write session history idx entry failed");
        return ESP_FAIL;
    }
    ESP_OK
}

/// `session_history_open_pair_for_append`.
fn open_pair_for_append(data_path: &str, idx_path: &str) -> Result<(fs::File, fs::File), EspErr> {
    let validation = validate_pair(data_path, idx_path);
    if let Err(e) = validation {
        if e == ESP_ERR_NOT_FOUND {
            let data_exists = util::path_exists(data_path);
            let idx_exists = util::path_exists(idx_path);
            if data_exists != idx_exists {
                log::warn!("Reinitializing mismatched session history pair {}", data_path);
            }
        } else {
            log::warn!("Reinitializing legacy or invalid session history pair {}", data_path);
        }
        let r = recreate_file(data_path, idx_path);
        if r != ESP_OK {
            return Err(r);
        }
    }

    let data_file = match fs::OpenOptions::new().append(true).create(true).open(data_path) {
        Ok(f) => f,
        Err(_) => {
            log::error!("open session history {} for append failed", data_path);
            return Err(ESP_FAIL);
        }
    };
    let idx_file = match fs::OpenOptions::new().append(true).create(true).open(idx_path) {
        Ok(f) => f,
        Err(_) => {
            log::error!("open session history idx {} for append failed", idx_path);
            return Err(ESP_FAIL);
        }
    };
    Ok((data_file, idx_file))
}

// --- compaction ------------------------------------------------------------

struct Turn {
    start: usize,
    end: usize,
    completed: bool,
    has_tool_records: bool,
    keep_tool_records: bool,
}

/// `session_history_analyze_turns`.
fn analyze_turns(entries: &[IndexEntry]) -> Result<Vec<Turn>, EspErr> {
    if entries.is_empty() {
        return Err(ESP_ERR_NOT_FOUND);
    }
    let mut turns: Vec<Turn> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let record_type = entry.record_type as i32;
        if i == 0 || record_type == CONTEXT_RECORD_USER {
            if let Some(last) = turns.last_mut() {
                last.end = i;
            }
            turns.push(Turn {
                start: i,
                end: 0,
                completed: false,
                has_tool_records: false,
                keep_tool_records: false,
            });
        }
    }
    if turns.is_empty() {
        return Err(ESP_ERR_NOT_FOUND);
    }
    let last = turns.len() - 1;
    turns[last].end = entries.len();

    for turn in turns.iter_mut() {
        for entry in &entries[turn.start..turn.end] {
            let record_type = entry.record_type as i32;
            if record_type == CONTEXT_RECORD_ASSISTANT_FINAL {
                turn.completed = true;
            } else if record_type == CONTEXT_RECORD_ASSISTANT_TOOL
                || record_type == CONTEXT_RECORD_TOOL_RESULT
            {
                turn.has_tool_records = true;
            }
        }
    }

    let mut kept = 0usize;
    for turn in turns.iter_mut().rev() {
        if turn.completed && turn.has_tool_records && kept < SESSION_COMPACT_TOOL_TURNS {
            turn.keep_tool_records = true;
            kept += 1;
        }
    }

    Ok(turns)
}

/// `session_history_compact_keep_record`.
fn keep_record(turns: &[Turn], turn_index: usize, record_type: i32) -> bool {
    if turn_index >= turns.len() {
        return false;
    }
    if record_type == CONTEXT_RECORD_USER || record_type == CONTEXT_RECORD_ASSISTANT_FINAL {
        return true;
    }
    if turn_index + 1 == turns.len() && !turns[turn_index].completed {
        return true;
    }
    turns[turn_index].keep_tool_records
        && (record_type == CONTEXT_RECORD_ASSISTANT_TOOL || record_type == CONTEXT_RECORD_TOOL_RESULT)
}

/// `session_history_plan_compaction`.
fn plan_compaction(entries: &[IndexEntry], turns: &[Turn]) -> Result<(usize, usize), EspErr> {
    let mut data_size = 0usize;
    let mut entry_count = 0usize;
    let mut turn_index = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        let record_type = entry.record_type as i32;
        while turn_index + 1 < turns.len() && i >= turns[turn_index].end {
            turn_index += 1;
        }
        if !keep_record(turns, turn_index, record_type) {
            continue;
        }
        data_size = match data_size.checked_add(entry.length as usize) {
            Some(v) => v,
            None => return Err(ESP_ERR_INVALID_SIZE),
        };
        entry_count += 1;
    }
    Ok((data_size, entry_count))
}

fn publish_size_warning(request: *const claw_core_request_t, session_id: &str) {
    if request.is_null() {
        log::warn!("session history blocked for {}: {}", session_id, SESSION_SIZE_WARNING);
        return;
    }
    if let Ok(c) = std::ffi::CString::new(SESSION_SIZE_WARNING) {
        let err = unsafe { claw_core_publish_stage_text(request, c.as_ptr()) };
        if err != ESP_OK {
            log::warn!("publish session history size warning failed for {}", session_id);
        }
    }
}

/// `session_history_rewrite_compacted`.
fn rewrite_compacted(
    session_id: &str,
    request: *const claw_core_request_t,
    data_path: &str,
    idx_path: &str,
    entries: &[IndexEntry],
) -> EspErr {
    let turns = match analyze_turns(entries) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let (compacted_data_size, compacted_entry_count) = match plan_compaction(entries, &turns) {
        Ok(v) => v,
        Err(e) => return e,
    };

    if compacted_data_size > SESSION_SIZE_LIMIT {
        let block_err = mark_blocked(data_path);
        if block_err != ESP_OK {
            log::warn!("mark session history blocked failed for {}", session_id);
        }
        publish_size_warning(request, session_id);
        return ESP_OK;
    }

    let original = match fs::read(data_path) {
        Ok(b) => b,
        Err(_) => {
            log::error!("open session history compaction files failed");
            return ESP_FAIL;
        }
    };

    let mut data_file = match fs::OpenOptions::new().read(true).write(true).open(data_path) {
        Ok(f) => f,
        Err(_) => {
            log::error!("open session history compaction files failed");
            return ESP_FAIL;
        }
    };
    let mut idx_file = match fs::OpenOptions::new().read(true).write(true).open(idx_path) {
        Ok(f) => f,
        Err(_) => {
            log::error!("open session history compaction files failed");
            return ESP_FAIL;
        }
    };

    if idx_file.seek(SeekFrom::Start(0)).is_err() || idx_file.write_all(&pack_header()).is_err() {
        log::error!("write session history idx header failed");
        return ESP_FAIL;
    }

    let mut turn_index = 0usize;
    let mut write_offset: u32 = 0;
    for (i, entry) in entries.iter().enumerate() {
        let record_type = entry.record_type as i32;
        while turn_index + 1 < turns.len() && i >= turns[turn_index].end {
            turn_index += 1;
        }
        if !keep_record(&turns, turn_index, record_type) {
            continue;
        }

        let offset = entry.offset as usize;
        let length = entry.length as usize;
        if offset + length > original.len() {
            return ESP_ERR_INVALID_STATE;
        }
        if data_file.seek(SeekFrom::Start(write_offset as u64)).is_err()
            || data_file.write_all(&original[offset..offset + length]).is_err()
        {
            log::error!("seek compacted session history write offset failed");
            return ESP_FAIL;
        }

        let new_entry = IndexEntry {
            offset: write_offset,
            length: entry.length,
            record_type: entry.record_type,
            backend_format: entry.backend_format,
        };
        if idx_file.write_all(&pack_entry(&new_entry)).is_err() {
            return ESP_FAIL;
        }
        write_offset += entry.length;
    }

    if write_offset as usize != compacted_data_size {
        log::warn!(
            "compacted session history size mismatch write_offset={} planned={}",
            write_offset,
            compacted_data_size
        );
        return ESP_ERR_INVALID_STATE;
    }
    if data_file.flush().is_err() || idx_file.flush().is_err() {
        log::error!("flush compacted session history files failed");
        return ESP_FAIL;
    }
    if data_file.set_len(compacted_data_size as u64).is_err() {
        log::error!("truncate compacted session history failed");
        return ESP_FAIL;
    }
    let idx_len = (SESSION_IDX_HEADER_SIZE + compacted_entry_count * SESSION_IDX_ENTRY_SIZE) as u64;
    if idx_file.set_len(idx_len).is_err() {
        log::error!("truncate compacted session history idx failed");
        return ESP_FAIL;
    }
    ESP_OK
}

/// `session_history_compact_if_needed`.
fn compact_if_needed(
    session_id: &str,
    request: *const claw_core_request_t,
    data_path: &str,
    idx_path: &str,
) -> EspErr {
    if util::file_size_bytes(data_path) <= SESSION_SIZE_LIMIT {
        return ESP_OK;
    }
    let entries = match validate_pair(data_path, idx_path) {
        Ok(e) => e,
        Err(_) => {
            log::warn!("Resetting invalid oversized session history {}", data_path);
            return recreate_file(data_path, idx_path);
        }
    };
    rewrite_compacted(session_id, request, data_path, idx_path, &entries)
}

// --- persist callback ------------------------------------------------------

/// A single persist record (mirror `claw_core_context_record_t`).
pub struct PersistRecordIn {
    pub record_type: i32,
    pub message_json: Option<String>,
    pub text: Option<String>,
}

fn validate_batch(records: &[PersistRecordIn]) -> bool {
    if records.is_empty() {
        return false;
    }
    for r in records {
        let has_json = r.message_json.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        let has_text = r.text.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        if !record_type_valid(r.record_type as u8) || (!has_json && !has_text) {
            return false;
        }
    }
    true
}

/// `claw_memory_persist_context_callback`.
pub fn persist_context(
    session_id: Option<&str>,
    records: &[PersistRecordIn],
    turn_completed: bool,
    request: *const claw_core_request_t,
) -> EspErr {
    {
        let st = state::lock();
        if !st.initialized {
            return ESP_ERR_INVALID_STATE;
        }
    }
    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return ESP_ERR_INVALID_ARG,
    };
    if !validate_batch(records) {
        return ESP_ERR_INVALID_ARG;
    }
    if session_blocked(session_id) {
        return ESP_ERR_INVALID_STATE;
    }

    let (data_path, idx_path) = {
        let st = state::lock();
        let dp = session_path_dup(&st, session_id);
        let ip = idx_path_dup(&dp);
        (dp, ip)
    };

    if util::ensure_parent_dir(&data_path) != ESP_OK {
        return ESP_FAIL;
    }

    let (mut data_file, mut idx_file) = match open_pair_for_append(&data_path, &idx_path) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let mut err = ESP_OK;
    {
        let st = state::lock();
        for r in records {
            let has_json = r.message_json.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
            let (role, text) = if !has_json {
                let role = record_text_role(r.record_type);
                let text = r.text.as_deref();
                if role.is_none() || text.map(|s| s.is_empty()).unwrap_or(true) {
                    err = ESP_ERR_INVALID_ARG;
                    break;
                }
                (role, text)
            } else {
                (None, None)
            };
            err = append_indexed_record(
                &st,
                &mut data_file,
                &mut idx_file,
                r.record_type,
                r.message_json.as_deref(),
                role,
                text,
            );
            if err != ESP_OK {
                break;
            }
        }
    }

    // Close files (drop) before compaction reopens them.
    drop(data_file);
    drop(idx_file);

    if err == ESP_OK && turn_completed {
        err = compact_if_needed(session_id, request, &data_path, &idx_path);
    }
    err
}

// --- delete ----------------------------------------------------------------

fn unlink_path(path: &str, deleted_any: &mut bool) -> EspErr {
    if path.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    match fs::remove_file(path) {
        Ok(()) => {
            *deleted_any = true;
            ESP_OK
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ESP_OK,
        Err(_) => {
            log::error!("delete session history artifact {} failed", path);
            ESP_FAIL
        }
    }
}

/// `claw_memory_delete_session_history`.
pub fn delete_session_history(session_id: &str) -> Result<bool, EspErr> {
    if session_id.is_empty() {
        return Err(ESP_ERR_INVALID_ARG);
    }
    let (data_path, idx_path, blocked_path) = {
        let st = state::lock();
        if !st.initialized {
            return Err(ESP_ERR_INVALID_STATE);
        }
        let dp = session_path_dup(&st, session_id);
        let ip = idx_path_dup(&dp);
        let bp = blocked_path_dup(&dp);
        (dp, ip, bp)
    };

    let mut deleted_any = false;
    let err = unlink_path(&data_path, &mut deleted_any);
    if err != ESP_OK {
        return Err(err);
    }
    let err = unlink_path(&idx_path, &mut deleted_any);
    if err != ESP_OK {
        return Err(err);
    }
    let err = unlink_path(&blocked_path, &mut deleted_any);
    if err != ESP_OK {
        return Err(err);
    }
    Ok(deleted_any)
}

// --- gate / start / stage-note ---------------------------------------------

/// `claw_memory_stage_note_callback` content: returns the stage note string.
pub fn stage_note(request_id: u32, session_id: Option<&str>) -> Option<String> {
    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return None,
    };

    let manual_write = request_take_manual_write(request_id);
    let mut summary_list = pending_take_summary_list(session_id);
    let async_summary = crate::extract::async_extract_take_summary_list(request_id, !manual_write);
    if util::line_list_merge_unique(&mut summary_list, async_summary.as_deref()) != ESP_OK {
        log::warn!("merge async extract summary failed for request={}", request_id);
    }
    util::format_update_stage_note(summary_list.as_deref())
}
