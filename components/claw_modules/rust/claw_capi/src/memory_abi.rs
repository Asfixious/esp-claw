//! C ABI for `claw_memory` (moved from `claw_memory::cabi`).
//!
//! Exports the `claw_memory.h` ABI as `#[no_mangle] extern "C"` plus the four
//! context-provider statics, the by-value `claw_memory_item_t` C struct, and the
//! `claw_memory_item_t <-> MemItem` conversions. C callers link against these
//! symbols unchanged; the pure Rust logic lives in `claw_memory`.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::core_abi::{
    claw_core_context_persist_batch_t, claw_core_context_provider_t, claw_core_context_t,
    claw_core_request_t,
};
use claw_platform::{
    esp_err_t as EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND,
    ESP_ERR_NO_MEM, ESP_OK,
};

use claw_memory::api::{self, LlmConfig, MemoryConfig};
use claw_memory::consts::{CONTEXT_KIND_MESSAGES, CONTEXT_KIND_SYSTEM_PROMPT, MAX_SUMMARIES};
use claw_memory::error::MemoryResult;
use claw_memory::item::MemItem;
use claw_memory::session::{self, PersistRecordIn};
use claw_memory::util;
use claw_memory::{extract, lightweight, profile};

// --- C-string interop -----------------------------------------------------

/// Read a `*const c_char` into an owned `String` (lossy), or `None` if null.
unsafe fn cstr_to_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(core::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

/// Write `s` into the C buffer with NUL termination, truncating to fit
/// (`strlcpy`/`snprintf("%s")` semantics).
unsafe fn write_c_buf(dst: *mut c_char, dst_size: usize, s: &str) {
    if dst.is_null() || dst_size == 0 {
        return;
    }
    let bytes = s.as_bytes();
    let n = bytes.len().min(dst_size - 1);
    // Back off to a char boundary to avoid emitting a partial sequence.
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, end);
    *dst.add(end) = 0;
}

// --- claw_memory_item_t C ABI struct + conversions ------------------------

/// The exact `claw_memory_item_t` C ABI layout (`include/claw_memory.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct claw_memory_item_t {
    pub id: [c_char; 40],
    pub source: [c_char; 16],
    pub content: [c_char; 256],
    pub summary_ids: [u16; 3],
    pub summary_id_count: u8,
    pub tags: [c_char; 96],
    pub keywords: [c_char; 128],
    pub created_at: u32,
    pub updated_at: u32,
    pub access_count: u16,
    pub deleted: u8,
}

impl Default for claw_memory_item_t {
    fn default() -> Self {
        claw_memory_item_t {
            id: [0; 40],
            source: [0; 16],
            content: [0; 256],
            summary_ids: [0; 3],
            summary_id_count: 0,
            tags: [0; 96],
            keywords: [0; 128],
            created_at: 0,
            updated_at: 0,
            access_count: 0,
            deleted: 0,
        }
    }
}

fn read_carr(arr: &[c_char]) -> String {
    let bytes: Vec<u8> = arr
        .iter()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn write_carr(arr: &mut [c_char], s: &str) {
    arr.fill(0);
    let cap = arr.len();
    if cap == 0 {
        return;
    }
    let truncated = util::safe_copy(s, cap);
    for (slot, &b) in arr.iter_mut().zip(truncated.as_bytes()) {
        *slot = b as c_char;
    }
}

/// Build a [`MemItem`] from the by-value C ABI struct.
fn mem_from_c(c: &claw_memory_item_t) -> MemItem {
    let count = (c.summary_id_count as usize).min(MAX_SUMMARIES);
    let summary_ids = c.summary_ids[..count].to_vec();
    MemItem {
        id: read_carr(&c.id),
        source: read_carr(&c.source),
        content: read_carr(&c.content),
        summary_ids,
        tags: read_carr(&c.tags),
        keywords: read_carr(&c.keywords),
        created_at: c.created_at,
        updated_at: c.updated_at,
        access_count: c.access_count,
        deleted: c.deleted != 0,
    }
}

/// Serialize a [`MemItem`] into the by-value C ABI struct.
fn mem_to_c(m: &MemItem) -> claw_memory_item_t {
    let mut c = claw_memory_item_t::default();
    write_carr(&mut c.id, &m.id);
    write_carr(&mut c.source, &m.source);
    write_carr(&mut c.content, &m.content);
    write_carr(&mut c.tags, &m.tags);
    write_carr(&mut c.keywords, &m.keywords);
    let count = m.summary_ids.len().min(MAX_SUMMARIES);
    c.summary_ids[..count].copy_from_slice(&m.summary_ids[..count]);
    c.summary_id_count = count as u8;
    c.created_at = m.created_at;
    c.updated_at = m.updated_at;
    c.access_count = m.access_count;
    c.deleted = u8::from(m.deleted);
    c
}

// --- config / query C ABI structs -----------------------------------------

#[repr(C)]
pub struct claw_memory_llm_config_t {
    pub api_key: *const c_char,
    pub backend_type: *const c_char,
    pub model: *const c_char,
    pub base_url: *const c_char,
    pub auth_type: *const c_char,
    pub max_tokens_field: *const c_char,
    pub timeout_ms: u32,
    pub max_tokens: u32,
    pub image_max_bytes: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub image_remote_url_only: bool,
}

#[repr(C)]
pub struct claw_memory_config_t {
    pub session_root_dir: *const c_char,
    pub memory_root_dir: *const c_char,
    pub max_message_chars: usize,
    pub max_tool_iterations: u32,
    pub llm: claw_memory_llm_config_t,
    pub enable_async_extract_stage_note: bool,
}

#[repr(C)]
pub struct claw_memory_query_t {
    pub summary_labels: *const *const c_char,
    pub summary_label_count: usize,
    pub limit: usize,
}

// --- init ------------------------------------------------------------------

/// `claw_memory_init`.
///
/// # Safety
/// `config` must be null or a valid pointer whose C string fields are null or
/// NUL-terminated and valid for reads.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_init(config: *const claw_memory_config_t) -> EspErr {
    if config.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let c = &*config;
    let llm = &c.llm;

    let cfg = MemoryConfig {
        session_root_dir: cstr_to_string(c.session_root_dir),
        memory_root_dir: cstr_to_string(c.memory_root_dir),
        max_message_chars: c.max_message_chars,
        max_tool_iterations: c.max_tool_iterations,
        llm: LlmConfig {
            api_key: cstr_to_string(llm.api_key),
            backend_type: cstr_to_string(llm.backend_type),
            model: cstr_to_string(llm.model),
            base_url: cstr_to_string(llm.base_url),
            auth_type: cstr_to_string(llm.auth_type),
            max_tokens_field: cstr_to_string(llm.max_tokens_field),
            timeout_ms: llm.timeout_ms,
            max_tokens: llm.max_tokens,
            image_max_bytes: llm.image_max_bytes,
            supports_tools: llm.supports_tools,
            supports_vision: llm.supports_vision,
            image_remote_url_only: llm.image_remote_url_only,
        },
        enable_async_extract_stage_note: c.enable_async_extract_stage_note,
    };
    match api::init(&cfg) {
        Ok(()) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

// --- store / update / forget -----------------------------------------------

/// `claw_memory_store`.
///
/// # Safety
/// `item` must be null or a valid pointer to a `claw_memory_item_t`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_store(item: *const claw_memory_item_t) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = mem_from_c(&*item);
    match api::store_with_result(&mut mem) {
        Ok(_changed) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

/// `claw_memory_store_with_result`.
///
/// # Safety
/// `item` must be null or a valid, writable pointer; `out_changed` must be null
/// or a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_store_with_result(
    item: *mut claw_memory_item_t,
    out_changed: *mut bool,
) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = mem_from_c(&*item);
    match api::store_with_result(&mut mem) {
        Ok(changed) => {
            *item = mem_to_c(&mem);
            if !out_changed.is_null() {
                *out_changed = changed;
            }
            ESP_OK
        }
        Err(e) => {
            // Preserve the original (changed = false) write on the error path.
            if !out_changed.is_null() {
                *out_changed = false;
            }
            crate::errmap::memory_esp_err(e)
        }
    }
}

/// `claw_memory_update`.
///
/// # Safety
/// `item` must be null or a valid pointer to a `claw_memory_item_t`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_update(item: *const claw_memory_item_t) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = mem_from_c(&*item);
    match api::update_with_result(&mut mem) {
        Ok(_changed) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

/// `claw_memory_update_with_result`.
///
/// # Safety
/// `item` must be null or a valid, writable pointer; `out_changed` must be null
/// or a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_update_with_result(
    item: *mut claw_memory_item_t,
    out_changed: *mut bool,
) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = mem_from_c(&*item);
    match api::update_with_result(&mut mem) {
        Ok(changed) => {
            *item = mem_to_c(&mem);
            if !out_changed.is_null() {
                *out_changed = changed;
            }
            ESP_OK
        }
        Err(e) => {
            // Preserve the original (changed = false) write on the error path.
            if !out_changed.is_null() {
                *out_changed = false;
            }
            crate::errmap::memory_esp_err(e)
        }
    }
}

/// `claw_memory_forget`.
///
/// # Safety
/// `memory_id` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_forget(memory_id: *const c_char) -> EspErr {
    let Some(id) = cstr_to_string(memory_id) else {
        return ESP_ERR_INVALID_ARG;
    };
    match api::forget_with_result(&id, false) {
        Ok(_) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

/// `claw_memory_forget_with_result`.
///
/// # Safety
/// `memory_id` must be null or a valid NUL-terminated C string; `out_item` and
/// `out_changed` must be null or valid, writable pointers.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_forget_with_result(
    memory_id: *const c_char,
    out_item: *mut claw_memory_item_t,
    out_changed: *mut bool,
) -> EspErr {
    let Some(id) = cstr_to_string(memory_id) else {
        return ESP_ERR_INVALID_ARG;
    };
    match api::forget_with_result(&id, !out_item.is_null()) {
        Ok((item, changed)) => {
            if let Some(it) = item {
                if !out_item.is_null() {
                    *out_item = mem_to_c(&it);
                }
            }
            if !out_changed.is_null() {
                *out_changed = changed;
            }
            ESP_OK
        }
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

// --- recall / list ---------------------------------------------------------

/// `claw_memory_recall`.
///
/// # Safety
/// `query` must be null or a valid pointer; `out_json` must be null or a valid,
/// writable pointer. The query's `summary_labels` array (if set) must be valid
/// for `summary_label_count` NUL-terminated string reads.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_recall(
    query: *const claw_memory_query_t,
    out_json: *mut *mut c_char,
) -> EspErr {
    if query.is_null() || out_json.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    *out_json = ptr::null_mut();
    let q = &*query;

    let mut labels: Vec<String> = Vec::new();
    if !q.summary_labels.is_null() {
        let count = q.summary_label_count.min(MAX_SUMMARIES);
        let raw = core::slice::from_raw_parts(q.summary_labels, count);
        labels.extend(raw.iter().filter_map(|&p| cstr_to_string(p)));
    }

    let json = match api::recall(&labels, q.limit) {
        Ok(j) => j,
        Err(e) => return crate::errmap::memory_esp_err(e),
    };
    write_out_json(out_json, &json)
}

/// `claw_memory_list`.
///
/// # Safety
/// `out_json` must be null or a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_list(out_json: *mut *mut c_char) -> EspErr {
    if out_json.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    *out_json = ptr::null_mut();
    let json = match api::list() {
        Ok(j) => j,
        Err(e) => return crate::errmap::memory_esp_err(e),
    };
    write_out_json(out_json, &json)
}

unsafe fn write_out_json(out_json: *mut *mut c_char, json: &str) -> EspErr {
    let p = crate::ffi::c_strdup(json);
    if p.is_null() {
        return ESP_ERR_NO_MEM;
    }
    *out_json = p;
    ESP_OK
}

// --- primary summary label -------------------------------------------------

/// `claw_memory_item_primary_summary_label`.
///
/// # Safety
/// `item` must be null or a valid pointer; `buf` must be null or valid for
/// `size` bytes of writes.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_item_primary_summary_label(
    item: *const claw_memory_item_t,
    buf: *mut c_char,
    size: usize,
) -> EspErr {
    if item.is_null() || buf.is_null() || size == 0 {
        return ESP_ERR_INVALID_ARG;
    }
    *buf = 0;
    let mem = mem_from_c(&*item);
    match api::item_primary_summary_label(&mem) {
        Some(label) => {
            write_c_buf(buf, size, &label);
            ESP_OK
        }
        None => ESP_ERR_NOT_FOUND,
    }
}

// --- session / note --------------------------------------------------------

/// `claw_memory_note_session_summary`.
///
/// # Safety
/// `session_id` and `summary_list` must be null or valid NUL-terminated C
/// strings.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_note_session_summary(
    session_id: *const c_char,
    summary_list: *const c_char,
) -> EspErr {
    let Some(session_id) = cstr_to_string(session_id) else {
        return ESP_OK;
    };
    let summary_list = cstr_to_string(summary_list).unwrap_or_default();
    match session::note_session_summary(&session_id, &summary_list) {
        Ok(()) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

/// `claw_memory_request_mark_manual_write`.
///
/// # Safety
/// Safe to call with any `request_id`; declared `unsafe extern "C"` to match the
/// `claw_memory.h` ABI signature.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_request_mark_manual_write(request_id: u32) -> EspErr {
    match session::request_mark_manual_write(request_id) {
        Ok(()) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

/// `claw_memory_delete_session_history`.
///
/// # Safety
/// `session_id` must be null or a valid NUL-terminated C string; `out_deleted_any`
/// must be null or a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_delete_session_history(
    session_id: *const c_char,
    out_deleted_any: *mut bool,
) -> EspErr {
    if out_deleted_any.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    *out_deleted_any = false;
    let session_id = match cstr_to_string(session_id) {
        Some(s) if !s.is_empty() => s,
        _ => return ESP_ERR_INVALID_ARG,
    };
    match session::delete_session_history(&session_id) {
        Ok(deleted) => {
            *out_deleted_any = deleted;
            ESP_OK
        }
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

// --- core callbacks --------------------------------------------------------

/// `claw_memory_persist_context_callback`.
///
/// # Safety
/// `batch` must be null or a valid pointer with NUL-terminated string fields and
/// a `records` array valid for `record_count` entries; `batch.request` must be
/// null or a valid pointer with NUL-terminated string fields.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_persist_context_callback(
    batch: *const claw_core_context_persist_batch_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    if batch.is_null() {
        return match session::persist_context(None, &[], false, None) {
            Ok(()) => ESP_OK,
            Err(e) => crate::errmap::memory_esp_err(e),
        };
    }
    let b = &*batch;
    let session_id = cstr_to_string(b.session_id);

    let records: Vec<PersistRecordIn> = if b.records.is_null() {
        Vec::new()
    } else {
        core::slice::from_raw_parts(b.records, b.record_count)
            .iter()
            .map(|r| PersistRecordIn {
                record_type: r.r#type,
                message_json: cstr_to_string(r.message_json),
                text: cstr_to_string(r.text),
            })
            .collect()
    };

    // Reproduce the old C `claw_core_publish_stage_text(request, text)` path: a
    // non-null request publishes via the router (espidf) and the size warning is
    // suppressed only on publish failure; a null request takes the warn path.
    if b.request.is_null() {
        match session::persist_context(session_id.as_deref(), &records, b.turn_completed, None) {
            Ok(()) => ESP_OK,
            Err(e) => crate::errmap::memory_esp_err(e),
        }
    } else {
        let req = crate::core_abi::request_from_c(b.request);
        let publish = move |text: &str| {
            crate::core_abi::publish_stage_text_impl(&req, text) == claw_platform::ESP_OK
        };
        let publish_dyn: &dyn Fn(&str) -> bool = &publish;
        match session::persist_context(
            session_id.as_deref(),
            &records,
            b.turn_completed,
            Some(publish_dyn),
        ) {
            Ok(()) => ESP_OK,
            Err(e) => crate::errmap::memory_esp_err(e),
        }
    }
}

/// `claw_memory_request_gate_callback`.
///
/// # Safety
/// `request` must be null or a valid pointer with NUL-terminated string fields;
/// `reject_message` must be null or valid for `reject_message_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_request_gate_callback(
    request: *const claw_core_request_t,
    reject_message: *mut c_char,
    reject_message_size: usize,
    _user_ctx: *mut c_void,
) -> EspErr {
    if request.is_null() {
        return ESP_OK;
    }
    let session_id = cstr_to_string((*request).session_id);
    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return ESP_OK,
    };
    if reject_message.is_null() || reject_message_size == 0 {
        return ESP_ERR_INVALID_ARG;
    }
    *reject_message = 0;

    if !session::session_blocked(&session_id) {
        return ESP_OK;
    }
    write_c_buf(
        reject_message,
        reject_message_size,
        claw_memory::consts::SESSION_SIZE_WARNING,
    );
    ESP_ERR_INVALID_STATE
}

/// `claw_memory_request_start_callback`.
///
/// # Safety
/// `request` must be null or a valid pointer with NUL-terminated string fields.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_request_start_callback(
    request: *const claw_core_request_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    if request.is_null() {
        return ESP_OK;
    }
    let r = &*request;
    let session_id = cstr_to_string(r.session_id);
    let user_text = cstr_to_string(r.user_text);
    match extract::async_extract_ensure_started(
        r.request_id,
        session_id.as_deref(),
        user_text.as_deref(),
    ) {
        Ok(()) => ESP_OK,
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

/// `claw_memory_stage_note_callback`.
///
/// # Safety
/// `request` must be null or a valid pointer; `out_note` must be null or a valid,
/// writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_stage_note_callback(
    request: *const claw_core_request_t,
    out_note: *mut *mut c_char,
    _user_ctx: *mut c_void,
) -> EspErr {
    if out_note.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    *out_note = ptr::null_mut();
    if request.is_null() {
        return ESP_OK;
    }
    let r = &*request;
    let session_id = cstr_to_string(r.session_id);
    if let Some(note) = session::stage_note(r.request_id, session_id.as_deref()) {
        let p = crate::ffi::c_strdup(&note);
        if p.is_null() {
            return ESP_ERR_NO_MEM;
        }
        *out_note = p;
    }
    ESP_OK
}

// --- context providers -----------------------------------------------------

#[repr(transparent)]
pub struct ProviderSync(claw_core_context_provider_t);
unsafe impl Sync for ProviderSync {}

unsafe fn finish_context(
    out_context: *mut claw_core_context_t,
    kind: c_int,
    content: MemoryResult<String>,
) -> EspErr {
    if out_context.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    (*out_context).kind = 0;
    (*out_context).content = ptr::null_mut();
    match content {
        Ok(s) => {
            let p = crate::ffi::c_strdup(&s);
            if p.is_null() {
                return ESP_ERR_NO_MEM;
            }
            (*out_context).kind = kind;
            (*out_context).content = p;
            ESP_OK
        }
        Err(e) => crate::errmap::memory_esp_err(e),
    }
}

unsafe extern "C" fn profile_collect(
    _request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    finish_context(
        out_context,
        CONTEXT_KIND_SYSTEM_PROMPT,
        profile::profile_collect_content(),
    )
}

unsafe extern "C" fn long_term_collect(
    _request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    finish_context(
        out_context,
        CONTEXT_KIND_SYSTEM_PROMPT,
        api::long_term_collect_content(),
    )
}

unsafe extern "C" fn long_term_lightweight_collect(
    _request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    finish_context(
        out_context,
        CONTEXT_KIND_SYSTEM_PROMPT,
        lightweight::lightweight_collect_content(),
    )
}

unsafe extern "C" fn session_history_collect(
    request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    if out_context.is_null() {
        return ESP_ERR_NOT_FOUND;
    }
    let session_id = if request.is_null() {
        None
    } else {
        cstr_to_string((*request).session_id)
    };
    finish_context(
        out_context,
        CONTEXT_KIND_MESSAGES,
        session::session_history_collect_content(session_id.as_deref()),
    )
}

const PROFILE_NAME: &[u8] = b"Editable Profile Memory\0";
const LONG_TERM_NAME: &[u8] = b"Long-term Memory\0";
const SESSION_NAME: &[u8] = b"Session History\0";

#[no_mangle]
pub static claw_memory_profile_provider: ProviderSync =
    ProviderSync(claw_core_context_provider_t {
        name: PROFILE_NAME.as_ptr() as *const c_char,
        collect: Some(profile_collect),
        user_ctx: ptr::null_mut(),
        flags: 0,
    });

#[no_mangle]
pub static claw_memory_long_term_provider: ProviderSync =
    ProviderSync(claw_core_context_provider_t {
        name: LONG_TERM_NAME.as_ptr() as *const c_char,
        collect: Some(long_term_collect),
        user_ctx: ptr::null_mut(),
        flags: 0,
    });

#[no_mangle]
pub static claw_memory_long_term_lightweight_provider: ProviderSync =
    ProviderSync(claw_core_context_provider_t {
        name: LONG_TERM_NAME.as_ptr() as *const c_char,
        collect: Some(long_term_lightweight_collect),
        user_ctx: ptr::null_mut(),
        flags: 0,
    });

#[no_mangle]
pub static claw_memory_session_history_provider: ProviderSync =
    ProviderSync(claw_core_context_provider_t {
        name: SESSION_NAME.as_ptr() as *const c_char,
        collect: Some(session_history_collect),
        user_ctx: ptr::null_mut(),
        flags: claw_memory::consts::CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY,
    });
