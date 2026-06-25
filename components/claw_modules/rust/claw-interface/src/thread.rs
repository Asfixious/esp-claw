//! The `ClawThread` worker-spawning injection trait.
//!
//! Long-running worker tasks need a platform-specific spawn policy: on ESP-IDF
//! they want a PSRAM-backed stack with a chosen [`Priority`] and [`CoreAffinity`]
//! (mirroring the C `claw_task`), while on the host a plain `std::thread` is
//! enough. [`ClawThread`] is the static-dispatch seam over that policy. The
//! priority/affinity types are platform-neutral; the concrete RTOS numbers (and
//! the `tskNO_AFFINITY` sentinel) live in the device impl, not in this API.
//!
//! It is deliberately a generics-only trait (the closure parameter `F` is a
//! generic method parameter, so the trait is *not* object-safe). That is on
//! purpose: spawning a worker must not box the worker closure, so callers take a
//! `T: ClawThread` bound rather than a `dyn ClawThread`. Implementors are
//! zero-sized, so the bound adds no size and no indirection.
//!
//! The device implementation (`EspIdfThread`) lives in `claw-sys`; the host
//! implementation ([`StdThread`]) lives here behind the `stdthread` feature.

use std::io;
use std::thread::JoinHandle;

/// Relative scheduling priority of a worker, abstracting the RTOS's numeric
/// priority scale (FreeRTOS `0..configMAX_PRIORITIES`).
///
/// The device implementation maps each level to a concrete RTOS priority; the
/// host ignores it (the OS scheduler decides).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Priority {
    /// Background work that should yield to interactive / system tasks.
    Low,
    /// The default for the agent's background workers.
    #[default]
    Normal,
    /// Latency-sensitive work that should preempt normal background tasks.
    High,
}

/// Which CPU core a worker may run on, abstracting FreeRTOS core pinning.
///
/// Has no effect on the host or on single-core targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoreAffinity {
    /// No pinning — the scheduler may run the worker on any core.
    #[default]
    Any,
    /// Pin the worker to a specific core index (`0`-based).
    Core(u8),
}

/// Spawns long-running worker threads, abstracting the platform threading policy.
///
/// A static-dispatch injection seam: the device impl applies the requested stack
/// size, [`Priority`], [`CoreAffinity`], and a PSRAM-backed stack via
/// `esp_pthread`; the host impl degrades to a plain named `std::thread`.
/// Implemented by zero-sized types, so a `T: ClawThread` field/bound costs
/// nothing at runtime.
pub trait ClawThread: Send + Sync {
    /// Spawn a worker thread named `name` that runs `f` to completion.
    ///
    /// `stack_size`, `priority`, and `affinity` are honored on platforms that
    /// support them and ignored where they have no analogue (the host).
    ///
    /// # Errors
    ///
    /// Returns the platform [`io::Error`] if the worker thread cannot be spawned.
    fn spawn_worker<F>(
        &self,
        name: &str,
        stack_size: usize,
        priority: Priority,
        affinity: CoreAffinity,
        f: F,
    ) -> io::Result<JoinHandle<()>>
    where
        F: FnOnce() + Send + 'static;
}

/// Host implementation of [`ClawThread`] over `std::thread`. Zero-sized.
///
/// The embedded stack sizes (8-16 KiB) would overflow std's deeper frames, so
/// the requested `stack_size` is ignored and the platform default (multi-MiB)
/// stack is used; `priority` / `affinity` have no host analogue.
#[cfg(feature = "stdthread")]
#[derive(Clone, Copy, Default)]
pub struct StdThread;

#[cfg(feature = "stdthread")]
impl ClawThread for StdThread {
    fn spawn_worker<F>(
        &self,
        name: &str,
        stack_size: usize,
        priority: Priority,
        affinity: CoreAffinity,
        f: F,
    ) -> io::Result<JoinHandle<()>>
    where
        F: FnOnce() + Send + 'static,
    {
        let _ = (stack_size, priority, affinity);
        std::thread::Builder::new().name(name.to_string()).spawn(f)
    }
}
