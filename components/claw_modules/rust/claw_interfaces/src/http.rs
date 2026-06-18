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

// ===========================================================================
// Reference implementations (feature-gated)
// ===========================================================================
//
// Host-only `ClawHttp` backends kept beside the trait so the duplicated doubles
// live in one place. They are NOT part of the platform-free seam, so each is
// opt-in and must never be enabled in a device build:
//
// - `httpmock`: scripted / failing / never-called test doubles.
// - `realhttp`: a blocking `reqwest` client for the host CLIs and live tests.

#[cfg(feature = "httpmock")]
mod httpmock {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, MutexGuard};

    use super::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};

    /// One scripted LLM round: a 200 body, or a transport-level error message.
    pub type ScriptStep = Result<String, String>;

    fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn respond(step: ScriptStep) -> Result<HttpResponse, HttpError> {
        match step {
            Ok(body) => Ok(HttpResponse {
                status_code: 200,
                body,
            }),
            Err(message) => Err(HttpError::RequestFailed(message)),
        }
    }

    fn into_steps(bodies: impl IntoIterator<Item = impl Into<String>>) -> VecDeque<ScriptStep> {
        bodies.into_iter().map(|body| Ok(body.into())).collect()
    }

    /// Replays scripted rounds in order; panics if the LLM is called more times
    /// than scripted, so an unexpected round fails loudly instead of silently.
    pub struct ScriptedHttp {
        steps: Mutex<VecDeque<ScriptStep>>,
    }

    impl ScriptedHttp {
        /// Script of all-success bodies served in order.
        pub fn new(bodies: impl IntoIterator<Item = impl Into<String>>) -> Self {
            Self {
                steps: Mutex::new(into_steps(bodies)),
            }
        }

        /// Script of mixed success / transport-error rounds served in order.
        pub fn with_steps(steps: impl IntoIterator<Item = ScriptStep>) -> Self {
            Self {
                steps: Mutex::new(steps.into_iter().collect()),
            }
        }
    }

    impl ClawHttp for ScriptedHttp {
        fn post_json(
            &self,
            _request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            let step = guard(&self.steps)
                .pop_front()
                .expect("ScriptedHttp: LLM called more times than scripted");
            respond(step)
        }
    }

    /// Like [`ScriptedHttp`] but records every request body so a test can assert
    /// on the context the agent sent to the model.
    pub struct CapturingHttp {
        steps: Mutex<VecDeque<ScriptStep>>,
        captured: Mutex<Vec<String>>,
    }

    impl CapturingHttp {
        /// Script of all-success bodies; returns an `Arc` for sharing with the LLM.
        pub fn new(bodies: impl IntoIterator<Item = impl Into<String>>) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(into_steps(bodies)),
                captured: Mutex::new(Vec::new()),
            })
        }

        /// Script of mixed success / transport-error rounds.
        pub fn with_steps(steps: impl IntoIterator<Item = ScriptStep>) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(steps.into_iter().collect()),
                captured: Mutex::new(Vec::new()),
            })
        }

        /// Raw request bodies the agent sent, in call order.
        pub fn captured(&self) -> Vec<String> {
            guard(&self.captured).clone()
        }

        /// Request bodies parsed as JSON, in call order (unparseable → `Null`).
        pub fn captured_bodies(&self) -> Vec<serde_json::Value> {
            guard(&self.captured)
                .iter()
                .map(|body| serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
                .collect()
        }

        /// Number of LLM calls made so far.
        pub fn call_count(&self) -> usize {
            guard(&self.captured).len()
        }
    }

    impl ClawHttp for CapturingHttp {
        fn post_json(
            &self,
            request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            guard(&self.captured).push(request.body.to_string());
            let step = guard(&self.steps)
                .pop_front()
                .expect("CapturingHttp: LLM called more times than scripted");
            respond(step)
        }
    }

    /// Always fails the LLM round with a transport error.
    pub struct FailingHttp;

    impl ClawHttp for FailingHttp {
        fn post_json(
            &self,
            _request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            Err(HttpError::RequestFailed("simulated failure".into()))
        }
    }

    /// Returns a no-op transport error; for wiring where the LLM is never driven.
    pub struct NoopHttp;

    impl ClawHttp for NoopHttp {
        fn post_json(
            &self,
            _request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            Err(HttpError::RequestFailed("noop".into()))
        }
    }

    /// Panics if called — for tests that must never reach the LLM.
    pub struct NeverHttp;

    impl ClawHttp for NeverHttp {
        fn post_json(
            &self,
            _request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            panic!("NeverHttp: the LLM must not be called in this test");
        }
    }
}

#[cfg(feature = "httpmock")]
pub use httpmock::{
    CapturingHttp, FailingHttp, NeverHttp, NoopHttp, ScriptStep, ScriptedHttp,
};

#[cfg(feature = "realhttp")]
mod realhttp {
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};

    /// Host [`ClawHttp`] backed by a blocking `reqwest` client.
    ///
    /// The single real transport for host CLIs and live/integration tests:
    /// honours `request.auth_type` (`"api-key"` / `"none"` / bearer), forwards
    /// extra headers, polls `abort` before sending, and treats any 2xx as success.
    /// An optional `User-Agent` lets a test route requests by client identity.
    #[derive(Debug, Clone, Default)]
    pub struct RealHttp {
        user_agent: Option<String>,
    }

    impl RealHttp {
        /// A client with no extra `User-Agent`.
        pub fn new() -> Self {
            Self::default()
        }

        /// A client that sends the given `User-Agent` on every request.
        pub fn with_user_agent(user_agent: impl Into<String>) -> Self {
            Self {
                user_agent: Some(user_agent.into()),
            }
        }
    }

    impl ClawHttp for RealHttp {
        fn post_json(
            &self,
            request: &HttpJsonRequest,
            abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            if abort.load(Ordering::Acquire) {
                return Err(HttpError::Aborted);
            }

            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_millis(u64::from(request.timeout_ms)))
                .build()
                .map_err(|_| HttpError::ClientInitFailed)?;

            let mut builder = client
                .post(request.url)
                .header("Content-Type", "application/json")
                .body(request.body.to_string());

            if let Some(user_agent) = &self.user_agent {
                builder = builder.header("User-Agent", user_agent);
            }

            if let Some(api_key) = request.api_key.filter(|value| !value.is_empty()) {
                match request.auth_type.unwrap_or("bearer") {
                    "api-key" => builder = builder.header("api-key", api_key),
                    "none" => {}
                    _ => builder = builder.header("Authorization", format!("Bearer {api_key}")),
                }
            }
            for header in request.headers {
                builder = builder.header(header.name, header.value);
            }

            let response = builder
                .send()
                .map_err(|error| HttpError::RequestFailed(error.to_string()))?;
            let status_code = i32::from(response.status().as_u16());
            let body = response
                .text()
                .map_err(|error| HttpError::RequestFailed(error.to_string()))?;

            if !(200..300).contains(&status_code) {
                return Err(HttpError::UnexpectedStatus(format!(
                    "HTTP {status_code}: {body}"
                )));
            }
            Ok(HttpResponse { status_code, body })
        }
    }
}

#[cfg(feature = "realhttp")]
pub use realhttp::RealHttp;
