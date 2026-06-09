//! ESP-IDF implementation of [`claw_platform::ClawPlatform`] using raw
//! `extern "C"` FFI to the symbols already present in the firmware image.
//!
//! Install it once during boot with [`install`]; afterwards the portable
//! claw_modules crates reach it through `claw_platform::platform()`.

#![no_std]
#![allow(non_camel_case_types)]

extern crate alloc;

mod ffi;

use core::ffi::{c_char, c_int, c_void};
use core::fmt;

use alloc::boxed::Box;

use claw_platform::abi::{dirent, lock_t, mode_t, stat, time_t, DIR, FILE};
use claw_platform::consts::esp_err_t;
use claw_platform::vfs::{ClawVfsDirOps, ClawVfsFsOps};
use claw_platform::ClawPlatform;

use ffi::{esp_vfs_dir_ops_t, esp_vfs_fs_ops_t};

/// Stateless platform handle; all state lives in ESP-IDF.
pub struct EspPlatform;

/// The singleton platform instance.
pub static ESP_PLATFORM: EspPlatform = EspPlatform;

/// Install the ESP-IDF platform implementation into `claw_platform`.
pub fn install() {
    claw_platform::set_platform(&ESP_PLATFORM);
}

/// Minimal `core::fmt::Write` sink that formats into a fixed stack buffer and
/// silently truncates. Used to bridge `format_args!` to `esp_log_write`.
struct StackFmt<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> StackFmt<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        StackFmt { buf, pos: 0 }
    }
}

impl fmt::Write for StackFmt<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let room = self.buf.len().saturating_sub(self.pos + 1); // keep 1 for NUL
        let take = room.min(bytes.len());
        self.buf[self.pos..self.pos + take].copy_from_slice(&bytes[..take]);
        self.pos += take;
        Ok(())
    }
}

/// Translate the platform-neutral dir ops into the ESP-IDF struct and leak it so
/// it satisfies `ESP_VFS_FLAG_STATIC` (it must outlive the registration). The C
/// implementation used a `static const` table for the same reason.
fn leak_dir_ops(src: &ClawVfsDirOps) -> *const esp_vfs_dir_ops_t {
    let ops = esp_vfs_dir_ops_t {
        stat_p: src.stat_p,
        link_p: src.link_p,
        unlink_p: src.unlink_p,
        rename_p: src.rename_p,
        opendir_p: src.opendir_p,
        readdir_p: src.readdir_p,
        readdir_r_p: src.readdir_r_p,
        telldir_p: src.telldir_p,
        seekdir_p: src.seekdir_p,
        closedir_p: src.closedir_p,
        mkdir_p: src.mkdir_p,
        rmdir_p: src.rmdir_p,
        access_p: src.access_p,
        truncate_p: src.truncate_p,
        ftruncate_p: src.ftruncate_p,
        utime_p: src.utime_p,
    };
    Box::leak(Box::new(ops)) as *const esp_vfs_dir_ops_t
}

fn leak_fs_ops(src: &ClawVfsFsOps) -> *const esp_vfs_fs_ops_t {
    let dir = match src.dir {
        Some(d) => leak_dir_ops(d),
        None => core::ptr::null(),
    };
    let ops = esp_vfs_fs_ops_t {
        write_p: src.write_p,
        lseek_p: src.lseek_p,
        read_p: src.read_p,
        pread_p: src.pread_p,
        pwrite_p: src.pwrite_p,
        open_p: src.open_p,
        close_p: src.close_p,
        fstat_p: src.fstat_p,
        fcntl_p: src.fcntl_p,
        ioctl_p: src.ioctl_p,
        fsync_p: src.fsync_p,
        dir,
        termios: core::ptr::null(),
        select: core::ptr::null(),
    };
    Box::leak(Box::new(ops)) as *const esp_vfs_fs_ops_t
}

impl ClawPlatform for EspPlatform {
    unsafe fn heap_caps_malloc(&self, size: usize, caps: u32) -> *mut u8 {
        ffi::heap_caps_malloc(size, caps) as *mut u8
    }
    unsafe fn heap_caps_calloc(&self, count: usize, size: usize, caps: u32) -> *mut u8 {
        ffi::heap_caps_calloc(count, size, caps) as *mut u8
    }
    unsafe fn heap_caps_realloc(&self, ptr: *mut u8, size: usize, caps: u32) -> *mut u8 {
        ffi::heap_caps_realloc(ptr as *mut c_void, size, caps) as *mut u8
    }
    unsafe fn heap_caps_free(&self, ptr: *mut u8) {
        ffi::heap_caps_free(ptr as *mut c_void)
    }

    unsafe fn malloc(&self, size: usize) -> *mut u8 {
        libc::malloc(size) as *mut u8
    }
    unsafe fn free(&self, ptr: *mut u8) {
        libc::free(ptr as *mut c_void)
    }

    unsafe fn lock_init(&self, lock: *mut lock_t) {
        ffi::_lock_init(lock)
    }
    unsafe fn lock_close(&self, lock: *mut lock_t) {
        ffi::_lock_close(lock)
    }
    unsafe fn lock_acquire(&self, lock: *mut lock_t) {
        ffi::_lock_acquire(lock)
    }
    unsafe fn lock_release(&self, lock: *mut lock_t) {
        ffi::_lock_release(lock)
    }

    fn errno(&self) -> c_int {
        unsafe { *ffi::__errno() }
    }
    fn set_errno(&self, value: c_int) {
        unsafe { *ffi::__errno() = value }
    }

    fn time(&self) -> time_t {
        unsafe { libc::time(core::ptr::null_mut()) }
    }

    fn log_write(&self, level: u32, tag: *const c_char, args: fmt::Arguments) {
        let mut storage = [0u8; 200];
        let mut sink = StackFmt::new(&mut storage);
        let _ = fmt::write(&mut sink, args);
        let nul = sink.pos;
        storage[nul] = 0;
        unsafe {
            ffi::esp_log_write(
                level,
                tag,
                b"%s\n\0".as_ptr() as *const c_char,
                storage.as_ptr() as *const c_char,
            );
        }
    }

    unsafe fn vfs_register_fs(
        &self,
        base_path: *const c_char,
        ops: &'static ClawVfsFsOps,
        flags: c_int,
        ctx: *mut c_void,
    ) -> esp_err_t {
        let fs_ops = leak_fs_ops(ops);
        ffi::esp_vfs_register_fs(base_path, fs_ops, flags, ctx)
    }
    unsafe fn vfs_unregister(&self, base_path: *const c_char) -> esp_err_t {
        ffi::esp_vfs_unregister(base_path)
    }
    fn esp_err_to_name(&self, err: esp_err_t) -> *const c_char {
        unsafe { ffi::esp_err_to_name(err) }
    }

    unsafe fn fopen(&self, path: *const c_char, mode: *const c_char) -> *mut FILE {
        libc::fopen(path, mode)
    }
    unsafe fn fread(&self, ptr: *mut u8, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
        libc::fread(ptr as *mut c_void, size, nmemb, stream)
    }
    unsafe fn fwrite(
        &self,
        ptr: *const u8,
        size: usize,
        nmemb: usize,
        stream: *mut FILE,
    ) -> usize {
        libc::fwrite(ptr as *const c_void, size, nmemb, stream)
    }
    unsafe fn fflush(&self, stream: *mut FILE) -> c_int {
        libc::fflush(stream)
    }
    unsafe fn fsync_stream(&self, stream: *mut FILE) -> c_int {
        libc::fsync(libc::fileno(stream))
    }
    unsafe fn fclose(&self, stream: *mut FILE) -> c_int {
        libc::fclose(stream)
    }
    unsafe fn ferror(&self, stream: *mut FILE) -> c_int {
        libc::ferror(stream)
    }

    unsafe fn unlink(&self, path: *const c_char) -> c_int {
        libc::unlink(path)
    }
    unsafe fn rename(&self, old: *const c_char, new: *const c_char) -> c_int {
        libc::rename(old, new)
    }
    unsafe fn mkdir(&self, path: *const c_char, mode: mode_t) -> c_int {
        libc::mkdir(path, mode)
    }
    unsafe fn stat(&self, path: *const c_char, st: *mut stat) -> c_int {
        libc::stat(path, st)
    }
    unsafe fn opendir(&self, path: *const c_char) -> *mut DIR {
        libc::opendir(path)
    }
    unsafe fn readdir(&self, dir: *mut DIR) -> *mut dirent {
        libc::readdir(dir)
    }
    unsafe fn closedir(&self, dir: *mut DIR) -> c_int {
        libc::closedir(dir)
    }
}
