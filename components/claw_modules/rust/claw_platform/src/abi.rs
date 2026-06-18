//! Target ABI types for the claw_modules crates.
//!
//! The newlib / POSIX layouts (`stat`, `dirent`, `DIR`, `utimbuf`, `timespec`,
//! and the integer typedefs) are taken from the `libc` crate, which has
//! ESP-IDF-validated definitions under `target_os = "espidf"` (and uses the
//! 64-bit `time_t` when built with `--cfg espidf_time64`, set in
//! `.cargo/config.toml`). This avoids hand-rolling fragile ABI structs.
//!
//! Only the handful of types that are ESP-IDF specific (e.g. the newlib
//! retargetable lock handle) are defined locally.

use core::ffi::c_void;

pub use libc::{dirent, ino_t, mode_t, off_t, ssize_t, stat, time_t, timespec, utimbuf, DIR, FILE};

/// `_lock_t` is `struct __lock *` on ESP-IDF (retargetable locking enabled).
/// Not exposed by `libc`, so define it here as an opaque pointer.
pub type lock_t = *mut c_void;
