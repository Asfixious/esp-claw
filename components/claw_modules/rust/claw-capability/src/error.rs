//! Capability-layer errors (no `esp_err_t` in this crate).

/// Failure outcome of capability registration, lifecycle, and dispatch.
///
/// This is the pure-Rust replacement for the `esp_err_t` codes returned by the
/// C capability layer; the future `claw_capi` boundary maps between the two:
///
/// - `InvalidArg`    <-> `ESP_ERR_INVALID_ARG`
/// - `NotFound`      <-> `ESP_ERR_NOT_FOUND`
/// - `AlreadyExists` <-> `ESP_ERR_INVALID_STATE` (id/name conflict on register)
/// - `InvalidState`  <-> `ESP_ERR_INVALID_STATE` (transition while draining/unloading)
/// - `NotAvailable`  <-> `ESP_ERR_INVALID_STATE` (descriptor not Registered/Started)
/// - `NotVisible`    <-> `ESP_ERR_INVALID_STATE` (not exposed to the LLM)
/// - `NotSupported`  <-> `ESP_ERR_NOT_SUPPORTED`
/// - `Timeout`       <-> `ESP_ERR_TIMEOUT` (drain timed out)
/// - `Failed`        <-> generic `ESP_FAIL` / handler failure
///
/// A *handler* failure that should still be shown to the model is carried as
/// `CapabilityInvokeResult { ok: false, .. }`, not as a `CapabilityError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityError {
    #[error("invalid argument")]
    InvalidArg,
    #[error("capability not found")]
    NotFound,
    #[error("capability or group already exists")]
    AlreadyExists,
    #[error("invalid state for this operation")]
    InvalidState,
    #[error("capability not available")]
    NotAvailable,
    #[error("capability not exposed to the LLM")]
    NotVisible,
    #[error("operation not supported")]
    NotSupported,
    #[error("operation timed out")]
    Timeout,
    #[error("operation failed")]
    Failed,
}
