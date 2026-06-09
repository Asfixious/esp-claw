//! `claw_paths` — boot-time logical storage root registry.
//!
//! Direct port of `claw_paths.c`. Exports the exact `claw_paths_*` C ABI. The
//! roots are written once during early boot (before any reader runs), mirroring
//! the C code's lock-free `static` storage; `claw_paths_get` returns a pointer
//! into that storage, so it must stay valid for the program lifetime.

#![allow(non_camel_case_types)]

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int};
use core::ptr;

use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_SIZE, ESP_ERR_INVALID_STATE, ESP_OK,
};

const CLAW_PATHS_MAX_LEN: usize = 32;
const CLAW_PATH_ROOT_MAX: usize = 2;

/// `claw_path_root_t` discriminants.
pub const CLAW_PATH_DATA: c_int = 0;
pub const CLAW_PATH_SYSTEM: c_int = 1;

struct Roots(UnsafeCell<[[u8; CLAW_PATHS_MAX_LEN]; CLAW_PATH_ROOT_MAX]>);
// Safety: written once at boot before any reader; matches the C contract.
unsafe impl Sync for Roots {}

static ROOTS: Roots = Roots(UnsafeCell::new([[0u8; CLAW_PATHS_MAX_LEN]; CLAW_PATH_ROOT_MAX]));

#[inline]
fn root_in_range(root: c_int) -> bool {
    (root as u32) < CLAW_PATH_ROOT_MAX as u32
}

/// `esp_err_t claw_paths_set(claw_path_root_t root, const char *path)`
#[no_mangle]
pub unsafe extern "C" fn claw_paths_set(root: c_int, path: *const c_char) -> EspErr {
    if !root_in_range(root) || path.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let bytes = core::ffi::CStr::from_ptr(path).to_bytes();
    if bytes.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    // strlcpy returns strlen(src); success requires it to be < buffer size.
    if bytes.len() >= CLAW_PATHS_MAX_LEN {
        return ESP_ERR_INVALID_SIZE;
    }
    let dst = &mut (*ROOTS.0.get())[root as usize];
    dst.fill(0);
    dst[..bytes.len()].copy_from_slice(bytes);
    ESP_OK
}

/// `const char *claw_paths_get(claw_path_root_t root)`
#[no_mangle]
pub unsafe extern "C" fn claw_paths_get(root: c_int) -> *const c_char {
    if !root_in_range(root) {
        return ptr::null();
    }
    let buf = &(*ROOTS.0.get())[root as usize];
    if buf[0] == 0 {
        return ptr::null();
    }
    buf.as_ptr() as *const c_char
}

/// `esp_err_t claw_paths_join(claw_path_root_t root, const char *subpath, char *out, size_t out_size)`
#[no_mangle]
pub unsafe extern "C" fn claw_paths_join(
    root: c_int,
    subpath: *const c_char,
    out: *mut c_char,
    out_size: usize,
) -> EspErr {
    let base = claw_paths_get(root);
    if base.is_null() {
        return ESP_ERR_INVALID_STATE;
    }
    if out.is_null() || out_size == 0 {
        return ESP_ERR_INVALID_ARG;
    }

    let base_bytes = core::ffi::CStr::from_ptr(base).to_bytes();
    let sub_bytes: &[u8] = if subpath.is_null() {
        &[]
    } else {
        core::ffi::CStr::from_ptr(subpath).to_bytes()
    };

    // Chars that snprintf would write, excluding the NUL. Success requires
    // `written < out_size`, matching the C check.
    let written = if !sub_bytes.is_empty() {
        base_bytes.len() + 1 + sub_bytes.len()
    } else {
        base_bytes.len()
    };
    if written >= out_size {
        return ESP_ERR_INVALID_SIZE;
    }

    let out_slice = core::slice::from_raw_parts_mut(out as *mut u8, out_size);
    let mut i = 0;
    out_slice[i..i + base_bytes.len()].copy_from_slice(base_bytes);
    i += base_bytes.len();
    if !sub_bytes.is_empty() {
        out_slice[i] = b'/';
        i += 1;
        out_slice[i..i + sub_bytes.len()].copy_from_slice(sub_bytes);
        i += sub_bytes.len();
    }
    out_slice[i] = 0;
    ESP_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::Mutex;

    // claw_paths uses process-global state; serialize the tests that touch it.
    static GUARD: Mutex<()> = Mutex::new(());

    unsafe fn reset() {
        for r in 0..CLAW_PATH_ROOT_MAX {
            (*ROOTS.0.get())[r].fill(0);
        }
    }

    #[test]
    fn set_get_roundtrip() {
        let _g = GUARD.lock().unwrap();
        unsafe {
            reset();
            let p = CString::new("/fatfs").unwrap();
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, p.as_ptr()), ESP_OK);
            let got = claw_paths_get(CLAW_PATH_DATA);
            assert!(!got.is_null());
            assert_eq!(core::ffi::CStr::from_ptr(got).to_str().unwrap(), "/fatfs");
            // unset root returns null
            assert!(claw_paths_get(CLAW_PATH_SYSTEM).is_null());
        }
    }

    #[test]
    fn set_rejects_bad_args() {
        let _g = GUARD.lock().unwrap();
        unsafe {
            reset();
            assert_eq!(claw_paths_set(99, CString::new("/x").unwrap().as_ptr()), ESP_ERR_INVALID_ARG);
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, ptr::null()), ESP_ERR_INVALID_ARG);
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, CString::new("").unwrap().as_ptr()), ESP_ERR_INVALID_ARG);
            let too_long = CString::new("/".repeat(40)).unwrap();
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, too_long.as_ptr()), ESP_ERR_INVALID_SIZE);
        }
    }

    #[test]
    fn join_composes_and_validates() {
        let _g = GUARD.lock().unwrap();
        unsafe {
            reset();
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, CString::new("/fatfs").unwrap().as_ptr()), ESP_OK);

            let mut out = [0u8; 64];
            let sub = CString::new("skills/foo").unwrap();
            assert_eq!(
                claw_paths_join(CLAW_PATH_DATA, sub.as_ptr(), out.as_mut_ptr() as *mut c_char, out.len()),
                ESP_OK
            );
            let s = core::ffi::CStr::from_ptr(out.as_ptr() as *const c_char).to_str().unwrap();
            assert_eq!(s, "/fatfs/skills/foo");

            // NULL subpath -> just the base
            let mut out2 = [0u8; 64];
            assert_eq!(
                claw_paths_join(CLAW_PATH_DATA, ptr::null(), out2.as_mut_ptr() as *mut c_char, out2.len()),
                ESP_OK
            );
            assert_eq!(core::ffi::CStr::from_ptr(out2.as_ptr() as *const c_char).to_str().unwrap(), "/fatfs");

            // unset root -> INVALID_STATE
            assert_eq!(
                claw_paths_join(CLAW_PATH_SYSTEM, sub.as_ptr(), out.as_mut_ptr() as *mut c_char, out.len()),
                ESP_ERR_INVALID_STATE
            );

            // too small buffer -> INVALID_SIZE
            let mut tiny = [0u8; 4];
            assert_eq!(
                claw_paths_join(CLAW_PATH_DATA, sub.as_ptr(), tiny.as_mut_ptr() as *mut c_char, tiny.len()),
                ESP_ERR_INVALID_SIZE
            );
        }
    }
}
