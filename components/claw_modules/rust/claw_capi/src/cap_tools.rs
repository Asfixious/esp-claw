//! C `claw_cap_build_llm_tools_json` + registry invoke → [`Tools`] port.

use std::ffi::CString;
use std::sync::Arc;

#[cfg(target_os = "espidf")]
use claw_cap::c_bridge::turn_to_capability_context;
use claw_cap::tools_adapter::{InvokerToolsDyn, ToolCatalogSource};
use claw_core::{ToolCatalog, Tools, TurnContext};

use crate::capability::default_registry;

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
    core: *mut libc::c_void,
    caller: u32,
}

fn cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

/// Builds tool JSON via `claw_cap_build_llm_tools_json`.
pub struct CLegacyToolCatalog;

impl ToolCatalogSource for CLegacyToolCatalog {
    fn catalog(&self, ctx: &TurnContext) -> ToolCatalog {
        #[cfg(not(target_os = "espidf"))]
        {
            let _ = ctx;
            return ToolCatalog::default();
        }

        #[cfg(target_os = "espidf")]
        {
            extern "C" {
                fn claw_cap_build_llm_tools_json(
                    context: *const ClawCapCallContext,
                    wrap_for_responses_api: bool,
                ) -> *mut libc::c_char;
            }

            let cap = turn_to_capability_context(ctx);
            let call_context = ClawCapCallContext {
                request_id: cap.request_id,
                session_id: cstring(cap.session_id.as_deref().unwrap_or("")).as_ptr(),
                channel: cstring(cap.source_channel.as_deref().unwrap_or("")).as_ptr(),
                chat_id: cstring(cap.source_chat_id.as_deref().unwrap_or("")).as_ptr(),
                target_channel: cstring(cap.target_channel.as_deref().unwrap_or("")).as_ptr(),
                target_chat_id: cstring(cap.target_chat_id.as_deref().unwrap_or("")).as_ptr(),
                source_cap: cstring(cap.source_capability.as_deref().unwrap_or("")).as_ptr(),
                correlation_id: std::ptr::null(),
                core: std::ptr::null_mut(),
                caller: match cap.caller {
                    claw_cap::CapabilityCaller::System => 0,
                    claw_cap::CapabilityCaller::Agent => 1,
                    claw_cap::CapabilityCaller::Console => 2,
                    claw_cap::CapabilityCaller::SubAgent => 3,
                },
            };

            let ptr = unsafe { claw_cap_build_llm_tools_json(&call_context, false) };
            if ptr.is_null() {
                return ToolCatalog::default();
            }
            let json = unsafe {
                let s = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
                libc::free(ptr as *mut libc::c_void);
                s
            };
            ToolCatalog {
                llm_json: if json.is_empty() { None } else { Some(json) },
            }
        }
    }
}

/// Device [`CapTools`] wired to the C capability registry.
pub fn legacy_tools(cap_user_ctx: *mut libc::c_void) -> Arc<dyn Tools> {
    let invoker = Arc::new(default_registry(cap_user_ctx));
    let catalog = Arc::new(CLegacyToolCatalog);
    Arc::new(InvokerToolsDyn::new(invoker, catalog))
}
