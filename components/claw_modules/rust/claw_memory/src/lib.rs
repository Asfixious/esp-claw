//! `claw_memory` — long-term + session memory, a faithful Rust port of the
//! `claw_memory` ESP-IDF component. It exports the exact `claw_memory.h` C ABI
//! (`#[no_mangle] extern "C"` functions and the four context-provider statics)
//! so existing C callers link unchanged. The crate is an `rlib`; the symbols
//! are bundled into the firmware staticlib by `claw_rt`.

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

mod api;
mod cabi;
mod cap;
mod consts;
mod extract;
mod item;
mod lightweight;
mod profile;
mod session;
mod state;
mod storage;
mod util;
