//! `claw_interface` — the cycle-breaking base crate for the claw_modules Rust
//! rewrite.
//!
//! The C `claw_modules` are mutually circular (`cap -> core -> router -> cap`).
//! Rust crates cannot depend on each other circularly, so shared types and the
//! dependency-injection traits live here and every module crate depends only on
//! this one, never on each other.

pub mod fs;
pub mod http;

#[cfg(feature = "diskfs")]
pub use fs::DiskFs;
#[cfg(feature = "memfs")]
pub use fs::MemFs;
pub use fs::{ClawFs, FsError};
#[cfg(feature = "httpmock")]
pub use http::{
    BlockingClawHttpAsync, CapturingHttp, FailingHttp, NeverHttp, NoopHttp, ScriptStep,
    ScriptedHttp, YieldingClawHttpAsync,
};
pub use http::{
    Cancel, ClawHttp, ClawHttpAsync, HttpError, HttpHeader, HttpJsonRequest, HttpResponse,
    HttpResponseFuture,
};
#[cfg(feature = "realhttp")]
pub use http::{RealHttp, RealHttpAsync};
