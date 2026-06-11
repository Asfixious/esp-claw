//! `claw_paths` — boot-time logical storage root registry (pure Rust).
//!
//! Holds the process-global `CLAW_PATH_DATA` / `CLAW_PATH_SYSTEM` root strings.
//! The roots are written once during early boot (before any reader runs),
//! mirroring the original lock-free `static` storage; [`get`] returns a
//! reference into that storage, so it stays valid for the program lifetime.
//!
//! The C ABI (`claw_paths_set/get/join`) lives in `claw_capi::paths`, which
//! wraps this Rust API; this crate itself exposes no C ABI.

use core::cell::UnsafeCell;

/// Logical root indices (`claw_path_root_t` discriminants).
pub const CLAW_PATH_DATA: usize = 0;
pub const CLAW_PATH_SYSTEM: usize = 1;
/// Number of logical roots.
pub const ROOT_MAX: usize = 2;
/// Maximum stored root length, including the implicit NUL the C ABI relies on.
pub const MAX_LEN: usize = 32;

/// Why a [`set`] was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// Root index is out of range.
    InvalidRoot,
    /// The path was empty.
    EmptyPath,
    /// The path did not fit in [`MAX_LEN`] (including the trailing NUL).
    PathTooLong,
}

struct Roots(UnsafeCell<[[u8; MAX_LEN]; ROOT_MAX]>);
// Safety: written once at boot before any reader runs; matches the C contract.
unsafe impl Sync for Roots {}

static ROOTS: Roots = Roots(UnsafeCell::new([[0u8; MAX_LEN]; ROOT_MAX]));

/// Store `path` for `root`. Rejects out-of-range roots, empty paths, and paths
/// that do not leave room for a trailing NUL within [`MAX_LEN`].
///
/// Must be called during early boot before any concurrent [`get`] (the C
/// contract); the storage is lock-free.
pub fn set(root: usize, path: &str) -> Result<(), PathError> {
    if root >= ROOT_MAX {
        return Err(PathError::InvalidRoot);
    }
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Err(PathError::EmptyPath);
    }
    // strlcpy success requires strlen(src) < buffer size.
    if bytes.len() >= MAX_LEN {
        return Err(PathError::PathTooLong);
    }
    // Safety: see `Roots` — boot-time single-writer.
    let dst = unsafe { &mut (*ROOTS.0.get())[root] };
    dst.fill(0);
    dst[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

/// The stored path for `root`, or `None` if the root is out of range or unset.
///
/// The returned reference points into process-global storage that is valid for
/// the program lifetime and is always NUL-terminated within its backing buffer.
pub fn get(root: usize) -> Option<&'static str> {
    if root >= ROOT_MAX {
        return None;
    }
    // Safety: see `Roots` — readers run after the boot-time writes.
    let buf = unsafe { &(*ROOTS.0.get())[root] };
    if buf[0] == 0 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(MAX_LEN);
    core::str::from_utf8(&buf[..len]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // claw_paths uses process-global state; serialize the tests that touch it.
    static GUARD: Mutex<()> = Mutex::new(());

    fn reset() {
        // Safety: tests hold GUARD, so this is the only accessor.
        unsafe {
            for r in 0..ROOT_MAX {
                (*ROOTS.0.get())[r].fill(0);
            }
        }
    }

    #[test]
    fn set_get_roundtrip() {
        let _g = GUARD.lock().unwrap();
        reset();
        assert_eq!(set(CLAW_PATH_DATA, "/fatfs"), Ok(()));
        assert_eq!(get(CLAW_PATH_DATA), Some("/fatfs"));
        assert_eq!(get(CLAW_PATH_SYSTEM), None);
    }

    #[test]
    fn set_rejects_bad_args() {
        let _g = GUARD.lock().unwrap();
        reset();
        assert_eq!(set(99, "/x"), Err(PathError::InvalidRoot));
        assert_eq!(set(CLAW_PATH_DATA, ""), Err(PathError::EmptyPath));
        assert_eq!(set(CLAW_PATH_DATA, &"/".repeat(40)), Err(PathError::PathTooLong));
    }

    #[test]
    fn get_rejects_out_of_range_root() {
        let _g = GUARD.lock().unwrap();
        reset();
        assert_eq!(get(ROOT_MAX), None);
    }

    #[test]
    fn max_len_boundary() {
        let _g = GUARD.lock().unwrap();
        reset();
        // 31 chars fit (leaves room for NUL); 32 do not.
        assert_eq!(set(CLAW_PATH_DATA, &"a".repeat(31)), Ok(()));
        assert_eq!(get(CLAW_PATH_DATA), Some("a".repeat(31).as_str()));
        assert_eq!(set(CLAW_PATH_DATA, &"a".repeat(32)), Err(PathError::PathTooLong));
    }
}
