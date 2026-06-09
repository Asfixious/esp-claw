//! Platform-neutral description of a VFS driver.
//!
//! The field names and callback signatures mirror `esp_vfs_fs_ops_t` /
//! `esp_vfs_dir_ops_t` (context-pointer variants) from
//! `components/vfs/include/esp_vfs_ops.h`. A portable module fills one of these
//! tables with its `extern "C"` callbacks; the platform implementation maps it
//! onto the concrete ESP-IDF struct and registers it.

use core::ffi::{c_char, c_int, c_long, c_void};

use crate::abi::{dirent, mode_t, off_t, ssize_t, stat, utimbuf, DIR};

pub type WriteOp = unsafe extern "C" fn(*mut c_void, c_int, *const c_void, usize) -> ssize_t;
pub type LseekOp = unsafe extern "C" fn(*mut c_void, c_int, off_t, c_int) -> off_t;
pub type ReadOp = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> ssize_t;
pub type PreadOp = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize, off_t) -> ssize_t;
pub type PwriteOp = unsafe extern "C" fn(*mut c_void, c_int, *const c_void, usize, off_t) -> ssize_t;
pub type OpenOp = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_int) -> c_int;
pub type CloseOp = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
pub type FstatOp = unsafe extern "C" fn(*mut c_void, c_int, *mut stat) -> c_int;
pub type FcntlOp = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int) -> c_int;
pub type IoctlOp = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut c_void) -> c_int;
pub type FsyncOp = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

pub type StatOp = unsafe extern "C" fn(*mut c_void, *const c_char, *mut stat) -> c_int;
pub type LinkOp = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
pub type UnlinkOp = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
pub type RenameOp = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
pub type OpendirOp = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut DIR;
pub type ReaddirOp = unsafe extern "C" fn(*mut c_void, *mut DIR) -> *mut dirent;
pub type ReaddirROp =
    unsafe extern "C" fn(*mut c_void, *mut DIR, *mut dirent, *mut *mut dirent) -> c_int;
pub type TelldirOp = unsafe extern "C" fn(*mut c_void, *mut DIR) -> c_long;
pub type SeekdirOp = unsafe extern "C" fn(*mut c_void, *mut DIR, c_long);
pub type ClosedirOp = unsafe extern "C" fn(*mut c_void, *mut DIR) -> c_int;
pub type MkdirOp = unsafe extern "C" fn(*mut c_void, *const c_char, mode_t) -> c_int;
pub type RmdirOp = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
pub type AccessOp = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
pub type TruncateOp = unsafe extern "C" fn(*mut c_void, *const c_char, off_t) -> c_int;
pub type FtruncateOp = unsafe extern "C" fn(*mut c_void, c_int, off_t) -> c_int;
pub type UtimeOp = unsafe extern "C" fn(*mut c_void, *const c_char, *const utimbuf) -> c_int;

/// Directory-related VFS callbacks (matches `esp_vfs_dir_ops_t`).
#[derive(Clone, Copy)]
pub struct ClawVfsDirOps {
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

/// Top-level VFS callbacks (matches `esp_vfs_fs_ops_t`).
#[derive(Clone, Copy)]
pub struct ClawVfsFsOps {
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
    pub dir: Option<&'static ClawVfsDirOps>,
}
