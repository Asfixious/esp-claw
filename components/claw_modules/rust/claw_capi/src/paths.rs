//! `claw_paths_*` C ABI, wrapping the pure-Rust `claw_paths` registry.
//!
//! Preserves the exact `claw_paths.h` surface and `esp_err_t` codes. The roots
//! are stored in `claw_paths`; `claw_paths_get` returns a pointer into that
//! lifetime-stable, NUL-terminated storage.

use core::ffi::{c_char, c_int};
use core::ptr;

use claw_platform::{
    esp_err_t as EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_SIZE, ESP_ERR_INVALID_STATE, ESP_OK,
};

use claw_paths::PathError;

/// `claw_path_root_t` discriminants (re-exported for callers that link this ABI).
pub const CLAW_PATH_DATA: c_int = claw_paths::CLAW_PATH_DATA as c_int;
pub const CLAW_PATH_SYSTEM: c_int = claw_paths::CLAW_PATH_SYSTEM as c_int;

#[inline]
fn root_index(root: c_int) -> Option<usize> {
    if (root as u32) < claw_paths::ROOT_MAX as u32 {
        Some(root as usize)
    } else {
        None
    }
}

/// `esp_err_t claw_paths_set(claw_path_root_t root, const char *path)`
///
/// # Safety
/// `path`, when non-null, must point to a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn claw_paths_set(root: c_int, path: *const c_char) -> EspErr {
    let Some(root) = root_index(root) else {
        return ESP_ERR_INVALID_ARG;
    };
    if path.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let Ok(path) = core::ffi::CStr::from_ptr(path).to_str() else {
        return ESP_ERR_INVALID_ARG;
    };
    match claw_paths::set(root, path) {
        Ok(()) => ESP_OK,
        Err(PathError::EmptyPath) | Err(PathError::InvalidRoot) => ESP_ERR_INVALID_ARG,
        Err(PathError::PathTooLong) => ESP_ERR_INVALID_SIZE,
    }
}

/// `const char *claw_paths_get(claw_path_root_t root)`
///
/// # Safety
/// Returns a pointer into process-global storage valid for the program
/// lifetime; the caller must not free it. Sound once boot-time
/// [`claw_paths_set`] writes have completed (the C contract).
#[no_mangle]
pub unsafe extern "C" fn claw_paths_get(root: c_int) -> *const c_char {
    match root_index(root).and_then(claw_paths::get) {
        Some(s) => s.as_ptr() as *const c_char,
        None => ptr::null(),
    }
}

/// `esp_err_t claw_paths_join(claw_path_root_t root, const char *subpath, char *out, size_t out_size)`
///
/// # Safety
/// `subpath`, when non-null, must be a valid NUL-terminated C string, and `out`
/// must point to a writable buffer of at least `out_size` bytes.
#[no_mangle]
pub unsafe extern "C" fn claw_paths_join(
    root: c_int,
    subpath: *const c_char,
    out: *mut c_char,
    out_size: usize,
) -> EspErr {
    let Some(base) = root_index(root).and_then(claw_paths::get) else {
        return ESP_ERR_INVALID_STATE;
    };
    if out.is_null() || out_size == 0 {
        return ESP_ERR_INVALID_ARG;
    }

    let base_bytes = base.as_bytes();
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

    #[test]
    fn join_composes_and_validates() {
        let _g = GUARD.lock().unwrap();
        unsafe {
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

            // too small buffer -> INVALID_SIZE
            let mut tiny = [0u8; 4];
            assert_eq!(
                claw_paths_join(CLAW_PATH_DATA, sub.as_ptr(), tiny.as_mut_ptr() as *mut c_char, tiny.len()),
                ESP_ERR_INVALID_SIZE
            );
        }
    }

    #[test]
    fn get_and_join_reject_unset_or_bad_root() {
        let _g = GUARD.lock().unwrap();
        unsafe {
            assert!(claw_paths_get(99).is_null());
            assert!(claw_paths_get(-1).is_null());
            let sub = CString::new("x").unwrap();
            let mut out = [0u8; 64];
            // Out-of-range root -> base lookup fails -> INVALID_STATE.
            assert_eq!(
                claw_paths_join(99, sub.as_ptr(), out.as_mut_ptr() as *mut c_char, out.len()),
                ESP_ERR_INVALID_STATE
            );
        }
    }

    #[test]
    fn set_rejects_bad_args() {
        let _g = GUARD.lock().unwrap();
        unsafe {
            assert_eq!(claw_paths_set(99, CString::new("/x").unwrap().as_ptr()), ESP_ERR_INVALID_ARG);
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, ptr::null()), ESP_ERR_INVALID_ARG);
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, CString::new("").unwrap().as_ptr()), ESP_ERR_INVALID_ARG);
            let too_long = CString::new("/".repeat(40)).unwrap();
            assert_eq!(claw_paths_set(CLAW_PATH_DATA, too_long.as_ptr()), ESP_ERR_INVALID_SIZE);
        }
    }
}
