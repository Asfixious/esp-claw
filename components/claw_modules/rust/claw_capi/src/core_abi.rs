//! C ABI for `claw_core` (moved from `claw_core::cabi`), exporting the
//! `claw_core.h` surface as `#[no_mangle] extern "C"` and adapting the C
//! callback function pointers to the [`claw_core::callbacks`] traits.
//!
//! The handle is an `Arc<Core>` leaked via [`Arc::into_raw`]; `destroy` reclaims
//! it. C-side string outputs from callbacks (`call_cap`, context content, stage
//! notes) are freed with the C allocator's `free`; response strings are produced
//! by Rust and reclaimed by [`claw_core_response_free`].
//!
//! Several callback adapters are only instantiated by the espidf
//! `claw_core_create`; on the host they exist for their trait impls but are
//! otherwise unused, hence the module-wide dead-code allowance off-target.
#![cfg_attr(not(target_os = "espidf"), allow(dead_code))]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use std::ffi::{CStr, CString};
use std::sync::Arc;

use claw_platform::{
    esp_err_t as EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND, ESP_OK,
};

use claw_core::callbacks::{
    CompletionObserver, CompletionSummary, ContextProvider, GateOutcome, PersistContext,
    PersistRecord, ProviderOutcome, RequestGate, RequestStart, StageNote,
};
use claw_core::consts::ContextKind;
use claw_core::core::Core;
use claw_core::request::RequestItem;
use claw_core::response::ResponseItem;

// --- C ABI types ---------------------------------------------------------

#[repr(C)]
pub struct claw_core_request_t {
    pub request_id: u32,
    pub flags: u32,
    pub session_id: *const c_char,
    pub user_text: *const c_char,
    pub source_channel: *const c_char,
    pub source_chat_id: *const c_char,
    pub source_sender_id: *const c_char,
    pub source_message_id: *const c_char,
    pub source_cap: *const c_char,
    pub target_channel: *const c_char,
    pub target_chat_id: *const c_char,
}

#[repr(C)]
pub struct claw_core_context_t {
    pub kind: c_int,
    pub content: *mut c_char,
}

#[repr(C)]
pub struct claw_core_context_record_t {
    pub r#type: c_int,
    pub message_json: *const c_char,
    pub text: *const c_char,
}

#[repr(C)]
pub struct claw_core_context_persist_batch_t {
    pub session_id: *const c_char,
    pub request: *const claw_core_request_t,
    pub records: *const claw_core_context_record_t,
    pub record_count: usize,
    pub turn_completed: bool,
}

#[repr(C)]
pub struct claw_core_completion_summary_t {
    pub request_id: u32,
    pub session_id: *const c_char,
    pub final_text: *const c_char,
    pub context_providers_csv: *const c_char,
    pub tool_calls_csv: *const c_char,
}

pub type claw_core_persist_context_fn =
    Option<unsafe extern "C" fn(*const claw_core_context_persist_batch_t, *mut c_void) -> EspErr>;
pub type claw_core_request_start_fn =
    Option<unsafe extern "C" fn(*const claw_core_request_t, *mut c_void) -> EspErr>;
pub type claw_core_request_gate_fn = Option<
    unsafe extern "C" fn(*const claw_core_request_t, *mut c_char, usize, *mut c_void) -> EspErr,
>;
pub type claw_core_stage_note_fn = Option<
    unsafe extern "C" fn(*const claw_core_request_t, *mut *mut c_char, *mut c_void) -> EspErr,
>;
pub type claw_core_context_provider_collect_fn = Option<
    unsafe extern "C" fn(
        *const claw_core_request_t,
        *mut claw_core_context_t,
        *mut c_void,
    ) -> EspErr,
>;
pub type claw_core_call_cap_fn = Option<
    unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *const claw_core_request_t,
        *mut *mut c_char,
        *mut c_void,
    ) -> EspErr,
>;
pub type claw_core_completion_observer_fn =
    Option<unsafe extern "C" fn(*const claw_core_completion_summary_t, *mut c_void)>;

#[repr(C)]
pub struct claw_core_context_provider_t {
    pub name: *const c_char,
    pub collect: claw_core_context_provider_collect_fn,
    pub user_ctx: *mut c_void,
    pub flags: u32,
}

#[repr(C)]
pub struct claw_core_config_t {
    pub instance_id: u32,
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
    pub system_prompt: *const c_char,
    pub persist_context: claw_core_persist_context_fn,
    pub persist_context_user_ctx: *mut c_void,
    pub request_gate: claw_core_request_gate_fn,
    pub request_gate_user_ctx: *mut c_void,
    pub on_request_start: claw_core_request_start_fn,
    pub on_request_start_user_ctx: *mut c_void,
    pub collect_stage_note: claw_core_stage_note_fn,
    pub collect_stage_note_user_ctx: *mut c_void,
    pub call_cap: claw_core_call_cap_fn,
    pub cap_user_ctx: *mut c_void,
    pub task_stack_size: u32,
    /// `UBaseType_t`
    pub task_priority: u32,
    /// `BaseType_t`
    pub task_core: i32,
    pub max_tool_iterations: u32,
    pub request_queue_len: u32,
    pub response_queue_len: u32,
    pub max_context_providers: usize,
}

#[repr(C)]
pub struct claw_core_response_t {
    pub request_id: u32,
    pub status: c_int,
    pub completion_type: c_int,
    pub target_channel: *mut c_char,
    pub target_chat_id: *mut c_char,
    pub text: *mut c_char,
    pub error_message: *mut c_char,
}

/// `claw_core_handle_t` (opaque `struct claw_core_state *`).
pub type claw_core_handle_t = *mut c_void;

// --- string helpers ------------------------------------------------------

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

unsafe fn cstr_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

unsafe fn cstr_str(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Copy a C-allocated string and free it with the C allocator.
unsafe fn take_c_string(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    crate::ffi::free(p as *mut c_void);
    Some(s)
}

fn push_cstr(s: &str, storage: &mut Vec<CString>) -> *const c_char {
    let c = cstring(s);
    let p = c.as_ptr();
    storage.push(c);
    p
}

fn opt_cstr(o: &Option<String>, storage: &mut Vec<CString>) -> *const c_char {
    match o {
        Some(s) => push_cstr(s, storage),
        None => ptr::null(),
    }
}

fn opt_into_raw(o: Option<String>) -> *mut c_char {
    match o {
        Some(s) => CString::new(s)
            .map(|c| c.into_raw())
            .unwrap_or(ptr::null_mut()),
        None => ptr::null_mut(),
    }
}

unsafe fn free_into_raw(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Borrowed C view of a [`RequestItem`], valid while this struct lives.
struct RequestCView {
    _storage: Vec<CString>,
    view: claw_core_request_t,
}

impl RequestCView {
    fn new(req: &RequestItem) -> RequestCView {
        let mut storage = Vec::with_capacity(10);
        let view = claw_core_request_t {
            request_id: req.request_id,
            flags: req.flags,
            session_id: opt_cstr(&req.session_id, &mut storage),
            user_text: push_cstr(&req.user_text, &mut storage),
            source_channel: opt_cstr(&req.source_channel, &mut storage),
            source_chat_id: opt_cstr(&req.source_chat_id, &mut storage),
            source_sender_id: opt_cstr(&req.source_sender_id, &mut storage),
            source_message_id: opt_cstr(&req.source_message_id, &mut storage),
            source_cap: opt_cstr(&req.source_cap, &mut storage),
            target_channel: opt_cstr(&req.target_channel, &mut storage),
            target_chat_id: opt_cstr(&req.target_chat_id, &mut storage),
        };
        RequestCView {
            _storage: storage,
            view,
        }
    }
}

pub(crate) unsafe fn request_from_c(req: *const claw_core_request_t) -> RequestItem {
    let r = &*req;
    RequestItem {
        request_id: r.request_id,
        flags: r.flags,
        session_id: cstr_opt(r.session_id),
        user_text: cstr_str(r.user_text),
        source_channel: cstr_opt(r.source_channel),
        source_chat_id: cstr_opt(r.source_chat_id),
        source_sender_id: cstr_opt(r.source_sender_id),
        source_message_id: cstr_opt(r.source_message_id),
        source_cap: cstr_opt(r.source_cap),
        target_channel: cstr_opt(r.target_channel),
        target_chat_id: cstr_opt(r.target_chat_id),
    }
}

fn context_kind_from_c(kind: c_int) -> ContextKind {
    match kind {
        1 => ContextKind::Messages,
        2 => ContextKind::Tools,
        _ => ContextKind::SystemPrompt,
    }
}

// --- callback adapters ---------------------------------------------------

type CollectFn = unsafe extern "C" fn(
    *const claw_core_request_t,
    *mut claw_core_context_t,
    *mut c_void,
) -> EspErr;

struct CContextProvider {
    name: String,
    collect: CollectFn,
    user_ctx: *mut c_void,
    flags: u32,
}
unsafe impl Send for CContextProvider {}
unsafe impl Sync for CContextProvider {}

impl ContextProvider for CContextProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn flags(&self) -> u32 {
        self.flags
    }
    fn collect(&self, request: &RequestItem) -> ProviderOutcome {
        let view = RequestCView::new(request);
        let mut ctx = claw_core_context_t {
            kind: 0,
            content: ptr::null_mut(),
        };
        let err = unsafe { (self.collect)(&view.view, &mut ctx, self.user_ctx) };
        if err == ESP_ERR_NOT_FOUND {
            return ProviderOutcome::Skip;
        }
        if err != ESP_OK {
            return ProviderOutcome::Error(err);
        }
        let kind = context_kind_from_c(ctx.kind);
        let content = unsafe { take_c_string(ctx.content) }.unwrap_or_default();
        ProviderOutcome::Provided { kind, content }
    }
}

type PersistFn =
    unsafe extern "C" fn(*const claw_core_context_persist_batch_t, *mut c_void) -> EspErr;

struct CPersistContext {
    f: PersistFn,
    user_ctx: *mut c_void,
}
unsafe impl Send for CPersistContext {}
unsafe impl Sync for CPersistContext {}

impl PersistContext for CPersistContext {
    fn persist(
        &self,
        session_id: &str,
        request: &RequestItem,
        records: &[PersistRecord],
        turn_completed: bool,
    ) -> EspErr {
        let view = RequestCView::new(request);
        let session = cstring(session_id);
        let mut storage: Vec<CString> = Vec::new();
        let c_records: Vec<claw_core_context_record_t> = records
            .iter()
            .map(|r| claw_core_context_record_t {
                r#type: r.record_type as c_int,
                message_json: opt_cstr(&r.message_json, &mut storage),
                text: opt_cstr(&r.text, &mut storage),
            })
            .collect();
        let batch = claw_core_context_persist_batch_t {
            session_id: session.as_ptr(),
            request: &view.view,
            records: c_records.as_ptr(),
            record_count: c_records.len(),
            turn_completed,
        };
        unsafe { (self.f)(&batch, self.user_ctx) }
    }
}

type GateFn =
    unsafe extern "C" fn(*const claw_core_request_t, *mut c_char, usize, *mut c_void) -> EspErr;

struct CRequestGate {
    f: GateFn,
    user_ctx: *mut c_void,
}
unsafe impl Send for CRequestGate {}
unsafe impl Sync for CRequestGate {}

impl RequestGate for CRequestGate {
    fn gate(&self, request: &RequestItem) -> GateOutcome {
        let view = RequestCView::new(request);
        let mut buf = [0i8; 192];
        let err = unsafe {
            (self.f)(
                &view.view,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
                self.user_ctx,
            )
        };
        if err == ESP_OK {
            return GateOutcome::Allow;
        }
        let msg = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned();
        if msg.is_empty() {
            GateOutcome::Error(err)
        } else {
            GateOutcome::Reject(msg)
        }
    }
}

type StartFn = unsafe extern "C" fn(*const claw_core_request_t, *mut c_void) -> EspErr;

struct CRequestStart {
    f: StartFn,
    user_ctx: *mut c_void,
}
unsafe impl Send for CRequestStart {}
unsafe impl Sync for CRequestStart {}

impl RequestStart for CRequestStart {
    fn on_start(&self, request: &RequestItem) -> EspErr {
        let view = RequestCView::new(request);
        unsafe { (self.f)(&view.view, self.user_ctx) }
    }
}

type StageNoteFn =
    unsafe extern "C" fn(*const claw_core_request_t, *mut *mut c_char, *mut c_void) -> EspErr;

struct CStageNote {
    f: StageNoteFn,
    user_ctx: *mut c_void,
}
unsafe impl Send for CStageNote {}
unsafe impl Sync for CStageNote {}

impl StageNote for CStageNote {
    fn collect(&self, request: &RequestItem) -> Result<Option<String>, EspErr> {
        let view = RequestCView::new(request);
        let mut out: *mut c_char = ptr::null_mut();
        let err = unsafe { (self.f)(&view.view, &mut out, self.user_ctx) };
        if err != ESP_OK {
            unsafe { take_c_string(out) };
            return Err(err);
        }
        Ok(unsafe { take_c_string(out) })
    }
}

type ObserverFn = unsafe extern "C" fn(*const claw_core_completion_summary_t, *mut c_void);

struct CCompletionObserver {
    f: ObserverFn,
    user_ctx: *mut c_void,
}
unsafe impl Send for CCompletionObserver {}
unsafe impl Sync for CCompletionObserver {}

impl CompletionObserver for CCompletionObserver {
    fn on_complete(&self, summary: &CompletionSummary) {
        let session = summary.session_id.map(cstring);
        let final_text = summary.final_text.map(cstring);
        let providers = cstring(summary.context_providers_csv);
        let tools = cstring(summary.tool_calls_csv);
        let c_summary = claw_core_completion_summary_t {
            request_id: summary.request_id,
            session_id: session.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
            final_text: final_text.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
            context_providers_csv: providers.as_ptr(),
            tool_calls_csv: tools.as_ptr(),
        };
        unsafe { (self.f)(&c_summary, self.user_ctx) };
    }
}

// --- handle helpers ------------------------------------------------------

/// Borrow the [`Core`] behind a `claw_core_handle_t`.
///
/// # Safety
/// `handle` must be a live handle returned by `claw_core_create` (or null).
pub unsafe fn handle_ref<'a>(handle: claw_core_handle_t) -> Option<&'a Core> {
    if handle.is_null() {
        None
    } else {
        Some(&*(handle as *const Core))
    }
}

/// Borrow the [`Core`] behind a `claw_core_handle_t`. Exposed for sibling
/// modules (e.g. `crate::llm`) that receive the opaque handle from C and
/// need to call core methods such as [`Core::infer_media`].
///
/// # Safety
/// `handle` must be a live handle returned by `claw_core_create` (or null).
pub unsafe fn core_from_handle<'a>(handle: claw_core_handle_t) -> Option<&'a Core> {
    handle_ref(handle)
}

// --- exported C ABI ------------------------------------------------------

/// `claw_core_create`.
///
/// # Safety
/// `config` and `out_core` must each be null or a valid pointer; the C string
/// fields of `*config` must be null or NUL-terminated and valid for reads.
#[no_mangle]
pub unsafe extern "C" fn claw_core_create(
    config: *const claw_core_config_t,
    out_core: *mut claw_core_handle_t,
) -> EspErr {
    crate::ffi::install_logger_once();
    if !out_core.is_null() {
        *out_core = ptr::null_mut();
    }
    if config.is_null() || out_core.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let c = &*config;
    if c.system_prompt.is_null()
        || c.api_key.is_null()
        || c.model.is_null()
        || c.backend_type.is_null()
        || cstr_str(c.backend_type).is_empty()
    {
        return ESP_ERR_INVALID_ARG;
    }

    build_and_store_core(c, out_core)
}

#[cfg(target_os = "espidf")]
unsafe fn build_and_store_core(
    c: &claw_core_config_t,
    out_core: *mut claw_core_handle_t,
) -> EspErr {
    use claw_api::ClawApiConfig;
    use claw_core::core::CoreConfig;

    let runtime_config = ClawApiConfig {
        api_key: cstr_opt(c.api_key),
        backend_type: cstr_str(c.backend_type),
        model: cstr_opt(c.model),
        base_url: cstr_opt(c.base_url),
        auth_type: cstr_opt(c.auth_type),
        max_tokens_field: cstr_opt(c.max_tokens_field),
        timeout_ms: c.timeout_ms,
        max_tokens: c.max_tokens,
        image_max_bytes: c.image_max_bytes,
        supports_tools: c.supports_tools,
        supports_vision: c.supports_vision,
        image_remote_url_only: c.image_remote_url_only,
    };

    let cfg = CoreConfig {
        instance_id: c.instance_id,
        system_prompt: cstr_str(c.system_prompt),
        runtime_config,
        http: Arc::new(claw_sys::EspIdfHttp),
        capability_invoker: Some(
            Box::new(crate::capability::default_registry(c.cap_user_ctx))
                as Box<dyn claw_cap::CapabilityInvoker>,
        ),
        request_gate: c.request_gate.map(|f| {
            Box::new(CRequestGate {
                f,
                user_ctx: c.request_gate_user_ctx,
            }) as Box<dyn RequestGate>
        }),
        on_request_start: c.on_request_start.map(|f| {
            Box::new(CRequestStart {
                f,
                user_ctx: c.on_request_start_user_ctx,
            }) as Box<dyn RequestStart>
        }),
        collect_stage_note: c.collect_stage_note.map(|f| {
            Box::new(CStageNote {
                f,
                user_ctx: c.collect_stage_note_user_ctx,
            }) as Box<dyn StageNote>
        }),
        events: Some(Box::new(espidf_events::CEventPublisher)),
        max_tool_iterations: c.max_tool_iterations,
        request_queue_len: c.request_queue_len,
        response_queue_len: c.response_queue_len,
        max_context_providers: c.max_context_providers,
        task_stack_size: c.task_stack_size,
        task_priority: c.task_priority,
        task_core: c.task_core,
    };

    match Core::create(cfg) {
        Ok(arc) => {
            *out_core = Arc::into_raw(arc) as claw_core_handle_t;
            ESP_OK
        }
        Err(err) => crate::errmap::core_esp_err(&err),
    }
}

#[cfg(not(target_os = "espidf"))]
unsafe fn build_and_store_core(
    _c: &claw_core_config_t,
    _out_core: *mut claw_core_handle_t,
) -> EspErr {
    // No esp_http_client / event router off-target.
    claw_platform::ESP_ERR_NOT_SUPPORTED
}

/// `claw_core_start`.
///
/// # Safety
/// `core` must be null or a live handle returned by [`claw_core_create`].
#[no_mangle]
pub unsafe extern "C" fn claw_core_start(core: claw_core_handle_t) -> EspErr {
    if core.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let arc = Arc::from_raw(core as *const Core);
    let ret = match arc.start() {
        Ok(()) => ESP_OK,
        Err(err) => crate::errmap::core_esp_err(&err),
    };
    let _ = Arc::into_raw(arc);
    ret
}

/// `claw_core_stop`.
///
/// # Safety
/// `core` must be null or a live handle returned by [`claw_core_create`].
#[no_mangle]
pub unsafe extern "C" fn claw_core_stop(core: claw_core_handle_t, timeout_ms: u32) -> EspErr {
    match handle_ref(core) {
        Some(core) => match core.stop(timeout_ms) {
            Ok(()) => ESP_OK,
            Err(err) => crate::errmap::core_esp_err(&err),
        },
        None => ESP_ERR_INVALID_STATE,
    }
}

/// `claw_core_destroy`.
///
/// # Safety
/// `core` must be null or a live handle returned by [`claw_core_create`] that
/// has not already been destroyed; it is consumed by this call.
#[no_mangle]
pub unsafe extern "C" fn claw_core_destroy(core: claw_core_handle_t) -> EspErr {
    if core.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    let arc = Arc::from_raw(core as *const Core);
    if arc.is_started() {
        let err = match arc.stop(5000) {
            Ok(()) => ESP_OK,
            Err(err) => crate::errmap::core_esp_err(&err),
        };
        if err != ESP_OK {
            let _ = Arc::into_raw(arc);
            return err;
        }
    }
    drop(arc);
    ESP_OK
}

/// `claw_core_add_context_provider`.
///
/// # Safety
/// `core` must be null or a live handle from [`claw_core_create`], and
/// `provider` must be null or a valid pointer whose `name` is NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn claw_core_add_context_provider(
    core: claw_core_handle_t,
    provider: *const claw_core_context_provider_t,
) -> EspErr {
    let Some(core) = handle_ref(core) else {
        return ESP_ERR_INVALID_STATE;
    };
    if provider.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let p = &*provider;
    let Some(collect) = p.collect else {
        return ESP_ERR_INVALID_ARG;
    };
    if p.name.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let adapter = CContextProvider {
        name: cstr_str(p.name),
        collect,
        user_ctx: p.user_ctx,
        flags: p.flags,
    };
    match core.add_context_provider(Box::new(adapter)) {
        Ok(()) => ESP_OK,
        Err(err) => crate::errmap::core_esp_err(&err),
    }
}

/// `claw_core_add_completion_observer`.
///
/// # Safety
/// `core` must be null or a live handle from [`claw_core_create`]; `observer`
/// (if set) and `user_ctx` must remain valid for the lifetime of the core.
#[no_mangle]
pub unsafe extern "C" fn claw_core_add_completion_observer(
    core: claw_core_handle_t,
    observer: claw_core_completion_observer_fn,
    user_ctx: *mut c_void,
) -> EspErr {
    let Some(core) = handle_ref(core) else {
        return ESP_ERR_INVALID_STATE;
    };
    let Some(f) = observer else {
        return ESP_ERR_INVALID_ARG;
    };
    match core.add_completion_observer(Box::new(CCompletionObserver { f, user_ctx })) {
        Ok(()) => ESP_OK,
        Err(err) => crate::errmap::core_esp_err(&err),
    }
}

/// `claw_core_submit`.
///
/// # Safety
/// `core` must be null or a live handle from [`claw_core_create`], and
/// `request` must be null or a valid pointer with NUL-terminated string fields.
#[no_mangle]
pub unsafe extern "C" fn claw_core_submit(
    core: claw_core_handle_t,
    request: *const claw_core_request_t,
    timeout_ms: u32,
) -> EspErr {
    let Some(core) = handle_ref(core) else {
        return ESP_ERR_INVALID_STATE;
    };
    if request.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    match core.submit(request_from_c(request), timeout_ms) {
        Ok(()) => ESP_OK,
        Err(err) => crate::errmap::core_esp_err(&err),
    }
}

/// `claw_core_cancel_request`.
///
/// # Safety
/// `core` must be null or a live handle returned by [`claw_core_create`].
#[no_mangle]
pub unsafe extern "C" fn claw_core_cancel_request(
    core: claw_core_handle_t,
    request_id: u32,
) -> EspErr {
    match handle_ref(core) {
        Some(core) => match core.cancel_request(request_id) {
            Ok(()) => ESP_OK,
            Err(err) => crate::errmap::core_esp_err(&err),
        },
        None => ESP_ERR_INVALID_STATE,
    }
}

/// `claw_core_receive`.
///
/// # Safety
/// `core` must be null or a live handle from [`claw_core_create`], and
/// `response` must be null or a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_core_receive(
    core: claw_core_handle_t,
    response: *mut claw_core_response_t,
    timeout_ms: u32,
) -> EspErr {
    claw_core_receive_for(core, 0, response, timeout_ms)
}

/// `claw_core_receive_for`.
///
/// # Safety
/// `core` must be null or a live handle from [`claw_core_create`], and
/// `response` must be null or a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn claw_core_receive_for(
    core: claw_core_handle_t,
    request_id: u32,
    response: *mut claw_core_response_t,
    timeout_ms: u32,
) -> EspErr {
    let Some(core) = handle_ref(core) else {
        return ESP_ERR_INVALID_STATE;
    };
    if response.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    match core.receive_for(request_id, timeout_ms) {
        Ok(item) => {
            write_response_to_c(item, response);
            ESP_OK
        }
        Err(err) => crate::errmap::core_esp_err(&err),
    }
}

unsafe fn write_response_to_c(item: ResponseItem, out: *mut claw_core_response_t) {
    let out = &mut *out;
    out.request_id = item.request_id;
    out.status = item.status as c_int;
    out.completion_type = item.completion_type as c_int;
    out.target_channel = opt_into_raw(item.target_channel);
    out.target_chat_id = opt_into_raw(item.target_chat_id);
    out.text = opt_into_raw(item.text);
    out.error_message = opt_into_raw(item.error_message);
}

/// `claw_core_response_free`.
///
/// # Safety
/// `response` must be null or a pointer previously populated by
/// [`claw_core_receive`] / [`claw_core_receive_for`]; its owned strings are
/// freed and the pointers nulled.
#[no_mangle]
pub unsafe extern "C" fn claw_core_response_free(response: *mut claw_core_response_t) {
    if response.is_null() {
        return;
    }
    let r = &mut *response;
    free_into_raw(r.target_channel);
    free_into_raw(r.target_chat_id);
    free_into_raw(r.text);
    free_into_raw(r.error_message);
    r.target_channel = ptr::null_mut();
    r.target_chat_id = ptr::null_mut();
    r.text = ptr::null_mut();
    r.error_message = ptr::null_mut();
}

/// `claw_core_publish_stage_text`.
///
/// # Safety
/// `request` must be null or a valid pointer with NUL-terminated string fields,
/// and `text` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn claw_core_publish_stage_text(
    request: *const claw_core_request_t,
    text: *const c_char,
) -> EspErr {
    if request.is_null() || text.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let text = cstr_str(text);
    if text.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    publish_stage_text_impl(&request_from_c(request), &text)
}

#[cfg(target_os = "espidf")]
pub(crate) fn publish_stage_text_impl(request: &RequestItem, text: &str) -> EspErr {
    claw_core::events::publish_stage_text(&espidf_events::CEventPublisher, request, text)
}

#[cfg(not(target_os = "espidf"))]
pub(crate) fn publish_stage_text_impl(_request: &RequestItem, _text: &str) -> EspErr {
    claw_platform::ESP_ERR_NOT_SUPPORTED
}

// --- espidf event publisher ----------------------------------------------

#[cfg(target_os = "espidf")]
mod espidf_events {
    use super::*;

    /// Mirror of `claw_session_policy_t` from `claw_session_mgr.h`.
    #[repr(i32)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum SessionPolicy {
        #[default]
        Chat = 0,
        Trigger = 1,
        Global = 2,
        Ephemeral = 3,
        NoSave = 4,
    }

    /// Rust-owned representation of the fields `claw_core` populates on a
    /// `claw_event_t`. String fields are truncated to the C buffer sizes at the
    /// publish boundary below.
    #[derive(Clone, Debug, Default)]
    pub struct ClawEvent {
        pub event_id: String,
        pub source_cap: String,
        pub event_type: String,
        pub source_channel: String,
        pub chat_id: String,
        pub message_id: String,
        pub correlation_id: String,
        pub content_type: String,
        pub timestamp_ms: i64,
        pub session_policy: SessionPolicy,
        pub text: Option<String>,
        pub payload_json: Option<String>,
    }

    /// Injection point for publishing events to the (still-C) event router.
    pub trait EventPublisher: Send + Sync {
        /// Equivalent to `claw_event_router_publish(&event)`.
        fn publish(&self, event: &ClawEvent) -> EspErr;
    }

    #[repr(C)]
    struct claw_event_t {
        event_id: [c_char; 48],
        source_cap: [c_char; 32],
        event_type: [c_char; 32],
        source_channel: [c_char; 16],
        target_channel: [c_char; 16],
        source_endpoint: [c_char; 64],
        target_endpoint: [c_char; 96],
        chat_id: [c_char; 96],
        sender_id: [c_char; 96],
        message_id: [c_char; 96],
        correlation_id: [c_char; 96],
        content_type: [c_char; 24],
        timestamp_ms: i64,
        session_policy: c_int,
        text: *mut c_char,
        payload_json: *mut c_char,
    }

    extern "C" {
        fn claw_event_router_publish(event: *const claw_event_t) -> EspErr;
    }

    fn copy_field(dst: &mut [c_char], src: &str) {
        let bytes = src.as_bytes();
        let max = dst.len().saturating_sub(1);
        let n = bytes.len().min(max);
        for i in 0..n {
            dst[i] = bytes[i] as c_char;
        }
        dst[n] = 0;
    }

    pub struct CEventPublisher;

    impl EventPublisher for CEventPublisher {
        fn publish(&self, e: &ClawEvent) -> EspErr {
            let mut ev: claw_event_t = unsafe { core::mem::zeroed() };
            copy_field(&mut ev.event_id, &e.event_id);
            copy_field(&mut ev.source_cap, &e.source_cap);
            copy_field(&mut ev.event_type, &e.event_type);
            copy_field(&mut ev.source_channel, &e.source_channel);
            copy_field(&mut ev.chat_id, &e.chat_id);
            copy_field(&mut ev.message_id, &e.message_id);
            copy_field(&mut ev.correlation_id, &e.correlation_id);
            copy_field(&mut ev.content_type, &e.content_type);
            ev.timestamp_ms = e.timestamp_ms;
            ev.session_policy = e.session_policy as c_int;

            let text = e.text.as_deref().map(cstring);
            let payload = e.payload_json.as_deref().map(cstring);
            ev.text = text
                .as_ref()
                .map_or(ptr::null_mut(), |c| c.as_ptr() as *mut c_char);
            ev.payload_json = payload
                .as_ref()
                .map_or(ptr::null_mut(), |c| c.as_ptr() as *mut c_char);

            unsafe { claw_event_router_publish(&ev) }
        }
    }
}
