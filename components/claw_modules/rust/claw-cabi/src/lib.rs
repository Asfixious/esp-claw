//! `claw-cabi` — the single outbound C ABI (Rust -> C) for the agent/capability
//! stack.
//!
//! To C, the only concept is a *capability*. The whole surface is three
//! functions in two planes:
//!
//! - **Control plane** — register capabilities: `claw_capability_register` /
//!   `claw_capability_register_group`.
//! - **Data plane** — the receive half of a channel:
//!   `claw_capability_ingress_push` (the mirror of
//!   `claw_core::Orchestrator::push_user_message`; the outbound half is the
//!   capability descriptor's `send` callback).
//!
//! Lifecycle *driving*, queries, and *building* the agent runtime are owned and
//! driven from Rust and are intentionally not exposed to C. This crate also
//! holds the Rust-side bridge that wires the populated
//! [`claw_capability::Registry`] into [`claw_agent`] — that is plain Rust, not
//! ABI.
//!
//! This crate is the one place in the workspace where `unsafe` / `extern "C"`
//! is allowed; every other crate keeps `unsafe_code = "forbid"`.
//!
//! Status: **skeleton only.** The hand-written `include/claw_cabi.h` is the
//! authoritative draft; the ABI functions and the bridge are not implemented
//! yet. See `DESIGN.md` in this directory.
