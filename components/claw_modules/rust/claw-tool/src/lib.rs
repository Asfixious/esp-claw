//! `claw-tool` — the tool framework: define a model-callable tool once, bundle
//! tools into a set, gate and execute calls, and enforce the on-disk tool
//! contract at build time.
//!
//! The crate sits below `claw-core` (and beside `claw-permission`): it knows
//! nothing about agents or the orchestrator, only about *tools*. Three layers:
//!
//! - **define** ([`handler`]): the [`ToolHandler`] trait and the
//!   [`tool_metadata!`] macro that bakes a tool's `name`/`schema`/`usage` from
//!   `resources/tools/<name>/`, plus the cheap-to-clone [`Tool`] value.
//! - **aggregate** ([`set`]): [`ToolGroup`], the per-agent [`ToolSet`] (combined
//!   schema JSON + flat dispatch + the soft-tools [`tool_context`](ToolSet::tool_context)),
//!   and the [`AllowedTools`] phase allow-set.
//! - **execute** ([`runner`]): the [`ToolRunner`] seam — soft-hide gating, the
//!   permission [`ToolGate`], and dispatch — shaped for future async concurrency.
//!
//! The on-disk contract those three rely on (`resources/tools/<name>/` holds
//! exactly `schema.json` + `usage.md`, and the directory name equals the schema's
//! `function.name`) is enforced at build time by [`bake`] — the *same* crate that
//! defines the macro reading those files, so the runtime and build-time halves of
//! the contract can never drift. Dependents call it from their build script via a
//! light `[build-dependencies]` entry with `default-features = false,
//! features = ["bake"]`.

#[cfg(feature = "bake")]
pub mod bake;

#[cfg(feature = "runtime")]
mod handler;
#[cfg(feature = "runtime")]
mod runner;
#[cfg(feature = "runtime")]
mod set;

#[cfg(feature = "runtime")]
pub use handler::{Tool, ToolError, ToolHandler, ToolInvocation, ToolOutput};
#[cfg(feature = "runtime")]
pub use runner::{ApprovalNeeded, CallOutcome, ToolGate, ToolRunner};
#[cfg(feature = "runtime")]
pub use set::{AllowedTools, ToolGroup, ToolSet, ToolSetError, DEFAULT_TOOL_GROUP};
