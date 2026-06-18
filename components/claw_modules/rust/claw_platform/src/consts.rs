//! Numeric constants shared across the claw_modules crates.
//!
//! POSIX / newlib constants are re-exported from `libc` (correct for the
//! `espidf` target). ESP-IDF-specific constants that `libc` does not provide are
//! defined here, mirroring the ESP-IDF headers (IDF v5.5.x).

// --- POSIX / newlib (from libc) --------------------------------------------
pub use libc::{
    DT_DIR, DT_REG, EBADF, EBUSY, EEXIST, EINVAL, EISDIR, ENFILE, ENOENT, ENOMEM, ENOSPC, ENOTDIR,
    ENOTEMPTY, ENOTSUP, F_GETFL, F_GETLK, F_SETFL, F_SETLK, F_SETLKW, O_ACCMODE, O_APPEND, O_CREAT,
    O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, SEEK_CUR, SEEK_END, SEEK_SET, S_IFDIR, S_IFMT,
    S_IFREG,
};

// libc's newlib module exposes the per-bit permission flags but not the grouped
// `S_IRWX*` macros, so define them here (same values as `<sys/stat.h>`).
pub const S_IRWXU: u32 = libc::S_IRUSR | libc::S_IWUSR | libc::S_IXUSR;
pub const S_IRWXG: u32 = libc::S_IRGRP | libc::S_IWGRP | libc::S_IXGRP;
pub const S_IRWXO: u32 = libc::S_IROTH | libc::S_IWOTH | libc::S_IXOTH;

// The `as u32` casts are required on the `espidf` target, where libc types
// these `S_IF*` macros as `c_int`; they are redundant on a host build (libc
// types them as `u32` there), hence the `unnecessary_cast` allow.
#[inline]
#[allow(clippy::unnecessary_cast)]
pub fn s_isdir(mode: u32) -> bool {
    (mode & S_IFMT as u32) == S_IFDIR as u32
}

#[inline]
#[allow(clippy::unnecessary_cast)]
pub fn s_isreg(mode: u32) -> bool {
    (mode & S_IFMT as u32) == S_IFREG as u32
}

// --- esp_err_t (components/esp_common/include/esp_err.h) -------------------
pub type esp_err_t = i32;
pub const ESP_OK: esp_err_t = 0;
pub const ESP_FAIL: esp_err_t = -1;
pub const ESP_ERR_NO_MEM: esp_err_t = 0x101;
pub const ESP_ERR_INVALID_ARG: esp_err_t = 0x102;
pub const ESP_ERR_INVALID_STATE: esp_err_t = 0x103;
pub const ESP_ERR_INVALID_SIZE: esp_err_t = 0x104;
pub const ESP_ERR_NOT_FOUND: esp_err_t = 0x105;
pub const ESP_ERR_NOT_SUPPORTED: esp_err_t = 0x106;
pub const ESP_ERR_TIMEOUT: esp_err_t = 0x107;

// --- esp_vfs register flags (components/vfs/include/esp_vfs.h) --------------
pub const ESP_VFS_FLAG_DEFAULT: i32 = 1 << 0;
pub const ESP_VFS_FLAG_CONTEXT_PTR: i32 = 1 << 1;
pub const ESP_VFS_FLAG_STATIC: i32 = 1 << 3;

/// Maximum VFS base path length (`ESP_VFS_PATH_MAX`).
pub const ESP_VFS_PATH_MAX: usize = 15;

// --- heap caps (components/heap/include/esp_heap_caps.h) --------------------
pub const MALLOC_CAP_8BIT: u32 = 1 << 2;
pub const MALLOC_CAP_INTERNAL: u32 = 1 << 11;
pub const MALLOC_CAP_DEFAULT: u32 = 1 << 12;

/// `SIZE_MAX` for overflow checks ported from C.
pub const SIZE_MAX: usize = usize::MAX;

/// `NAME_MAX` as used by claw_ramfs (matches the C `#define NAME_MAX 255`).
pub const NAME_MAX: usize = 255;
