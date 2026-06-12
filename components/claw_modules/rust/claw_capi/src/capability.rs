//! C `claw_cap` registry backend (second-class adapter into [`claw_cap`]).
//!
//! Rust capability handlers should register on [`claw_cap::Registry`] directly;
//! this module forwards unknown names to the legacy C registry in `claw_cap.c`.

use std::ffi::{c_void, CString};

use claw_cap::context::{CapabilityCaller, CapabilityContext};
use claw_cap::error::CapabilityError;
use claw_cap::invoker::CapabilityInvokeResult;
use claw_cap::registry::RegistryBackend;
use claw_cap::Registry;
use claw_interfaces::error::{EspErr, ESP_ERR_NO_MEM, ESP_OK};

use crate::abi_types::claw_core_handle_t;

fn cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

// --- claw_cap.h (invoke surface only) ------------------------------------

#[repr(C)]
struct ClawCapCallContext {
    request_id: u32,
    session_id: *const libc::c_char,
    channel: *const libc::c_char,
    chat_id: *const libc::c_char,
    target_channel: *const libc::c_char,
    target_chat_id: *const libc::c_char,
    source_cap: *const libc::c_char,
    correlation_id: *const libc::c_char,
    core: claw_core_handle_t,
    caller: u32,
}

#[repr(C)]
struct ClawCapCoreCallUserContext {
    magic: u32,
    core: *mut claw_core_handle_t,
    caller: u32,
}

const CLAW_CAP_CORE_CALL_USER_CTX_MAGIC: u32 = 0x4341_5043;
const CLAW_CAP_CORE_OUTPUT_SIZE: usize = 32 * 1024;

const CLAW_CAP_CALLER_AGENT: u32 = 1;
const CLAW_CAP_CALLER_SUB_AGENT: u32 = 3;

extern "C" {
    fn claw_cap_call(
        id_or_name: *const libc::c_char,
        input_json: *const libc::c_char,
        context: *const ClawCapCallContext,
        output: *mut libc::c_char,
        output_size: usize,
    ) -> EspErr;
}

fn caller_to_c(caller: CapabilityCaller) -> u32 {
    match caller {
        CapabilityCaller::System => 0,
        CapabilityCaller::Agent => CLAW_CAP_CALLER_AGENT,
        CapabilityCaller::Console => 2,
        CapabilityCaller::SubAgent => CLAW_CAP_CALLER_SUB_AGENT,
    }
}

fn invoke_result_from_c(
    error: EspErr,
    output: String,
) -> Result<CapabilityInvokeResult, CapabilityError> {
    if error == ESP_ERR_NO_MEM && output.is_empty() {
        return Err(CapabilityError::NoMem);
    }
    Ok(CapabilityInvokeResult {
        output,
        ok: error == ESP_OK,
    })
}

/// Adapts the C registry (`claw_cap_call`) into [`RegistryBackend`].
pub struct CLegacyRegistryBackend {
    cap_user_ctx: *mut c_void,
}

unsafe impl Send for CLegacyRegistryBackend {}
unsafe impl Sync for CLegacyRegistryBackend {}

impl CLegacyRegistryBackend {
    pub fn new(cap_user_ctx: *mut c_void) -> Self {
        CLegacyRegistryBackend { cap_user_ctx }
    }

    fn fill_call_user_context(&self, call_context: &mut ClawCapCallContext) {
        if self.cap_user_ctx.is_null() {
            call_context.caller = caller_to_c(CapabilityCaller::Agent);
            return;
        }
        let user = self.cap_user_ctx as *const ClawCapCoreCallUserContext;
        unsafe {
            if (*user).magic == CLAW_CAP_CORE_CALL_USER_CTX_MAGIC {
                if !(*user).core.is_null() {
                    call_context.core = *(*user).core;
                }
                call_context.caller = (*user).caller;
                return;
            }
            call_context.core = *(self.cap_user_ctx as *const claw_core_handle_t);
            call_context.caller = caller_to_c(CapabilityCaller::Agent);
        }
    }
}

impl RegistryBackend for CLegacyRegistryBackend {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        #[cfg(not(target_os = "espidf"))]
        {
            let _ = (capability_name, input_json, context);
            return Err(CapabilityError::NotFound);
        }

        #[cfg(target_os = "espidf")]
        {
            let mut call_context = ClawCapCallContext {
                request_id: context.request_id,
                session_id: cstring(context.session_id.as_deref().unwrap_or("")).as_ptr(),
                channel: cstring(context.source_channel.as_deref().unwrap_or("")).as_ptr(),
                chat_id: cstring(context.source_chat_id.as_deref().unwrap_or("")).as_ptr(),
                target_channel: cstring(context.target_channel.as_deref().unwrap_or("")).as_ptr(),
                target_chat_id: cstring(context.target_chat_id.as_deref().unwrap_or("")).as_ptr(),
                source_cap: cstring(context.source_capability.as_deref().unwrap_or("")).as_ptr(),
                correlation_id: std::ptr::null(),
                core: std::ptr::null_mut(),
                caller: caller_to_c(context.caller),
            };
            self.fill_call_user_context(&mut call_context);

            let name = cstring(capability_name);
            let input = cstring(if input_json.is_empty() { "{}" } else { input_json });
            let mut buffer = vec![0u8; CLAW_CAP_CORE_OUTPUT_SIZE];
            let error = unsafe {
                claw_cap_call(
                    name.as_ptr(),
                    input.as_ptr(),
                    &call_context,
                    buffer.as_mut_ptr() as *mut libc::c_char,
                    buffer.len(),
                )
            };
            let nul = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            let output = String::from_utf8_lossy(&buffer[..nul]).into_owned();
            invoke_result_from_c(error, output)
        }
    }
}

/// Build the default Rust-first registry used by [`crate::core_abi::claw_core_create`].
pub fn default_registry(cap_user_ctx: *mut c_void) -> Registry {
    Registry::new(Some(Box::new(CLegacyRegistryBackend::new(cap_user_ctx))))
}
