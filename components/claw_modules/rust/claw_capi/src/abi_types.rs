//! Shared C ABI typedefs (decoupled from legacy `core_abi`).

use core::ffi::c_void;

pub type claw_core_handle_t = *mut c_void;
