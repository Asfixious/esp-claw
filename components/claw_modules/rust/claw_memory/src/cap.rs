//! Memory capability logic: the five `memory_*` tool handlers as pure Rust
//! functions (JSON request parsing / response building). Ports
//! `claw_memory_cap.c`. The `claw_cap` C ABI registration, descriptor/group
//! statics, and execute trampolines live in `claw_capi::memory_cap`.

use serde_json::{Map, Value};

use crate::api;
use crate::consts::MAX_SUMMARIES;
use crate::error::{MemoryError, MemoryResult};
use crate::item::{self, MemItem};
use crate::session;
use crate::util;

/// Pure view of the `claw_cap_call_context_t` fields the memory handlers read.
pub struct CapCtx<'a> {
    pub request_id: u32,
    pub session_id: Option<&'a str>,
}

/// Outcome of a memory capability handler. The C ABI boundary
/// (`claw_capi::errmap::cap_status_esp_err`) maps this to the `esp_err_t` the
/// `claw_cap` execute trampoline returns; `claw_memory` stays `esp_err_t`-free.
pub enum CapStatus {
    /// Success or a handled (model-visible) error reported in the JSON body.
    Ok,
    /// Invalid tool input (`ESP_ERR_INVALID_ARG`).
    InvalidArg,
    /// A backing memory operation failed.
    Failed(MemoryError),
}

// --- helpers --------------------------------------------------------------

fn parse_input(input: Option<&str>) -> Option<Value> {
    serde_json::from_str(input.unwrap_or("{}")).ok()
}

fn get_str<'a>(root: &'a Value, key: &str) -> Option<&'a str> {
    root.get(key).and_then(|v| v.as_str())
}

fn item_from_root(root: &Value) -> MemItem {
    let mut item = MemItem::default();
    if let Some(v) = get_str(root, "memory_id") {
        item.set_id(v);
    }
    if let Some(v) = get_str(root, "source") {
        item.set_source(v);
    }
    if let Some(v) = get_str(root, "content") {
        item.set_content(v);
    }
    if let Some(v) = get_str(root, "tags") {
        item.set_tags(v);
    }
    if let Some(v) = get_str(root, "keywords") {
        item.set_keywords(v);
    }
    item
}

fn primary_summary(item: &MemItem) -> String {
    item::primary_summary_label(item).unwrap_or_default()
}

fn collect_summary_list(item: &MemItem) -> Option<String> {
    let mut list: Option<String> = None;
    let _ = item::append_item_summary_labels(item, &mut list);
    list
}

fn record_pending_summary(ctx: Option<&CapCtx>, summary_list: Option<&str>, changed: bool) {
    let Some(ctx) = ctx else {
        return;
    };
    if !changed {
        return;
    }
    if ctx.request_id != 0 {
        let _ = session::request_mark_manual_write(ctx.request_id);
    }
    let session_id = match ctx.session_id {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let summary_list = match summary_list {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let _ = session::note_session_summary(session_id, summary_list);
}

// --- render helpers (cJSON object construction; preserve_order) -----------

fn render_result(
    action: &str,
    memory_id: Option<&str>,
    summary: Option<&str>,
    changed: bool,
    items_json: Option<&str>,
) -> String {
    let mut root = Map::new();
    root.insert("ok".into(), Value::Bool(true));
    root.insert("action".into(), Value::from(action));
    root.insert("changed".into(), Value::Bool(changed));
    if let Some(id) = memory_id {
        if !id.is_empty() {
            root.insert("memory_id".into(), Value::from(id));
        }
    }
    if let Some(s) = summary {
        if !s.is_empty() {
            root.insert("summary".into(), Value::from(s));
        }
    }
    if let Some(items) = items_json {
        let parsed = serde_json::from_str::<Value>(items).unwrap_or_else(|_| Value::Array(vec![]));
        root.insert("items".into(), parsed);
    }
    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

fn render_error(error: &str) -> String {
    let mut root = Map::new();
    root.insert("ok".into(), Value::Bool(false));
    root.insert("error".into(), Value::from(error));
    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

fn render_invalid_summary_labels(invalid_label: &str, catalog_json: &str) -> String {
    let mut root = Map::new();
    root.insert("ok".into(), Value::Bool(false));
    root.insert("error".into(), Value::from("invalid_summary_labels"));
    root.insert(
        "message".into(),
        Value::from(
            "summary_labels must be copied exactly from the current memory summary label catalog",
        ),
    );
    root.insert("invalid_label".into(), Value::from(invalid_label));
    let catalog = serde_json::from_str::<Value>(catalog_json).unwrap_or_else(|_| Value::Array(vec![]));
    let catalog = if catalog.is_array() { catalog } else { Value::Array(vec![]) };
    root.insert("available_summary_labels".into(), catalog);
    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

// --- summary catalog ------------------------------------------------------

fn json_array_has_string(array: &[Value], value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    array.iter().any(|v| v.as_str() == Some(value))
}

fn collect_summary_catalog() -> MemoryResult<String> {
    let items_json = api::list()?;
    let items: Value = serde_json::from_str(&items_json).unwrap_or(Value::Null);
    let Some(items) = items.as_array() else {
        return Err(MemoryError::Fail);
    };

    let mut catalog: Vec<Value> = Vec::new();
    for item in items {
        let Some(labels) = item.get("summary_labels").and_then(|v| v.as_array()) else {
            continue;
        };
        for label in labels {
            if let Some(text) = label.as_str() {
                if !text.is_empty() && !json_array_has_string(&catalog, text) {
                    catalog.push(Value::from(text));
                }
            }
        }
    }

    if catalog.is_empty() {
        return Ok("[]".to_string());
    }
    serde_json::to_string(&Value::Array(catalog)).map_err(|_| MemoryError::NoMem)
}

fn find_invalid_summary_label(labels: &[String], catalog_json: &str) -> Option<String> {
    if labels.is_empty() {
        return None;
    }
    let catalog: Value = serde_json::from_str(catalog_json).unwrap_or(Value::Null);
    let Some(catalog) = catalog.as_array() else {
        return labels.first().cloned();
    };
    for label in labels {
        if label.is_empty() {
            continue;
        }
        if !json_array_has_string(catalog, label) {
            return Some(label.clone());
        }
    }
    None
}

// --- execute handlers (pure) ----------------------------------------------

/// `memory_store` tool handler. Returns the capability status and the output
/// JSON to write to the caller's buffer.
pub fn cap_store(input: Option<&str>, ctx: Option<&CapCtx>) -> (CapStatus, String) {
    let Some(root) = parse_input(input) else {
        return (CapStatus::InvalidArg, "{\"ok\":false,\"error\":\"invalid_json\"}".to_string());
    };
    let mut item = item_from_root(&root);
    if item.content.is_empty() {
        return (
            CapStatus::InvalidArg,
            "{\"ok\":false,\"error\":\"content is required\"}".to_string(),
        );
    }

    let changed = match api::store_with_result(&mut item) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "{{\"ok\":false,\"error\":\"memory_store_failed\",\"code\":\"{}\"}}",
                e.code_name()
            );
            return (CapStatus::Failed(e), msg);
        }
    };
    let summary = primary_summary(&item);
    let summary_list = collect_summary_list(&item);
    record_pending_summary(ctx, summary_list.as_deref(), changed);
    let rendered = render_result("memory_store", Some(&item.id), Some(&summary), changed, None);
    (CapStatus::Ok, rendered)
}

/// `memory_recall` tool handler.
pub fn cap_recall(input: Option<&str>) -> (CapStatus, String) {
    let Some(root) = parse_input(input) else {
        return (CapStatus::InvalidArg, "{\"ok\":false,\"error\":\"invalid_json\"}".to_string());
    };

    let limit = root
        .get("limit")
        .and_then(|v| v.as_f64())
        .map(|f| if f > 0.0 { f as usize } else { 0 })
        .unwrap_or(0);

    let mut labels: Vec<String> = Vec::new();
    if let Some(arr) = root.get("summary_labels").and_then(|v| v.as_array()) {
        for v in arr {
            if labels.len() >= MAX_SUMMARIES {
                break;
            }
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    labels.push(s.to_string());
                }
            }
        }
    }

    let catalog_json = match collect_summary_catalog() {
        Ok(c) => c,
        Err(_) => {
            return (CapStatus::Ok, render_error("memory_summary_catalog_unavailable"));
        }
    };

    if let Some(invalid) = find_invalid_summary_label(&labels, &catalog_json) {
        return (CapStatus::Ok, render_invalid_summary_labels(&invalid, &catalog_json));
    }

    let items_json = match api::recall(&labels, limit) {
        Ok(j) => j,
        Err(e) => {
            let msg = format!(
                "{{\"ok\":false,\"error\":\"memory_recall_failed\",\"code\":\"{}\"}}",
                e.code_name()
            );
            return (CapStatus::Failed(e), msg);
        }
    };
    let rendered = render_result("memory_recall", None, None, true, Some(items_json.as_str()));
    (CapStatus::Ok, rendered)
}

/// `memory_list` tool handler.
pub fn cap_list() -> (CapStatus, String) {
    let items_json = match api::list() {
        Ok(j) => j,
        Err(e) => {
            let msg = format!(
                "{{\"ok\":false,\"error\":\"memory_list_failed\",\"code\":\"{}\"}}",
                e.code_name()
            );
            return (CapStatus::Failed(e), msg);
        }
    };
    let rendered = render_result("memory_list", None, None, false, Some(items_json.as_str()));
    (CapStatus::Ok, rendered)
}

/// `memory_update` tool handler.
pub fn cap_update(input: Option<&str>, ctx: Option<&CapCtx>) -> (CapStatus, String) {
    let Some(root) = parse_input(input) else {
        return (CapStatus::InvalidArg, "{\"ok\":false,\"error\":\"invalid_json\"}".to_string());
    };
    let mut item = item_from_root(&root);
    if item.id.is_empty() {
        return (
            CapStatus::InvalidArg,
            "{\"ok\":false,\"error\":\"memory_id is required\"}".to_string(),
        );
    }

    let changed = match api::update_with_result(&mut item) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "{{\"ok\":false,\"error\":\"memory_update_failed\",\"code\":\"{}\"}}",
                e.code_name()
            );
            return (CapStatus::Failed(e), msg);
        }
    };
    let summary = primary_summary(&item);
    let summary_list = collect_summary_list(&item);
    record_pending_summary(ctx, summary_list.as_deref(), changed);
    let rendered = render_result("memory_update", Some(&item.id), Some(&summary), changed, None);
    (CapStatus::Ok, rendered)
}

/// `memory_forget` tool handler.
pub fn cap_forget(input: Option<&str>, ctx: Option<&CapCtx>) -> (CapStatus, String) {
    let Some(root) = parse_input(input) else {
        return (CapStatus::InvalidArg, "{\"ok\":false,\"error\":\"invalid_json\"}".to_string());
    };
    let memory_id = match get_str(&root, "memory_id") {
        Some(s) if !s.is_empty() => util::safe_copy(s, crate::consts::ID_CAP),
        _ => {
            return (
                CapStatus::InvalidArg,
                "{\"ok\":false,\"error\":\"memory_id is required\"}".to_string(),
            );
        }
    };

    let (out_item, changed) = match api::forget_with_result(&memory_id, true) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!(
                "{{\"ok\":false,\"error\":\"memory_forget_failed\",\"code\":\"{}\"}}",
                e.code_name()
            );
            return (CapStatus::Failed(e), msg);
        }
    };
    let summary = out_item.as_ref().map(primary_summary).unwrap_or_default();
    let summary_list = out_item.as_ref().and_then(collect_summary_list);
    record_pending_summary(ctx, summary_list.as_deref(), changed);
    let rendered = render_result("memory_forget", Some(&memory_id), Some(&summary), changed, None);
    (CapStatus::Ok, rendered)
}
