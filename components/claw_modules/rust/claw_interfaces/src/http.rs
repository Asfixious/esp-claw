//! The `ClawHttp` networking injection trait.
//!
//! Replaces `claw_llm_http_transport.c`. The espidf wiring implements this over
//! `esp_http_client`; host tests provide canned responses.

use core::sync::atomic::AtomicBool;

/// A single extra request header (`name: value`).
pub struct HttpHeader<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

/// Parameters for a JSON POST, mirroring `claw_llm_http_json_request_t`.
pub struct HttpJsonRequest<'a> {
    pub url: &'a str,
    pub body: &'a str,
    /// `None` (or empty) disables the auth header.
    pub api_key: Option<&'a str>,
    /// `"bearer"` (default), `"api-key"`, or `"none"`.
    pub auth_type: Option<&'a str>,
    pub timeout_ms: u32,
    pub headers: &'a [HttpHeader<'a>],
}

/// A successful HTTP response (status 200), mirroring `claw_llm_http_response_t`.
pub struct HttpResponse {
    pub status_code: i32,
    pub body: String,
}

/// Rust-native HTTP transport failure.
///
/// `esp_err_t` mapping for the C ABI lives in `claw_capi::errmap::http_esp_err`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP request aborted by caller")]
    Aborted,
    #[error("invalid URL")]
    InvalidUrl,
    #[error("request body contains NUL byte")]
    InvalidBody,
    #[error("failed to create HTTP client")]
    ClientInitFailed,
    #[error("HTTP request failed: {0}")]
    RequestFailed(String),
    /// Non-200 response; message matches C `parse_error_message_body` shape.
    #[error("{0}")]
    UnexpectedStatus(String),
}

/// Networking injection point for the LLM backends.
pub trait ClawHttp: Send + Sync {
    /// Equivalent to `claw_llm_http_post_json`. Returns the body on HTTP 200,
    /// otherwise an [`HttpError`]. `abort` is polled cooperatively; when set the
    /// request is cancelled (mirrors the C `volatile bool *abort_flag`).
    fn post_json(
        &self,
        request: &HttpJsonRequest,
        abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError>;
}
