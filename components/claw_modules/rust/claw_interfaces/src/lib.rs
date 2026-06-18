//! `claw_interfaces` — the cycle-breaking base crate for the claw_modules Rust
//! rewrite.
//!
//! The C `claw_modules` are mutually circular (`cap -> core -> router -> cap`).
//! Rust crates cannot depend on each other circularly, so shared types and the
//! dependency-injection traits live here and every module crate depends only on
//! this one, never on each other.

pub mod error;
pub mod event;
pub mod fs;
pub mod http;

pub use error::*;
pub use event::{ClawEvent, EventPublisher, SessionPolicy};
pub use fs::{ClawFs, FsError};
#[cfg(feature = "diskfs")]
pub use fs::DiskFs;
#[cfg(feature = "memfs")]
pub use fs::MemFs;
pub use http::{ClawHttp, HttpError, HttpHeader, HttpJsonRequest, HttpResponse};
#[cfg(feature = "httpmock")]
pub use http::{CapturingHttp, FailingHttp, NeverHttp, NoopHttp, ScriptStep, ScriptedHttp};
#[cfg(feature = "realhttp")]
pub use http::RealHttp;
