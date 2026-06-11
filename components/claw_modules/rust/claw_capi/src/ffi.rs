//! Shared FFI helpers for the C ABI layer: C-allocator access, C-string
//! duplication for `*out` parameters the C side releases with `free`, and the
//! one-time `log` -> `ESP_LOGx` backend install.

use core::ffi::{c_char, c_void};
use core::ptr;

extern "C" {
    pub fn free(ptr: *mut c_void);
    pub fn malloc(size: usize) -> *mut c_void;
}

/// Duplicate `s` into a NUL-terminated C string allocated with the C allocator
/// (`malloc`), so the C caller can release it with `free`. Returns null on
/// allocation failure (or for an oversized length).
///
/// # Safety
/// The returned pointer, when non-null, owns a `malloc`-allocated buffer that
/// the caller must release with `free`.
pub unsafe fn c_strdup(s: &str) -> *mut c_char {
    let bytes = s.as_bytes();
    let buf = malloc(bytes.len() + 1) as *mut u8;
    if buf.is_null() {
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
    *buf.add(bytes.len()) = 0;
    buf as *mut c_char
}

/// Install the `log` facade backend once so the Rust crates' `log::*` calls
/// reach `ESP_LOGx`. Idempotent; safe to call from every C create entry point.
pub fn install_logger_once() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(claw_sys::init_logger);
}
