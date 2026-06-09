//! Memory capability registration via `claw_cap` (still C) plus the five
//! `memory_*` tool execute handlers. Ports `claw_memory_cap.c`.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use serde_json::{Map, Value};

use claw_core::errname::esp_err_to_name;
use claw_interfaces::error::{EspErr, ESP_FAIL, ESP_OK};

use crate::api;
use crate::consts::MAX_SUMMARIES;
use crate::item::{self, MemItem};
use crate::session;
use crate::util;

// --- claw_cap.h C ABI -----------------------------------------------------

type ClawCapExecuteFn = unsafe extern "C" fn(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr;
type ClawCapLifecycleFn = unsafe extern "C" fn() -> EspErr;

#[repr(C)]
pub struct claw_cap_call_context_t {
    pub request_id: u32,
    pub session_id: *const c_char,
    pub channel: *const c_char,
    pub chat_id: *const c_char,
    pub target_channel: *const c_char,
    pub target_chat_id: *const c_char,
    pub source_cap: *const c_char,
    pub correlation_id: *const c_char,
    pub core: *mut c_void,
    pub caller: c_int,
}

#[repr(C)]
struct claw_cap_descriptor_t {
    id: *const c_char,
    name: *const c_char,
    family: *const c_char,
    description: *const c_char,
    kind: c_int,
    cap_flags: u32,
    input_schema_json: *const c_char,
    init: Option<ClawCapLifecycleFn>,
    start: Option<ClawCapLifecycleFn>,
    stop: Option<ClawCapLifecycleFn>,
    execute: Option<ClawCapExecuteFn>,
}

#[repr(C)]
struct claw_cap_group_t {
    group_id: *const c_char,
    plugin_name: *const c_char,
    version: *const c_char,
    descriptors: *const claw_cap_descriptor_t,
    descriptor_count: usize,
    plugin_ctx: *mut c_void,
    group_init: Option<ClawCapLifecycleFn>,
    group_start: Option<ClawCapLifecycleFn>,
    group_stop: Option<ClawCapLifecycleFn>,
}

const CLAW_CAP_KIND_CALLABLE: c_int = 0;
const CLAW_CAP_FLAG_CALLABLE_BY_LLM: u32 = 1 << 0;

extern "C" {
    fn claw_cap_register_group(group: *const claw_cap_group_t) -> EspErr;
    fn claw_cap_group_exists(group_id: *const c_char) -> bool;
}

// --- descriptor / group statics -------------------------------------------

const GROUP_ID: &[u8] = b"claw_memory\0";

const STORE_ID: &[u8] = b"memory_store\0";
const STORE_FAMILY: &[u8] = b"memory\0";
const STORE_DESC: &[u8] = b"Store one on-device long-term memory item. Prefer normalized content plus precise LLM-generated tags and keywords.\0";
const STORE_SCHEMA: &[u8] = b"{\"type\":\"object\",\"properties\":{\"memory_id\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"},\"tags\":{\"type\":\"string\"},\"keywords\":{\"type\":\"string\"}},\"required\":[\"content\"]}\0";

const RECALL_ID: &[u8] = b"memory_recall\0";
const RECALL_DESC: &[u8] =
    b"Recall detailed long-term memories by exact summary_labels from the current catalog.\0";
const RECALL_SCHEMA: &[u8] = b"{\"type\":\"object\",\"properties\":{\"summary_labels\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"limit\":{\"type\":\"integer\"}}}\0";

const LIST_ID: &[u8] = b"memory_list\0";
const LIST_DESC: &[u8] = b"List active long-term memories.\0";
const LIST_SCHEMA: &[u8] = b"{\"type\":\"object\",\"properties\":{}}\0";

const UPDATE_ID: &[u8] = b"memory_update\0";
const UPDATE_DESC: &[u8] = b"Update one long-term memory item. Prefer normalized content plus precise LLM-generated tags and keywords.\0";
const UPDATE_SCHEMA: &[u8] = b"{\"type\":\"object\",\"properties\":{\"memory_id\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"},\"tags\":{\"type\":\"string\"},\"keywords\":{\"type\":\"string\"}},\"required\":[\"memory_id\"]}\0";

const FORGET_ID: &[u8] = b"memory_forget\0";
const FORGET_DESC: &[u8] = b"Forget one long-term memory item by exact memory_id.\0";
const FORGET_SCHEMA: &[u8] =
    b"{\"type\":\"object\",\"properties\":{\"memory_id\":{\"type\":\"string\"}},\"required\":[\"memory_id\"]}\0";

struct GroupSync(claw_cap_group_t);
unsafe impl Sync for GroupSync {}
struct DescriptorsSync([claw_cap_descriptor_t; 5]);
unsafe impl Sync for DescriptorsSync {}

const fn descriptor(
    id: &'static [u8],
    family: &'static [u8],
    desc: &'static [u8],
    schema: &'static [u8],
    execute: ClawCapExecuteFn,
) -> claw_cap_descriptor_t {
    claw_cap_descriptor_t {
        id: id.as_ptr() as *const c_char,
        name: id.as_ptr() as *const c_char,
        family: family.as_ptr() as *const c_char,
        description: desc.as_ptr() as *const c_char,
        kind: CLAW_CAP_KIND_CALLABLE,
        cap_flags: CLAW_CAP_FLAG_CALLABLE_BY_LLM,
        input_schema_json: schema.as_ptr() as *const c_char,
        init: None,
        start: None,
        stop: None,
        execute: Some(execute),
    }
}

static DESCRIPTORS: DescriptorsSync = DescriptorsSync([
    descriptor(STORE_ID, STORE_FAMILY, STORE_DESC, STORE_SCHEMA, cap_memory_store_execute),
    descriptor(RECALL_ID, STORE_FAMILY, RECALL_DESC, RECALL_SCHEMA, cap_memory_recall_execute),
    descriptor(LIST_ID, STORE_FAMILY, LIST_DESC, LIST_SCHEMA, cap_memory_list_execute),
    descriptor(UPDATE_ID, STORE_FAMILY, UPDATE_DESC, UPDATE_SCHEMA, cap_memory_update_execute),
    descriptor(FORGET_ID, STORE_FAMILY, FORGET_DESC, FORGET_SCHEMA, cap_memory_forget_execute),
]);

static GROUP: GroupSync = GroupSync(claw_cap_group_t {
    group_id: GROUP_ID.as_ptr() as *const c_char,
    plugin_name: ptr::null(),
    version: ptr::null(),
    descriptors: DESCRIPTORS.0.as_ptr(),
    descriptor_count: 5,
    plugin_ctx: ptr::null_mut(),
    group_init: None,
    group_start: None,
    group_stop: None,
});

// --- helpers --------------------------------------------------------------

unsafe fn write_output(output: *mut c_char, output_size: usize, s: &str) {
    if output.is_null() || output_size == 0 {
        return;
    }
    let bytes = s.as_bytes();
    let n = bytes.len().min(output_size - 1);
    ptr::copy_nonoverlapping(bytes.as_ptr(), output as *mut u8, n);
    *output.add(n) = 0;
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        core::ffi::CStr::from_ptr(p).to_str().ok()
    }
}

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

fn record_pending_summary(ctx: *const claw_cap_call_context_t, summary_list: Option<&str>, changed: bool) {
    if ctx.is_null() || !changed {
        return;
    }
    let c = unsafe { &*ctx };
    if c.request_id != 0 {
        let _ = session::request_mark_manual_write(c.request_id);
    }
    let session_id = unsafe { cstr(c.session_id) };
    let session_id = match session_id {
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

fn collect_summary_catalog() -> Result<String, EspErr> {
    let (err, items_json) = api::list();
    if err != ESP_OK {
        return Err(err);
    }
    let items_json = items_json.unwrap_or_else(|| "[]".to_string());
    let items: Value = serde_json::from_str(&items_json).unwrap_or(Value::Null);
    let items = match items.as_array() {
        Some(a) => a,
        None => return Err(ESP_FAIL),
    };

    let mut catalog: Vec<Value> = Vec::new();
    for item in items {
        let labels = match item.get("summary_labels").and_then(|v| v.as_array()) {
            Some(l) => l,
            None => continue,
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
    serde_json::to_string(&Value::Array(catalog)).map_err(|_| claw_interfaces::error::ESP_ERR_NO_MEM)
}

fn find_invalid_summary_label(labels: &[String], catalog_json: &str) -> Option<String> {
    if labels.is_empty() {
        return None;
    }
    let catalog: Value = serde_json::from_str(catalog_json).unwrap_or(Value::Null);
    let catalog = match catalog.as_array() {
        Some(a) => a,
        None => return labels.first().cloned(),
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

// --- execute handlers -----------------------------------------------------

unsafe extern "C" fn cap_memory_store_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let root = match parse_input(cstr(input_json)) {
        Some(v) => v,
        None => {
            write_output(output, output_size, "{\"ok\":false,\"error\":\"invalid_json\"}");
            return claw_interfaces::error::ESP_ERR_INVALID_ARG;
        }
    };
    let mut item = item_from_root(&root);
    if item.content.is_empty() {
        write_output(output, output_size, "{\"ok\":false,\"error\":\"content is required\"}");
        return claw_interfaces::error::ESP_ERR_INVALID_ARG;
    }

    let (err, changed) = api::store_with_result(&mut item);
    if err != ESP_OK {
        let msg = format!(
            "{{\"ok\":false,\"error\":\"memory_store_failed\",\"code\":\"{}\"}}",
            esp_err_to_name(err)
        );
        write_output(output, output_size, &msg);
        return err;
    }
    let summary = primary_summary(&item);
    let summary_list = collect_summary_list(&item);
    record_pending_summary(ctx, summary_list.as_deref(), changed);
    let rendered = render_result("memory_store", Some(&item.id), Some(&summary), changed, None);
    write_output(output, output_size, &rendered);
    ESP_OK
}

unsafe extern "C" fn cap_memory_recall_execute(
    input_json: *const c_char,
    _ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let root = match parse_input(cstr(input_json)) {
        Some(v) => v,
        None => {
            write_output(output, output_size, "{\"ok\":false,\"error\":\"invalid_json\"}");
            return claw_interfaces::error::ESP_ERR_INVALID_ARG;
        }
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
            write_output(output, output_size, &render_error("memory_summary_catalog_unavailable"));
            return ESP_OK;
        }
    };

    if let Some(invalid) = find_invalid_summary_label(&labels, &catalog_json) {
        write_output(output, output_size, &render_invalid_summary_labels(&invalid, &catalog_json));
        return ESP_OK;
    }

    let (err, items_json) = api::recall(&labels, limit);
    if err != ESP_OK {
        let msg = format!(
            "{{\"ok\":false,\"error\":\"memory_recall_failed\",\"code\":\"{}\"}}",
            esp_err_to_name(err)
        );
        write_output(output, output_size, &msg);
        return err;
    }
    let rendered = render_result("memory_recall", None, None, true, items_json.as_deref());
    write_output(output, output_size, &rendered);
    ESP_OK
}

unsafe extern "C" fn cap_memory_list_execute(
    _input_json: *const c_char,
    _ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let (err, items_json) = api::list();
    if err != ESP_OK {
        let msg = format!(
            "{{\"ok\":false,\"error\":\"memory_list_failed\",\"code\":\"{}\"}}",
            esp_err_to_name(err)
        );
        write_output(output, output_size, &msg);
        return err;
    }
    let rendered = render_result("memory_list", None, None, false, items_json.as_deref());
    write_output(output, output_size, &rendered);
    ESP_OK
}

unsafe extern "C" fn cap_memory_update_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let root = match parse_input(cstr(input_json)) {
        Some(v) => v,
        None => {
            write_output(output, output_size, "{\"ok\":false,\"error\":\"invalid_json\"}");
            return claw_interfaces::error::ESP_ERR_INVALID_ARG;
        }
    };
    let mut item = item_from_root(&root);
    if item.id.is_empty() {
        write_output(output, output_size, "{\"ok\":false,\"error\":\"memory_id is required\"}");
        return claw_interfaces::error::ESP_ERR_INVALID_ARG;
    }

    let (err, changed) = api::update_with_result(&mut item);
    if err != ESP_OK {
        let msg = format!(
            "{{\"ok\":false,\"error\":\"memory_update_failed\",\"code\":\"{}\"}}",
            esp_err_to_name(err)
        );
        write_output(output, output_size, &msg);
        return err;
    }
    let summary = primary_summary(&item);
    let summary_list = collect_summary_list(&item);
    record_pending_summary(ctx, summary_list.as_deref(), changed);
    let rendered = render_result("memory_update", Some(&item.id), Some(&summary), changed, None);
    write_output(output, output_size, &rendered);
    ESP_OK
}

unsafe extern "C" fn cap_memory_forget_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let root = match parse_input(cstr(input_json)) {
        Some(v) => v,
        None => {
            write_output(output, output_size, "{\"ok\":false,\"error\":\"invalid_json\"}");
            return claw_interfaces::error::ESP_ERR_INVALID_ARG;
        }
    };
    let memory_id = match get_str(&root, "memory_id") {
        Some(s) if !s.is_empty() => util::safe_copy(s, crate::consts::ID_CAP),
        _ => {
            write_output(output, output_size, "{\"ok\":false,\"error\":\"memory_id is required\"}");
            return claw_interfaces::error::ESP_ERR_INVALID_ARG;
        }
    };

    let (err, out_item, changed) = api::forget_with_result(&memory_id, true);
    if err != ESP_OK {
        let msg = format!(
            "{{\"ok\":false,\"error\":\"memory_forget_failed\",\"code\":\"{}\"}}",
            esp_err_to_name(err)
        );
        write_output(output, output_size, &msg);
        return err;
    }
    let summary = out_item.as_ref().map(primary_summary).unwrap_or_default();
    let summary_list = out_item.as_ref().and_then(collect_summary_list);
    record_pending_summary(ctx, summary_list.as_deref(), changed);
    let rendered = render_result("memory_forget", Some(&memory_id), Some(&summary), changed, None);
    write_output(output, output_size, &rendered);
    ESP_OK
}

// --- register -------------------------------------------------------------

/// `claw_memory_register_group`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_register_group() -> EspErr {
    if claw_cap_group_exists(GROUP.0.group_id) {
        return ESP_OK;
    }
    claw_cap_register_group(&GROUP.0)
}
