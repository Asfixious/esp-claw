//! The `#[no_mangle] extern "C"` exports of the `claw_memory.h` ABI plus the
//! four context-provider statics. C callers link against these symbols
//! unchanged.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use claw_core::cabi::{
    claw_core_context_persist_batch_t, claw_core_context_provider_t, claw_core_context_t,
    claw_core_request_t,
};
use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND, ESP_ERR_NO_MEM, ESP_OK,
};

use crate::api::{self, LlmConfig, MemoryConfig};
use crate::consts::{CONTEXT_KIND_MESSAGES, CONTEXT_KIND_SYSTEM_PROMPT, MAX_SUMMARIES};
use crate::item::{claw_memory_item_t, MemItem};
use crate::session::{self, PersistRecordIn};
use crate::util;

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
#[no_mangle]
pub unsafe extern "C" fn claw_memory_init(config: *const claw_memory_config_t) -> EspErr {
    if config.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let c = &*config;
    let llm = &c.llm;

    let cfg = MemoryConfig {
        session_root_dir: util::cstr_to_string(c.session_root_dir),
        memory_root_dir: util::cstr_to_string(c.memory_root_dir),
        max_message_chars: c.max_message_chars,
        max_tool_iterations: c.max_tool_iterations,
        llm: LlmConfig {
            api_key: util::cstr_to_string(llm.api_key),
            backend_type: util::cstr_to_string(llm.backend_type),
            model: util::cstr_to_string(llm.model),
            base_url: util::cstr_to_string(llm.base_url),
            auth_type: util::cstr_to_string(llm.auth_type),
            max_tokens_field: util::cstr_to_string(llm.max_tokens_field),
            timeout_ms: llm.timeout_ms,
            max_tokens: llm.max_tokens,
            image_max_bytes: llm.image_max_bytes,
            supports_tools: llm.supports_tools,
            supports_vision: llm.supports_vision,
            image_remote_url_only: llm.image_remote_url_only,
        },
        enable_async_extract_stage_note: c.enable_async_extract_stage_note,
    };
    api::init(&cfg)
}

// --- store / update / forget -----------------------------------------------

/// `claw_memory_store`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_store(item: *const claw_memory_item_t) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = MemItem::from_c(&*item);
    let (err, _changed) = api::store_with_result(&mut mem);
    err
}

/// `claw_memory_store_with_result`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_store_with_result(
    item: *mut claw_memory_item_t,
    out_changed: *mut bool,
) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = MemItem::from_c(&*item);
    let (err, changed) = api::store_with_result(&mut mem);
    if err == ESP_OK {
        *item = mem.to_c();
    }
    if !out_changed.is_null() {
        *out_changed = changed;
    }
    err
}

/// `claw_memory_update`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_update(item: *const claw_memory_item_t) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = MemItem::from_c(&*item);
    let (err, _changed) = api::update_with_result(&mut mem);
    err
}

/// `claw_memory_update_with_result`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_update_with_result(
    item: *mut claw_memory_item_t,
    out_changed: *mut bool,
) -> EspErr {
    if item.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let mut mem = MemItem::from_c(&*item);
    let (err, changed) = api::update_with_result(&mut mem);
    if err == ESP_OK {
        *item = mem.to_c();
    }
    if !out_changed.is_null() {
        *out_changed = changed;
    }
    err
}

/// `claw_memory_forget`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_forget(memory_id: *const c_char) -> EspErr {
    let id = match util::cstr_to_string(memory_id) {
        Some(s) => s,
        None => return ESP_ERR_INVALID_ARG,
    };
    let (err, _item, _changed) = api::forget_with_result(&id, false);
    err
}

/// `claw_memory_forget_with_result`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_forget_with_result(
    memory_id: *const c_char,
    out_item: *mut claw_memory_item_t,
    out_changed: *mut bool,
) -> EspErr {
    let id = match util::cstr_to_string(memory_id) {
        Some(s) => s,
        None => return ESP_ERR_INVALID_ARG,
    };
    let (err, item, changed) = api::forget_with_result(&id, !out_item.is_null());
    if err == ESP_OK {
        if let Some(it) = item {
            if !out_item.is_null() {
                *out_item = it.to_c();
            }
        }
        if !out_changed.is_null() {
            *out_changed = changed;
        }
    }
    err
}

// --- recall / list ---------------------------------------------------------

/// `claw_memory_recall`.
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
        for i in 0..count {
            let p = *q.summary_labels.add(i);
            if let Some(s) = util::cstr_to_string(p) {
                labels.push(s);
            }
        }
    }

    let (err, json) = api::recall(&labels, q.limit);
    if err != ESP_OK {
        return err;
    }
    write_out_json(out_json, json)
}

/// `claw_memory_list`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_list(out_json: *mut *mut c_char) -> EspErr {
    if out_json.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    *out_json = ptr::null_mut();
    let (err, json) = api::list();
    if err != ESP_OK {
        return err;
    }
    write_out_json(out_json, json)
}

unsafe fn write_out_json(out_json: *mut *mut c_char, json: Option<String>) -> EspErr {
    let s = json.unwrap_or_default();
    let p = util::c_strdup(&s);
    if p.is_null() {
        return ESP_ERR_NO_MEM;
    }
    *out_json = p;
    ESP_OK
}

// --- primary summary label -------------------------------------------------

/// `claw_memory_item_primary_summary_label`.
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
    let mem = MemItem::from_c(&*item);
    match api::item_primary_summary_label(&mem) {
        Some(label) => {
            util::write_c_buf(buf, size, &label);
            ESP_OK
        }
        None => ESP_ERR_NOT_FOUND,
    }
}

// --- session / note --------------------------------------------------------

/// `claw_memory_note_session_summary`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_note_session_summary(
    session_id: *const c_char,
    summary_list: *const c_char,
) -> EspErr {
    let session_id = match util::cstr_to_string(session_id) {
        Some(s) => s,
        None => return ESP_OK,
    };
    let summary_list = util::cstr_to_string(summary_list).unwrap_or_default();
    session::note_session_summary(&session_id, &summary_list)
}

/// `claw_memory_request_mark_manual_write`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_request_mark_manual_write(request_id: u32) -> EspErr {
    session::request_mark_manual_write(request_id)
}

/// `claw_memory_delete_session_history`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_delete_session_history(
    session_id: *const c_char,
    out_deleted_any: *mut bool,
) -> EspErr {
    if out_deleted_any.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    *out_deleted_any = false;
    let session_id = match util::cstr_to_string(session_id) {
        Some(s) if !s.is_empty() => s,
        _ => return ESP_ERR_INVALID_ARG,
    };
    match session::delete_session_history(&session_id) {
        Ok(deleted) => {
            *out_deleted_any = deleted;
            ESP_OK
        }
        Err(e) => e,
    }
}

// --- core callbacks --------------------------------------------------------

/// `claw_memory_persist_context_callback`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_persist_context_callback(
    batch: *const claw_core_context_persist_batch_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    if batch.is_null() {
        return session::persist_context(None, &[], false, ptr::null());
    }
    let b = &*batch;
    let session_id = util::cstr_to_string(b.session_id);

    let mut records: Vec<PersistRecordIn> = Vec::new();
    if !b.records.is_null() {
        for i in 0..b.record_count {
            let r = &*b.records.add(i);
            records.push(PersistRecordIn {
                record_type: r.r#type as i32,
                message_json: util::cstr_to_string(r.message_json),
                text: util::cstr_to_string(r.text),
            });
        }
    }

    session::persist_context(session_id.as_deref(), &records, b.turn_completed, b.request)
}

/// `claw_memory_request_gate_callback`.
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
    let session_id = util::cstr_to_string((*request).session_id);
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
    util::write_c_buf(reject_message, reject_message_size, crate::consts::SESSION_SIZE_WARNING);
    ESP_ERR_INVALID_STATE
}

/// `claw_memory_request_start_callback`.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_request_start_callback(
    request: *const claw_core_request_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    if request.is_null() {
        return ESP_OK;
    }
    let r = &*request;
    let session_id = util::cstr_to_string(r.session_id);
    let user_text = util::cstr_to_string(r.user_text);
    crate::extract::async_extract_ensure_started(r.request_id, session_id.as_deref(), user_text.as_deref())
}

/// `claw_memory_stage_note_callback`.
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
    let session_id = util::cstr_to_string(r.session_id);
    if let Some(note) = session::stage_note(r.request_id, session_id.as_deref()) {
        let p = util::c_strdup(&note);
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
    content: Result<String, EspErr>,
) -> EspErr {
    if out_context.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    (*out_context).kind = 0;
    (*out_context).content = ptr::null_mut();
    match content {
        Ok(s) => {
            let p = util::c_strdup(&s);
            if p.is_null() {
                return ESP_ERR_NO_MEM;
            }
            (*out_context).kind = kind;
            (*out_context).content = p;
            ESP_OK
        }
        Err(e) => e,
    }
}

unsafe extern "C" fn profile_collect(
    _request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    finish_context(out_context, CONTEXT_KIND_SYSTEM_PROMPT, crate::profile::profile_collect_content())
}

unsafe extern "C" fn long_term_collect(
    _request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    finish_context(out_context, CONTEXT_KIND_SYSTEM_PROMPT, api::long_term_collect_content())
}

unsafe extern "C" fn long_term_lightweight_collect(
    _request: *const claw_core_request_t,
    out_context: *mut claw_core_context_t,
    _user_ctx: *mut c_void,
) -> EspErr {
    finish_context(
        out_context,
        CONTEXT_KIND_SYSTEM_PROMPT,
        crate::lightweight::lightweight_collect_content(),
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
        util::cstr_to_string((*request).session_id)
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
pub static claw_memory_profile_provider: ProviderSync = ProviderSync(claw_core_context_provider_t {
    name: PROFILE_NAME.as_ptr() as *const c_char,
    collect: Some(profile_collect),
    user_ctx: ptr::null_mut(),
    flags: 0,
});

#[no_mangle]
pub static claw_memory_long_term_provider: ProviderSync = ProviderSync(claw_core_context_provider_t {
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
        flags: crate::consts::CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY,
    });
