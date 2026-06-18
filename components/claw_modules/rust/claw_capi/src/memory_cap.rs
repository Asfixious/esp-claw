//! `claw_memory` capability C ABI (moved from `claw_memory::cap`): the
//! `claw_cap.h` C ABI types, the descriptor/group statics, the `extern "C"`
//! execute trampolines, and `#[no_mangle] claw_memory_register_group`. The
//! trampolines bridge C strings <-> Rust and call the pure capability logic in
//! [`claw_memory::cap`].

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use claw_platform::esp_err_t as EspErr;

use claw_memory::cap::{self, CapCtx};

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

// --- C-string bridge helpers ----------------------------------------------

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

unsafe fn cap_ctx_from_c<'a>(ctx: *const claw_cap_call_context_t) -> Option<CapCtx<'a>> {
    if ctx.is_null() {
        return None;
    }
    let c = &*ctx;
    Some(CapCtx {
        request_id: c.request_id,
        session_id: cstr(c.session_id),
    })
}

// --- execute trampolines --------------------------------------------------

unsafe extern "C" fn cap_memory_store_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let cap_ctx = cap_ctx_from_c(ctx);
    let (status, out) = cap::cap_store(cstr(input_json), cap_ctx.as_ref());
    write_output(output, output_size, &out);
    crate::errmap::cap_status_esp_err(status)
}

unsafe extern "C" fn cap_memory_recall_execute(
    input_json: *const c_char,
    _ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let (status, out) = cap::cap_recall(cstr(input_json));
    write_output(output, output_size, &out);
    crate::errmap::cap_status_esp_err(status)
}

unsafe extern "C" fn cap_memory_list_execute(
    _input_json: *const c_char,
    _ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let (status, out) = cap::cap_list();
    write_output(output, output_size, &out);
    crate::errmap::cap_status_esp_err(status)
}

unsafe extern "C" fn cap_memory_update_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let cap_ctx = cap_ctx_from_c(ctx);
    let (status, out) = cap::cap_update(cstr(input_json), cap_ctx.as_ref());
    write_output(output, output_size, &out);
    crate::errmap::cap_status_esp_err(status)
}

unsafe extern "C" fn cap_memory_forget_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    let cap_ctx = cap_ctx_from_c(ctx);
    let (status, out) = cap::cap_forget(cstr(input_json), cap_ctx.as_ref());
    write_output(output, output_size, &out);
    crate::errmap::cap_status_esp_err(status)
}

// --- register -------------------------------------------------------------

/// `claw_memory_register_group`.
///
/// # Safety
/// Calls into the C `claw_cap` registry; must run on a thread where that
/// registry is initialized.
#[no_mangle]
pub unsafe extern "C" fn claw_memory_register_group() -> EspErr {
    if claw_cap_group_exists(GROUP.0.group_id) {
        return claw_platform::ESP_OK;
    }
    claw_cap_register_group(&GROUP.0)
}
