//! Dependency-injection traits for the agent loop.
//!
//! In C these are function-pointer-plus-`user_ctx` fields on `claw_core_state_t`
//! (`call_cap`, context providers, `persist_context`, `request_gate`,
//! `on_request_start`, `collect_stage_note`, completion observers). Modelling
//! them as Rust traits keeps the loop host-testable with mock implementations;
//! the C ABI layer ([`crate::cabi`]) wraps the C function pointers in adapters
//! that implement these traits.

use claw_interfaces::error::EspErr;

use crate::consts::{ContextKind, ContextRecordType};
use crate::request::RequestItem;

/// `claw_core_call_cap_fn`. Returns the capability `esp_err_t` and the produced
/// output (the C `*out_output`). The loop uses the error for the `is_error`
/// flag and falls back to the error name when no output is produced.
pub trait CapCaller: Send + Sync {
    fn call_cap(
        &self,
        cap_name: &str,
        input_json: &str,
        request: &RequestItem,
    ) -> (EspErr, Option<String>);
}

/// Result of a `claw_core_context_provider_collect_fn` call.
pub enum ProviderOutcome {
    /// `ESP_ERR_NOT_FOUND`: the provider has nothing for this request.
    Skip,
    /// Any other non-OK `esp_err_t`.
    Error(EspErr),
    /// Non-empty content of a given kind.
    Provided { kind: ContextKind, content: String },
}

/// A context provider (`claw_core_context_provider_t`).
pub trait ContextProvider: Send + Sync {
    fn name(&self) -> &str;
    fn flags(&self) -> u32;
    fn collect(&self, request: &RequestItem) -> ProviderOutcome;
}

/// One record in a persistence batch (`claw_core_context_record_t`).
#[derive(Clone, Debug)]
pub struct PersistRecord {
    pub record_type: ContextRecordType,
    pub message_json: Option<String>,
    pub text: Option<String>,
}

/// `claw_core_persist_context_fn`, pre-split into the batch fields.
pub trait PersistContext: Send + Sync {
    fn persist(
        &self,
        session_id: &str,
        request: &RequestItem,
        records: &[PersistRecord],
        turn_completed: bool,
    ) -> EspErr;
}

/// Result of a `claw_core_request_gate_fn` call.
pub enum GateOutcome {
    /// `ESP_OK`: proceed.
    Allow,
    /// Non-OK with a reject message: respond OK with this text.
    Reject(String),
    /// Non-OK without a reject message: respond with the error.
    Error(EspErr),
}

/// `claw_core_request_gate_fn`.
pub trait RequestGate: Send + Sync {
    fn gate(&self, request: &RequestItem) -> GateOutcome;
}

/// `claw_core_request_start_fn`.
pub trait RequestStart: Send + Sync {
    fn on_start(&self, request: &RequestItem) -> EspErr;
}

/// `claw_core_stage_note_fn`. `Ok(None)` mirrors a NULL/empty `*out_note`.
pub trait StageNote: Send + Sync {
    fn collect(&self, request: &RequestItem) -> Result<Option<String>, EspErr>;
}

/// `claw_core_completion_summary_t`.
pub struct CompletionSummary<'a> {
    pub request_id: u32,
    pub session_id: Option<&'a str>,
    pub final_text: Option<&'a str>,
    pub context_providers_csv: &'a str,
    pub tool_calls_csv: &'a str,
}

/// `claw_core_completion_observer_fn`.
pub trait CompletionObserver: Send + Sync {
    fn on_complete(&self, summary: &CompletionSummary);
}
