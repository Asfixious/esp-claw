//! `claw_platform` — the portable boundary for the claw_modules Rust crates.
//!
//! The claw_modules crates are `#![no_std]` and reference *only* this crate for
//! anything that would otherwise be an ESP-IDF or libc call. A concrete
//! implementation (see `claw_platform_espidf`) provides a `&'static dyn
//! ClawPlatform` via [`set_platform`] during boot, and the modules reach it via
//! [`platform`].
//!
//! Keeping this boundary as a trait (rather than direct `extern "C"` calls from
//! every module) makes claw_modules portable to a host test platform later,
//! while the first migration stays a literal 1:1 translation of the C code.

#![no_std]
#![allow(non_camel_case_types)]

pub mod abi;
pub mod consts;
pub mod vfs;

use core::ffi::{c_char, c_int, c_void};
use core::fmt;

pub use abi::{dirent, lock_t, mode_t, off_t, ssize_t, stat, time_t, utimbuf, DIR, FILE};
pub use consts::*;
pub use vfs::{ClawVfsDirOps, ClawVfsFsOps};

/// `esp_log_level_t` values used by the modules.
pub const ESP_LOG_ERROR: u32 = 1;

/// Platform services required by the claw_modules crates.
///
/// All methods take raw pointers and mirror their C counterparts 1:1. They are
/// `unsafe` because they forward to C functions with C contracts: every pointer
/// argument carries the same validity contract as the C function it wraps.
/// Because that contract is uniform across the whole trait, it is documented
/// here once rather than repeated on each method.
#[allow(clippy::missing_safety_doc)]
pub trait ClawPlatform: Sync {
    // --- heap (capability aware) ------------------------------------------
    /// `heap_caps_malloc(size, caps)`
    unsafe fn heap_caps_malloc(&self, size: usize, caps: u32) -> *mut u8;
    /// `heap_caps_calloc(count, size, caps)`
    unsafe fn heap_caps_calloc(&self, count: usize, size: usize, caps: u32) -> *mut u8;
    /// `heap_caps_realloc(ptr, size, caps)`
    unsafe fn heap_caps_realloc(&self, ptr: *mut u8, size: usize, caps: u32) -> *mut u8;
    /// `heap_caps_free(ptr)`
    unsafe fn heap_caps_free(&self, ptr: *mut u8);

    // --- heap (default, used where the C code called plain malloc/free) ----
    /// `malloc(size)`
    unsafe fn malloc(&self, size: usize) -> *mut u8;
    /// `free(ptr)`
    unsafe fn free(&self, ptr: *mut u8);

    // --- newlib retargetable locks ----------------------------------------
    /// `_lock_init(plock)`
    unsafe fn lock_init(&self, lock: *mut lock_t);
    /// `_lock_close(plock)`
    unsafe fn lock_close(&self, lock: *mut lock_t);
    /// `_lock_acquire(plock)`
    unsafe fn lock_acquire(&self, lock: *mut lock_t);
    /// `_lock_release(plock)`
    unsafe fn lock_release(&self, lock: *mut lock_t);

    // --- errno ------------------------------------------------------------
    /// Read the current thread's `errno`.
    fn errno(&self) -> c_int;
    /// Set the current thread's `errno`.
    fn set_errno(&self, value: c_int);

    // --- time -------------------------------------------------------------
    /// `time(NULL)`
    fn time(&self) -> time_t;

    // --- logging ----------------------------------------------------------
    /// `ESP_LOGE`-equivalent. `tag` is a NUL-terminated C string.
    fn log_write(&self, level: u32, tag: *const c_char, args: fmt::Arguments);

    // --- VFS registration -------------------------------------------------
    /// `esp_vfs_register_fs(base_path, vfs, flags, ctx)` where `vfs` is built
    /// from the platform-neutral `ops`.
    unsafe fn vfs_register_fs(
        &self,
        base_path: *const c_char,
        ops: &'static ClawVfsFsOps,
        flags: c_int,
        ctx: *mut c_void,
    ) -> esp_err_t;
    /// `esp_vfs_unregister(base_path)`
    unsafe fn vfs_unregister(&self, base_path: *const c_char) -> esp_err_t;
    /// `esp_err_to_name(err)`
    fn esp_err_to_name(&self, err: esp_err_t) -> *const c_char;

    // --- stdio (external filesystem, used by the fatfs sync helpers) -------
    /// `fopen(path, mode)`
    unsafe fn fopen(&self, path: *const c_char, mode: *const c_char) -> *mut FILE;
    /// `fread(ptr, size, nmemb, stream)`
    unsafe fn fread(&self, ptr: *mut u8, size: usize, nmemb: usize, stream: *mut FILE) -> usize;
    /// `fwrite(ptr, size, nmemb, stream)`
    unsafe fn fwrite(&self, ptr: *const u8, size: usize, nmemb: usize, stream: *mut FILE)
        -> usize;
    /// `fflush(stream)`
    unsafe fn fflush(&self, stream: *mut FILE) -> c_int;
    /// `fsync(fileno(stream))`
    unsafe fn fsync_stream(&self, stream: *mut FILE) -> c_int;
    /// `fclose(stream)`
    unsafe fn fclose(&self, stream: *mut FILE) -> c_int;
    /// `ferror(stream)`
    unsafe fn ferror(&self, stream: *mut FILE) -> c_int;

    // --- POSIX path operations (external filesystem) ----------------------
    /// `unlink(path)`
    unsafe fn unlink(&self, path: *const c_char) -> c_int;
    /// `rename(old, new)`
    unsafe fn rename(&self, old: *const c_char, new: *const c_char) -> c_int;
    /// `mkdir(path, mode)`
    unsafe fn mkdir(&self, path: *const c_char, mode: mode_t) -> c_int;
    /// `stat(path, st)`
    unsafe fn stat(&self, path: *const c_char, st: *mut stat) -> c_int;
    /// `opendir(path)`
    unsafe fn opendir(&self, path: *const c_char) -> *mut DIR;
    /// `readdir(dir)`
    unsafe fn readdir(&self, dir: *mut DIR) -> *mut dirent;
    /// `closedir(dir)`
    unsafe fn closedir(&self, dir: *mut DIR) -> c_int;
}

static mut PLATFORM: Option<&'static dyn ClawPlatform> = None;

/// Install the platform implementation. Call exactly once during boot, before
/// any claw_modules entry point runs.
pub fn set_platform(p: &'static dyn ClawPlatform) {
    // Safety: called once at boot, single-threaded, before any reader.
    unsafe {
        PLATFORM = Some(p);
    }
}

/// Access the installed platform implementation.
///
/// Panics (which aborts on this target) if [`set_platform`] has not been called.
#[inline]
pub fn platform() -> &'static dyn ClawPlatform {
    // Safety: PLATFORM is only written once at boot via set_platform.
    unsafe { PLATFORM }.expect("claw_platform: platform not installed")
}

/// `ESP_LOGE`-equivalent helper used by the modules.
#[macro_export]
macro_rules! claw_loge {
    ($tag:expr, $($arg:tt)*) => {
        $crate::platform().log_write($crate::ESP_LOG_ERROR, $tag, ::core::format_args!($($arg)*))
    };
}
