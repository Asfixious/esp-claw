//! Crate-internal error type.
//!
//! Internal (non-`extern "C"`) helpers return `Result<T, MemoryError>` instead
//! of threading raw `esp_err_t` codes. `claw_memory` never converts a
//! [`MemoryError`] *to* an `esp_err_t`; that outbound mapping lives at the C ABI
//! boundary in `claw_capi::errmap`. Each variant still maps 1:1 to an
//! `esp_err_t`, so the codes returned to C callers stay byte-identical to the
//! original C implementation. The variant's `esp_err_to_name` string
//! ([`MemoryError::code_name`]) is the behavioral label embedded in capability
//! error JSON and stays here with the error.

/// Crate-internal error. Mirrors the `esp_err_t` codes the memory component can
/// produce; the boundary maps each variant back to the exact code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    #[error("operation failed")]
    Fail,
    #[error("out of memory")]
    NoMem,
    #[error("invalid argument")]
    InvalidArg,
    #[error("invalid state")]
    InvalidState,
    #[error("invalid size")]
    InvalidSize,
    #[error("not found")]
    NotFound,
    #[error("not supported")]
    NotSupported,
    #[error("timeout")]
    Timeout,
}

impl MemoryError {
    /// The `esp_err_to_name` string for this error, used as the `code` label in
    /// capability error JSON. Derived directly per variant; the numeric
    /// `esp_err_t` mapping consumed at the C ABI boundary lives in
    /// `claw_capi::errmap`.
    pub fn code_name(&self) -> &'static str {
        match self {
            MemoryError::Fail => "ESP_FAIL",
            MemoryError::NoMem => "ESP_ERR_NO_MEM",
            MemoryError::InvalidArg => "ESP_ERR_INVALID_ARG",
            MemoryError::InvalidState => "ESP_ERR_INVALID_STATE",
            MemoryError::InvalidSize => "ESP_ERR_INVALID_SIZE",
            MemoryError::NotFound => "ESP_ERR_NOT_FOUND",
            MemoryError::NotSupported => "ESP_ERR_NOT_SUPPORTED",
            MemoryError::Timeout => "ESP_ERR_TIMEOUT",
        }
    }
}

/// Convenience alias for crate-internal fallible operations.
pub type MemoryResult<T> = Result<T, MemoryError>;
