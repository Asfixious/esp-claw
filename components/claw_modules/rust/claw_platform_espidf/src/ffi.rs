//! Raw `extern "C"` declarations for the ESP-IDF-specific symbols used by the
//! claw_modules crates.
//!
//! Newlib / POSIX functions (`fopen`, `stat`, `opendir`, `time`, `malloc`, ...)
//! come from the `libc` crate and are used directly in `lib.rs`. Only symbols
//! that `libc` does not provide are declared here: the heap-capability
//! allocator, the newlib retargetable locks, `errno`, the ESP logging entry
//! point, and the VFS registration API plus its ops structs.
//!
//! The `esp_vfs_fs_ops_t` / `esp_vfs_dir_ops_t` layouts match
//! `components/vfs/include/esp_vfs_ops.h` for a build with
//! `CONFIG_VFS_SUPPORT_DIR`, `CONFIG_VFS_SUPPORT_TERMIOS` and
//! `CONFIG_VFS_SUPPORT_SELECT` all enabled (the project's configuration).

use core::ffi::{c_char, c_int, c_void};

use claw_platform::abi::lock_t;
use claw_platform::consts::esp_err_t;
use claw_platform::vfs::{
    AccessOp, CloseOp, ClosedirOp, FcntlOp, FstatOp, FsyncOp, FtruncateOp, IoctlOp, LinkOp,
    LseekOp, MkdirOp, OpenOp, OpendirOp, PreadOp, PwriteOp, ReadOp, ReaddirOp, ReaddirROp,
    RenameOp, RmdirOp, SeekdirOp, StatOp, TelldirOp, TruncateOp, UnlinkOp, UtimeOp, WriteOp,
};

/// Mirror of `esp_vfs_dir_ops_t`. Each `union { _p; _no_p; }` is a single
/// function-pointer slot, modelled as one `Option<fn>`.
#[repr(C)]
pub struct esp_vfs_dir_ops_t {
    pub stat_p: Option<StatOp>,
    pub link_p: Option<LinkOp>,
    pub unlink_p: Option<UnlinkOp>,
    pub rename_p: Option<RenameOp>,
    pub opendir_p: Option<OpendirOp>,
    pub readdir_p: Option<ReaddirOp>,
    pub readdir_r_p: Option<ReaddirROp>,
    pub telldir_p: Option<TelldirOp>,
    pub seekdir_p: Option<SeekdirOp>,
    pub closedir_p: Option<ClosedirOp>,
    pub mkdir_p: Option<MkdirOp>,
    pub rmdir_p: Option<RmdirOp>,
    pub access_p: Option<AccessOp>,
    pub truncate_p: Option<TruncateOp>,
    pub ftruncate_p: Option<FtruncateOp>,
    pub utime_p: Option<UtimeOp>,
}

/// Mirror of `esp_vfs_fs_ops_t`. `termios` and `select` sub-tables are present
/// in the layout (their Kconfig options are enabled) but we never populate them.
#[repr(C)]
pub struct esp_vfs_fs_ops_t {
    pub write_p: Option<WriteOp>,
    pub lseek_p: Option<LseekOp>,
    pub read_p: Option<ReadOp>,
    pub pread_p: Option<PreadOp>,
    pub pwrite_p: Option<PwriteOp>,
    pub open_p: Option<OpenOp>,
    pub close_p: Option<CloseOp>,
    pub fstat_p: Option<FstatOp>,
    pub fcntl_p: Option<FcntlOp>,
    pub ioctl_p: Option<IoctlOp>,
    pub fsync_p: Option<FsyncOp>,
    pub dir: *const esp_vfs_dir_ops_t,
    pub termios: *const c_void,
    pub select: *const c_void,
}

// SAFETY: the structs only hold function pointers / raw pointers to data that
// lives for 'static. They are shared read-only with the VFS layer.
unsafe impl Sync for esp_vfs_fs_ops_t {}
unsafe impl Sync for esp_vfs_dir_ops_t {}

extern "C" {
    // heap, capability aware (esp_heap_caps.h)
    pub fn heap_caps_malloc(size: usize, caps: u32) -> *mut c_void;
    pub fn heap_caps_calloc(n: usize, size: usize, caps: u32) -> *mut c_void;
    pub fn heap_caps_realloc(ptr: *mut c_void, size: usize, caps: u32) -> *mut c_void;
    pub fn heap_caps_free(ptr: *mut c_void);

    // newlib retargetable locks (sys/lock.h)
    pub fn _lock_init(plock: *mut lock_t);
    pub fn _lock_close(plock: *mut lock_t);
    pub fn _lock_acquire(plock: *mut lock_t);
    pub fn _lock_release(plock: *mut lock_t);

    // errno (errno.h: `#define errno (*__errno())`)
    pub fn __errno() -> *mut c_int;

    // logging (esp_log.h) - variadic
    pub fn esp_log_write(level: u32, tag: *const c_char, format: *const c_char, ...);

    // VFS (esp_vfs.h / esp_vfs_ops.h)
    pub fn esp_vfs_register_fs(
        base_path: *const c_char,
        vfs: *const esp_vfs_fs_ops_t,
        flags: c_int,
        ctx: *mut c_void,
    ) -> esp_err_t;
    pub fn esp_vfs_unregister(base_path: *const c_char) -> esp_err_t;
    pub fn esp_err_to_name(code: esp_err_t) -> *const c_char;
}
