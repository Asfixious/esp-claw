//! Crate-internal error type for `claw_core`.
//!
//! Internal (non-`extern "C"`) helpers return `Result<_, CoreError>` instead of
//! the C-style `esp_err_t` integer codes. `claw_core` never converts a
//! [`CoreError`] *to* an `esp_err_t`; that outbound mapping lives at the C ABI
//! boundary in `claw_capi::errmap`. What `claw_core` keeps here is the
//! behavioral output it emits for a failed turn: the error-message strings and
//! log lines, reproduced byte-for-byte via [`CoreError::response_message`] /
//! [`CoreError::esp_name`]. [`CoreError::Esp`] still carries a raw `esp_err_t`
//! produced by an injected C callback so the original code/name reaches the
//! boundary and the logs unchanged.

use claw_api::{ChatError, InitError};
use claw_interfaces::error::EspErr;

use crate::errname::esp_err_to_name;

/// Crate-internal agent-core error.
///
/// Variants either map to a fixed `esp_err_t` code or carry a value that is
/// turned into one at the C ABI boundary; [`CoreError::Esp`] preserves a raw
/// `esp_err_t` produced by an injected C callback so the original code reaches
/// the boundary unchanged.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid argument")]
    InvalidArg,
    #[error("invalid state")]
    InvalidState,
    #[error("out of memory")]
    NoMem,
    #[error("operation failed")]
    Fail,
    #[error("not supported")]
    NotSupported,
    /// The tool loop hit its iteration cap (maps to `ESP_ERR_INVALID_STATE`).
    #[error("cap tool iteration limit reached")]
    IterationLimit,
    /// LLM runtime construction failure (config validation / backend select).
    #[error(transparent)]
    Init(InitError),
    /// LLM chat completion failure.
    #[error(transparent)]
    Chat(ChatError),
    /// A raw `esp_err_t` produced by an injected C callback (context provider,
    /// request gate) that must reach the boundary / logs verbatim.
    #[error("{}", esp_err_to_name(*.0))]
    Esp(EspErr),
}

impl CoreError {
    /// The `response.error_message` text for a failed turn, byte-identical to
    /// the C component: chat failures report the [`ChatError`] message, the
    /// iteration-limit guard reports its fixed string, and every other failure
    /// reports the `esp_err_to_name` string of its code.
    pub fn response_message(&self) -> String {
        match self {
            CoreError::Chat(err) => err.to_string(),
            CoreError::IterationLimit => "cap tool iteration limit reached".to_string(),
            other => other.esp_name().to_string(),
        }
    }

    /// The `esp_err_to_name` string for this error (used in the log lines the C
    /// component formatted with `esp_err_to_name`).
    ///
    /// This is the behavioral name string `claw_core` emits, derived directly
    /// per variant; the numeric `esp_err_t` mapping consumed at the C ABI
    /// boundary lives in `claw_capi::errmap`.
    pub fn esp_name(&self) -> &'static str {
        match self {
            CoreError::InvalidArg => "ESP_ERR_INVALID_ARG",
            CoreError::InvalidState | CoreError::IterationLimit => "ESP_ERR_INVALID_STATE",
            CoreError::NoMem => "ESP_ERR_NO_MEM",
            CoreError::Fail => "ESP_FAIL",
            CoreError::NotSupported => "ESP_ERR_NOT_SUPPORTED",
            CoreError::Init(err) => init_error_name(err),
            CoreError::Chat(err) => chat_error_name(err),
            // Raw code from an injected C callback: name it verbatim.
            CoreError::Esp(code) => esp_err_to_name(*code),
        }
    }
}

/// The `esp_err_to_name` string a [`claw_api::InitError`] reports, matching the
/// numeric mapping applied at the C ABI boundary (`claw_capi::errmap`).
fn init_error_name(err: &InitError) -> &'static str {
    match err {
        InitError::MissingApiKey
        | InitError::MissingModel
        | InitError::MissingBaseUrl
        | InitError::MissingBackendType => "ESP_ERR_INVALID_ARG",
        InitError::UnknownBackend => "ESP_ERR_NOT_SUPPORTED",
    }
}

/// The `esp_err_to_name` string a [`claw_api::ChatError`] reports, matching the
/// numeric mapping applied at the C ABI boundary (`claw_capi::errmap`).
fn chat_error_name(err: &ChatError) -> &'static str {
    match err {
        ChatError::ToolsUnsupported => "ESP_ERR_NOT_SUPPORTED",
        ChatError::InvalidToolsJson => "ESP_ERR_INVALID_ARG",
        ChatError::Api(_) => "ESP_FAIL",
    }
}
