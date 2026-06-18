//! `claw_task_*` C ABI, wrapping the pure-Rust `claw_task` stack-policy logic.
//!
//! Holds the `claw_task_config_t` C struct and the FreeRTOS / `heap_caps` FFI;
//! the portable policy decision lives in `claw_task::memory_caps`.

use core::ffi::{c_char, c_void};

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
/// CMake passes `--cfg claw_freertos_ext_mem` when
/// `CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM` is set; otherwise this is always
/// false (the C `#else` branch).
#[cfg(all(target_os = "espidf", claw_freertos_ext_mem))]
fn external_memory_available() -> bool {
    unsafe { heap_caps_get_total_size(claw_task::MALLOC_CAP_SPIRAM) > 0 }
}

#[cfg(not(all(target_os = "espidf", claw_freertos_ext_mem)))]
#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
fn external_memory_available() -> bool {
    false
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

// Referenced only by external_memory_available on espidf; silence unused
// warnings on the host build where that function is a stub.
#[cfg(not(target_os = "espidf"))]
#[allow(dead_code)]
const _UNUSED_CAPS: u32 = claw_task::MALLOC_CAP_SPIRAM | claw_task::MALLOC_CAP_INTERNAL;

/// `BaseType_t claw_task_create(...)`
///
/// The override table in the C source is empty (sentinel only), so the caller
/// config is always used as-is.
///
/// # Safety
/// `config`, when non-null, must point to a valid `claw_task_config_t` with a
/// valid NUL-terminated `name`, and `task_handle` must be writable.
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

    let caps = claw_task::memory_caps(cfg.stack_policy, external_memory_available());
    if cfg.stack_policy == claw_task::CLAW_TASK_STACK_PSRAM_ONLY
        && caps != claw_task::MALLOC_CAP_SPIRAM
    {
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
///
/// # Safety
/// `task_handle` must be a handle returned by `claw_task_create` (or null).
#[cfg(target_os = "espidf")]
#[no_mangle]
pub unsafe extern "C" fn claw_task_delete(task_handle: *mut c_void) {
    vTaskDeleteWithCaps(task_handle);
}
