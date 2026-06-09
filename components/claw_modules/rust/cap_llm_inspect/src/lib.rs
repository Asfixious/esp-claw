//! `cap_llm_inspect` — the `inspect_image` capability, a Rust port of
//! `cap_llm_inspect.c`.
//!
//! It registers a single capability with `claw_cap` (still C) whose `execute`
//! callback analyses a local image file by running media inference on the
//! agent core's LLM runtime. The C console command (`cmd_cap_llm_inspect.c`)
//! is unchanged and still calls this capability through `claw_cap_call`.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use claw_core::cabi::{claw_core_handle_t, core_from_handle};
use claw_core::errname::esp_err_to_name;
use claw_core::llm::types::{AssetKind, MediaAsset, MediaRequest};
use claw_interfaces::error::{EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_OK};

const SYSTEM_PROMPT: &[u8] = b"You analyze local image files for the ESP32 claw. \
Describe visible content plainly and briefly. \
If the image is unclear, say what is uncertain instead of guessing.\0";

// --- claw_cap.h C ABI (claw_cap remains C) -------------------------------

type ClawCapExecuteFn = unsafe extern "C" fn(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr;
type ClawCapLifecycleFn = unsafe extern "C" fn() -> EspErr;

#[repr(C)]
struct claw_cap_call_context_t {
    request_id: u32,
    session_id: *const c_char,
    channel: *const c_char,
    chat_id: *const c_char,
    target_channel: *const c_char,
    target_chat_id: *const c_char,
    source_cap: *const c_char,
    correlation_id: *const c_char,
    core: claw_core_handle_t,
    caller: c_int,
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

// --- descriptor / group statics ------------------------------------------

const GROUP_ID: &[u8] = b"cap_llm_inspect\0";
const CAP_ID: &[u8] = b"inspect_image\0";
const CAP_FAMILY: &[u8] = b"system\0";
const CAP_DESCRIPTION: &[u8] =
    b"Analyze a local image from an absolute path. Confirm the path first, then provide a prompt describing what to inspect.\0";
const CAP_INPUT_SCHEMA: &[u8] =
    b"{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"prompt\":{\"type\":\"string\"}},\"required\":[\"path\",\"prompt\"]}\0";

struct GroupSync(claw_cap_group_t);
unsafe impl Sync for GroupSync {}

struct DescriptorsSync([claw_cap_descriptor_t; 1]);
unsafe impl Sync for DescriptorsSync {}

static DESCRIPTORS: DescriptorsSync = DescriptorsSync([claw_cap_descriptor_t {
    id: CAP_ID.as_ptr() as *const c_char,
    name: CAP_ID.as_ptr() as *const c_char,
    family: CAP_FAMILY.as_ptr() as *const c_char,
    description: CAP_DESCRIPTION.as_ptr() as *const c_char,
    kind: CLAW_CAP_KIND_CALLABLE,
    cap_flags: CLAW_CAP_FLAG_CALLABLE_BY_LLM,
    input_schema_json: CAP_INPUT_SCHEMA.as_ptr() as *const c_char,
    init: None,
    start: None,
    stop: None,
    execute: Some(cap_llm_inspect_execute),
}]);

static GROUP: GroupSync = GroupSync(claw_cap_group_t {
    group_id: GROUP_ID.as_ptr() as *const c_char,
    plugin_name: ptr::null(),
    version: ptr::null(),
    descriptors: DESCRIPTORS.0.as_ptr(),
    descriptor_count: 1,
    plugin_ctx: ptr::null_mut(),
    group_init: None,
    group_start: None,
    group_stop: None,
});

// --- helpers --------------------------------------------------------------

/// Copy `s` into the C `output` buffer with NUL termination, truncating to fit.
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
        return None;
    }
    core::ffi::CStr::from_ptr(p).to_str().ok()
}

// --- exported C ABI -------------------------------------------------------

/// `cap_llm_inspect_execute` — analyse a local image referenced by `path` with
/// the LLM, guided by `prompt`.
unsafe extern "C" fn cap_llm_inspect_execute(
    input_json: *const c_char,
    ctx: *const claw_cap_call_context_t,
    output: *mut c_char,
    output_size: usize,
) -> EspErr {
    if input_json.is_null() || output.is_null() || output_size == 0 {
        return ESP_ERR_INVALID_ARG;
    }
    if ctx.is_null() || (*ctx).core.is_null() {
        write_output(output, output_size, "Error: claw_core is not ready");
        return ESP_ERR_INVALID_STATE;
    }

    let input = match cstr(input_json) {
        Some(s) => s,
        None => {
            write_output(output, output_size, "Error: input must be a JSON object");
            return ESP_ERR_INVALID_ARG;
        }
    };
    let root: serde_json::Value = match serde_json::from_str(input) {
        Ok(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
        _ => {
            write_output(output, output_size, "Error: input must be a JSON object");
            return ESP_ERR_INVALID_ARG;
        }
    };
    let path = root.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let prompt = root.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    if path.is_empty() || prompt.is_empty() {
        write_output(output, output_size, "Error: path and prompt are required");
        return ESP_ERR_INVALID_ARG;
    }

    let core = match core_from_handle((*ctx).core) {
        Some(c) => c,
        None => {
            write_output(output, output_size, "Error: claw_core is not ready");
            return ESP_ERR_INVALID_STATE;
        }
    };

    let asset = MediaAsset {
        kind: AssetKind::LocalPath,
        path: Some(path.to_string()),
        url: None,
        bytes: None,
        mime_type: None,
    };
    let system = core::str::from_utf8(&SYSTEM_PROMPT[..SYSTEM_PROMPT.len() - 1]).unwrap();
    let request = MediaRequest {
        system_prompt: Some(system),
        user_prompt: Some(prompt),
        media: core::slice::from_ref(&asset),
    };

    match core.infer_media(&request) {
        Ok(analysis) => {
            write_output(output, output_size, &analysis);
            ESP_OK
        }
        Err(err) => {
            let detail = if err.message.is_empty() {
                String::new()
            } else {
                format!(": {}", err.message)
            };
            let msg = format!(
                "Error: image analysis failed ({}){}",
                esp_err_to_name(err.err),
                detail
            );
            write_output(output, output_size, &msg);
            err.err
        }
    }
}

/// `cap_llm_inspect_register_group`.
#[no_mangle]
pub unsafe extern "C" fn cap_llm_inspect_register_group() -> EspErr {
    if claw_cap_group_exists(GROUP.0.group_id) {
        return ESP_OK;
    }
    claw_cap_register_group(&GROUP.0)
}
