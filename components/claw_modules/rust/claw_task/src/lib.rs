//! `claw_task` — pinned/PSRAM-stack FreeRTOS task stack policy (pure Rust).
//!
//! This crate holds the portable, host-testable stack-policy logic. The
//! `claw_task_*` C ABI and the FreeRTOS / `heap_caps` FFI live in
//! `claw_capi::task`, which wraps this crate; this crate exposes no C ABI.

/// `claw_task_stack_policy_t`
pub const CLAW_TASK_STACK_INTERNAL_ONLY: i32 = 0;
pub const CLAW_TASK_STACK_PREFER_PSRAM: i32 = 1;
pub const CLAW_TASK_STACK_PSRAM_ONLY: i32 = 2;

/// `MALLOC_CAP_*` values from `esp_heap_caps.h`.
pub const MALLOC_CAP_SPIRAM: u32 = 1 << 10;
pub const MALLOC_CAP_INTERNAL: u32 = 1 << 11;

/// Choose the stack memory caps for a policy, given whether PSRAM task stacks
/// are permitted and PSRAM is present. Pure logic; host-testable.
pub fn memory_caps(policy: i32, external_available: bool) -> u32 {
    if policy != CLAW_TASK_STACK_INTERNAL_ONLY && external_available {
        MALLOC_CAP_SPIRAM
    } else {
        MALLOC_CAP_INTERNAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_internal_when_no_psram() {
        assert_eq!(memory_caps(CLAW_TASK_STACK_INTERNAL_ONLY, false), MALLOC_CAP_INTERNAL);
        assert_eq!(memory_caps(CLAW_TASK_STACK_PREFER_PSRAM, false), MALLOC_CAP_INTERNAL);
        assert_eq!(memory_caps(CLAW_TASK_STACK_PSRAM_ONLY, false), MALLOC_CAP_INTERNAL);
    }

    #[test]
    fn caps_psram_when_available_and_permitted() {
        assert_eq!(memory_caps(CLAW_TASK_STACK_INTERNAL_ONLY, true), MALLOC_CAP_INTERNAL);
        assert_eq!(memory_caps(CLAW_TASK_STACK_PREFER_PSRAM, true), MALLOC_CAP_SPIRAM);
        assert_eq!(memory_caps(CLAW_TASK_STACK_PSRAM_ONLY, true), MALLOC_CAP_SPIRAM);
    }
}
