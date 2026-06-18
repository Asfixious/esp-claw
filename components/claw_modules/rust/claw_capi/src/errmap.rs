//! Outbound error-code mapping for the C ABI boundary.
//!
//! The pure logic crates (`claw_core`, `claw_memory`) model failures with Rust
//! error enums and never convert them to `esp_err_t` themselves. This module is
//! the single place that translates those Rust errors into the exact
//! `esp_err_t` integers the original C components returned, so the codes seen by
//! C callers stay byte-identical.
//!
//! Inbound codes are different: a raw `esp_err_t` produced by an injected C
//! callback (e.g. `CoreError::Esp`) is preserved verbatim by the logic crates
//! and is *not* re-mapped here.

use claw_api::{ChatError, InferMediaError, InitError};
use claw_interface::http::HttpError;
use claw_memory::cap::CapStatus;
use claw_memory::error::MemoryError;
use claw_platform::{
    esp_err_t as EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_SIZE, ESP_ERR_INVALID_STATE,
    ESP_ERR_NOT_FOUND, ESP_ERR_NOT_SUPPORTED, ESP_ERR_NO_MEM, ESP_ERR_TIMEOUT, ESP_FAIL, ESP_OK,
};

/// Map a [`MemoryError`] to the `esp_err_t` the C `claw_memory` ABI returns.
pub fn memory_esp_err(err: MemoryError) -> EspErr {
    match err {
        MemoryError::Fail => ESP_FAIL,
        MemoryError::NoMem => ESP_ERR_NO_MEM,
        MemoryError::InvalidArg => ESP_ERR_INVALID_ARG,
        MemoryError::InvalidState => ESP_ERR_INVALID_STATE,
        MemoryError::InvalidSize => ESP_ERR_INVALID_SIZE,
        MemoryError::NotFound => ESP_ERR_NOT_FOUND,
        MemoryError::NotSupported => ESP_ERR_NOT_SUPPORTED,
        MemoryError::Timeout => ESP_ERR_TIMEOUT,
    }
}

/// Map an [`HttpError`] to the `esp_err_t` the C HTTP transport reports.
pub fn http_esp_err(err: &HttpError) -> EspErr {
    match err {
        HttpError::Aborted => ESP_ERR_INVALID_STATE,
        HttpError::InvalidUrl | HttpError::InvalidBody => ESP_ERR_INVALID_ARG,
        HttpError::ClientInitFailed
        | HttpError::RequestFailed(_)
        | HttpError::UnexpectedStatus(_) => ESP_FAIL,
    }
}

/// Map a memory capability handler outcome to the `esp_err_t` the C `claw_cap`
/// execute trampoline returns.
pub fn cap_status_esp_err(status: CapStatus) -> EspErr {
    match status {
        CapStatus::Ok => ESP_OK,
        CapStatus::InvalidArg => ESP_ERR_INVALID_ARG,
        CapStatus::Failed(err) => memory_esp_err(err),
    }
}

/// `claw-api` is platform-agnostic; map its media error to the `esp_err_t` the
/// C `cap_llm_inspect` reports.
pub fn infer_media_error_code(err: &InferMediaError) -> EspErr {
    match err {
        InferMediaError::VisionUnsupported
        | InferMediaError::UnsupportedMediaType
        | InferMediaError::UnsupportedMediaKind
        | InferMediaError::RemoteOnlyProfile
        | InferMediaError::RequiresLocalImage => ESP_ERR_NOT_SUPPORTED,
        InferMediaError::IncompleteRequest
        | InferMediaError::MediaPathEmpty
        | InferMediaError::MediaPathNotAbsolute
        | InferMediaError::MediaUrlEmpty => ESP_ERR_INVALID_ARG,
        InferMediaError::MediaNotFound => ESP_ERR_NOT_FOUND,
        InferMediaError::MediaFileEmpty | InferMediaError::MediaTooLarge => ESP_ERR_INVALID_SIZE,
        InferMediaError::MediaReadFailed
        | InferMediaError::PayloadPrepFailed
        | InferMediaError::Api(_) => ESP_FAIL,
    }
}

/// Maps a [`claw_api::InitError`] to the `esp_err_t` this component reports
/// across its C ABI (`claw-api` itself is platform-agnostic).
fn init_error_code(err: &InitError) -> EspErr {
    match err {
        InitError::MissingApiKey
        | InitError::MissingModel
        | InitError::MissingBaseUrl
        | InitError::MissingBackendType => ESP_ERR_INVALID_ARG,
        InitError::UnknownBackend => ESP_ERR_NOT_SUPPORTED,
    }
}

/// Maps a [`claw_api::ChatError`] to the `esp_err_t` this component reports
/// across its C ABI.
fn chat_error_code(err: &ChatError) -> EspErr {
    match err {
        ChatError::ToolsUnsupported => ESP_ERR_NOT_SUPPORTED,
        ChatError::InvalidToolsJson => ESP_ERR_INVALID_ARG,
        ChatError::Api(_) => ESP_FAIL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_interface::http::HttpError;

    #[test]
    fn http_esp_err_matches_c_transport() {
        assert_eq!(http_esp_err(&HttpError::Aborted), ESP_ERR_INVALID_STATE);
        assert_eq!(http_esp_err(&HttpError::InvalidUrl), ESP_ERR_INVALID_ARG);
        assert_eq!(
            http_esp_err(&HttpError::RequestFailed("timeout".into())),
            ESP_FAIL
        );
    }
}
