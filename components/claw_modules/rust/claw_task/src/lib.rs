//! `claw_task` — pinned/PSRAM-stack FreeRTOS task wrapper.
//!
//! Port of `claw_task.c`. Kept as a standalone crate purely to preserve the
//! `claw_task_*` C ABI for external capability/Lua consumers; `claw_core` uses
//! `std::thread` for its own agent loop and does not depend on this crate.
//!
//! The FreeRTOS plumbing is gated to the espidf target. The config-resolution
//! and stack-policy logic is portable and host-tested.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_void};

/// `claw_task_stack_policy_t`
pub const CLAW_TASK_STACK_INTERNAL_ONLY: i32 = 0;
pub const CLAW_TASK_STACK_PREFER_PSRAM: i32 = 1;
pub const CLAW_TASK_STACK_PSRAM_ONLY: i32 = 2;

/// `MALLOC_CAP_*` values from `esp_heap_caps.h`.
const MALLOC_CAP_SPIRAM: u32 = 1 << 10;
const MALLOC_CAP_INTERNAL: u32 = 1 << 11;

/// `pdPASS` / `errCOULD_NOT_ALLOCATE_REQUIRED_MEMORY` from FreeRTOS projdefs.h.
const ERR_COULD_NOT_ALLOCATE_REQUIRED_MEMORY: i32 = -1;

/// `TaskFunction_t` = `void (*)(void *)`.
pub type TaskFunction = Option<extern "C" fn(*mut c_void)>;

/// Mirror of `claw_task_config_t`. `priority` is `UBaseType_t` (u32) and
/// `core_id` is `BaseType_t` (i32) on the Xtensa FreeRTOS port.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct claw_task_config_t {
    pub name: *const c_char,
    pub stack_size: u32,
    pub priority: u32,
    pub core_id: i32,
    pub stack_policy: i32,
}

/// Whether PSRAM task stacks are permitted and PSRAM is present.
///
/// Mirrors the C `claw_task_external_memory_available`. The CMake build passes
/// `--cfg claw_freertos_ext_mem` when `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM`
/// is set; otherwise this is always false (the C `#else` branch).
#[cfg(all(target_os = "espidf", claw_freertos_ext_mem))]
fn external_memory_available() -> bool {
    unsafe { heap_caps_get_total_size(MALLOC_CAP_SPIRAM) > 0 }
}

#[cfg(not(all(target_os = "espidf", claw_freertos_ext_mem)))]
fn external_memory_available() -> bool {
    false
}

/// Choose the stack memory caps for a policy. Pure logic; host-testable.
fn memory_caps(policy: i32) -> u32 {
    if policy != CLAW_TASK_STACK_INTERNAL_ONLY && external_memory_available() {
        MALLOC_CAP_SPIRAM
    } else {
        MALLOC_CAP_INTERNAL
    }
}

// Only referenced when PSRAM stacks are permitted; gate it so the non-PSRAM
// build does not flag it as dead.
#[cfg(all(target_os = "espidf", claw_freertos_ext_mem))]
extern "C" {
    fn heap_caps_get_total_size(caps: u32) -> usize;
}

#[cfg(target_os = "espidf")]
extern "C" {
    fn xTaskCreatePinnedToCoreWithCaps(
        task_code: TaskFunction,
        name: *const c_char,
        stack_depth: u32,
        params: *mut c_void,
        priority: u32,
        created_task: *mut *mut c_void,
        core_id: i32,
        memory_caps: u32,
    ) -> i32;
    fn vTaskDeleteWithCaps(task: *mut c_void);
}

// Referenced only by external_memory_available on espidf; silence unused warning
// on the host build where that function is a stub.
#[cfg(not(target_os = "espidf"))]
#[allow(dead_code)]
const _UNUSED_CAPS: u32 = MALLOC_CAP_SPIRAM | MALLOC_CAP_INTERNAL;

/// `BaseType_t claw_task_create(...)`
///
/// The override table in the C source is empty (sentinel only), so the caller
/// config is always used as-is.
#[cfg(target_os = "espidf")]
#[no_mangle]
pub unsafe extern "C" fn claw_task_create(
    config: *const claw_task_config_t,
    task_func: TaskFunction,
    arg: *mut c_void,
    task_handle: *mut *mut c_void,
) -> i32 {
    if config.is_null() {
        return ERR_COULD_NOT_ALLOCATE_REQUIRED_MEMORY;
    }
    let cfg = &*config;
    if cfg.name.is_null() || *cfg.name == 0 || task_func.is_none() || cfg.stack_size == 0 {
        return ERR_COULD_NOT_ALLOCATE_REQUIRED_MEMORY;
    }

    let caps = memory_caps(cfg.stack_policy);
    if cfg.stack_policy == CLAW_TASK_STACK_PSRAM_ONLY && caps != MALLOC_CAP_SPIRAM {
        log::error!("task requires PSRAM stack but PSRAM is unavailable");
        return ERR_COULD_NOT_ALLOCATE_REQUIRED_MEMORY;
    }

    xTaskCreatePinnedToCoreWithCaps(
        task_func,
        cfg.name,
        cfg.stack_size,
        arg,
        cfg.priority,
        task_handle,
        cfg.core_id,
        caps,
    )
}

/// `void claw_task_delete(TaskHandle_t task_handle)`
#[cfg(target_os = "espidf")]
#[no_mangle]
pub unsafe extern "C" fn claw_task_delete(task_handle: *mut c_void) {
    vTaskDeleteWithCaps(task_handle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_internal_when_no_psram() {
        // On the host external_memory_available() is always false.
        assert_eq!(memory_caps(CLAW_TASK_STACK_INTERNAL_ONLY), MALLOC_CAP_INTERNAL);
        assert_eq!(memory_caps(CLAW_TASK_STACK_PREFER_PSRAM), MALLOC_CAP_INTERNAL);
        assert_eq!(memory_caps(CLAW_TASK_STACK_PSRAM_ONLY), MALLOC_CAP_INTERNAL);
    }
}
