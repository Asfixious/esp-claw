//! Worker-thread spawning that mirrors the C `claw_task` behavior.
//!
//! The C firmware created its long-running worker tasks
//! (`xTaskCreatePinnedToCoreWithCaps`) with a PSRAM-backed stack. A bare
//! `std::thread` uses the small default pthread stack in internal RAM and
//! overflows under the agent / extraction workloads (LLM, mbedTLS, serde_json).
//!
//! [`spawn_worker`] applies the requested stack size, priority, core affinity,
//! and PSRAM stack caps to the next `pthread_create` (which `std::thread::spawn`
//! uses on ESP-IDF) via `esp_pthread`, then restores the previous config so
//! unrelated thread spawns are unaffected. On host builds it degrades to a plain
//! named, sized `std::thread`.

use std::io;
use std::thread::JoinHandle;

/// FreeRTOS "no core affinity" sentinel (`tskNO_AFFINITY`). Use this for
/// `core` when the worker should not be pinned.
pub const NO_AFFINITY: i32 = 0x7fff_ffff;

/// Spawns a long-running worker thread with a PSRAM-backed stack (when PSRAM is
/// available), matching the C `claw_task` policy.
pub fn spawn_worker<F>(
    name: &str,
    stack_size: usize,
    priority: u32,
    core: i32,
    f: F,
) -> io::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(target_os = "espidf")]
    {
        let _restore = espidf::apply_cfg(name, stack_size, priority, core);
        // The embedded stack size is tuned for ESP32 frames and PSRAM; esp_pthread
        // (via _restore's cfg) already carries it, and Builder::stack_size pins
        // the pthread attr stack to the same value.
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(stack_size)
            .spawn(f)
    }
    // On host, the small embedded stack sizes (8-16 KiB) would overflow std's
    // deeper frames, so let the platform pick its default (multi-MiB) stack.
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = (stack_size, priority, core);
        std::thread::Builder::new().name(name.to_string()).spawn(f)
    }
}

#[cfg(target_os = "espidf")]
mod espidf {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    // MALLOC_CAP_* bits from esp_heap_caps.h.
    const MALLOC_CAP_8BIT: u32 = 1 << 2;
    const MALLOC_CAP_SPIRAM: u32 = 1 << 10;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EspPthreadCfg {
        stack_size: usize,
        prio: usize,
        inherit_cfg: bool,
        thread_name: *const c_char,
        pin_to_core: c_int,
        stack_alloc_caps: u32,
    }

    extern "C" {
        fn esp_pthread_get_default_config() -> EspPthreadCfg;
        fn esp_pthread_get_cfg(p: *mut EspPthreadCfg) -> c_int;
        fn esp_pthread_set_cfg(cfg: *const EspPthreadCfg) -> c_int;
        fn heap_caps_get_total_size(caps: u32) -> usize;
    }

    /// Restores the prior `esp_pthread` config on drop. Holds the `thread_name`
    /// `CString` alive until then since the config borrows its pointer.
    pub struct CfgGuard {
        previous: EspPthreadCfg,
        had_previous: bool,
        _name: CString,
    }

    impl Drop for CfgGuard {
        fn drop(&mut self) {
            unsafe {
                if self.had_previous {
                    esp_pthread_set_cfg(&self.previous);
                } else {
                    let def = esp_pthread_get_default_config();
                    esp_pthread_set_cfg(&def);
                }
            }
        }
    }

    pub fn apply_cfg(name: &str, stack_size: usize, priority: u32, core: i32) -> CfgGuard {
        unsafe {
            let mut previous = esp_pthread_get_default_config();
            let had_previous = esp_pthread_get_cfg(&mut previous) == 0;

            // Prefer a PSRAM stack when PSRAM is present (the build enables
            // CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM); otherwise let
            // esp_pthread choose a valid internal-RAM default (caps == 0).
            let caps = if heap_caps_get_total_size(MALLOC_CAP_SPIRAM) > 0 {
                MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT
            } else {
                0
            };

            let cname = CString::new(name).unwrap_or_default();
            let mut cfg = esp_pthread_get_default_config();
            cfg.stack_size = stack_size;
            if priority > 0 {
                cfg.prio = priority as usize;
            }
            cfg.inherit_cfg = false;
            cfg.thread_name = cname.as_ptr();
            cfg.pin_to_core = core as c_int;
            cfg.stack_alloc_caps = caps;
            esp_pthread_set_cfg(&cfg);

            CfgGuard { previous, had_previous, _name: cname }
        }
    }
}
