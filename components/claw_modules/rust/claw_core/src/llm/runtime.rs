//! LLM runtime, port of `claw_llm_runtime.c`.
//!
//! Holds the resolved config, the derived model profile, the constructed
//! backend, and the injected [`ClawHttp`]. The `init` here applies the same
//! defaulting logic as the C `claw_llm_runtime_init`.

use core::sync::atomic::AtomicBool;
use std::sync::Arc;

use claw_interfaces::error::{ESP_ERR_INVALID_ARG, ESP_ERR_NOT_SUPPORTED};
use claw_interfaces::http::ClawHttp;

use super::backend::{find_builtin_registration, LlmBackend};
use super::types::{ChatRequest, LlmError, LlmResponse, MediaRequest, ModelProfile, RuntimeConfig};

const DEFAULT_TIMEOUT_MS: u32 = 120 * 1000;
const DEFAULT_MAX_TOKENS: u32 = 8192;
const DEFAULT_IMAGE_MAX_BYTES: usize = 512 * 1024;

pub struct LlmRuntime {
    profile: ModelProfile,
    backend: Box<dyn LlmBackend>,
    http: Arc<dyn ClawHttp>,
}

fn empty(value: &Option<String>) -> bool {
    value.as_deref().map(|s| s.is_empty()).unwrap_or(true)
}

impl LlmRuntime {
    /// `claw_llm_runtime_init`
    pub fn init(mut config: RuntimeConfig, http: Arc<dyn ClawHttp>) -> Result<LlmRuntime, LlmError> {
        if config.api_key.is_none() || config.model.is_none() || config.backend_type.is_empty() {
            return Err(LlmError::new(ESP_ERR_INVALID_ARG, "invalid runtime config"));
        }

        let registration = find_builtin_registration(&config.backend_type)
            .ok_or_else(|| LlmError::new(ESP_ERR_NOT_SUPPORTED, "Unknown LLM backend type"))?;

        // Apply defaults exactly as the C code does.
        if empty(&config.auth_type) {
            config.auth_type = Some(registration.defaults.auth_type.to_string());
        }
        if config.timeout_ms == 0 {
            config.timeout_ms = DEFAULT_TIMEOUT_MS;
        }
        if config.max_tokens == 0 {
            config.max_tokens = DEFAULT_MAX_TOKENS;
        }
        if config.image_max_bytes == 0 {
            config.image_max_bytes = DEFAULT_IMAGE_MAX_BYTES;
        }
        if empty(&config.max_tokens_field) {
            config.max_tokens_field = Some(registration.defaults.max_tokens_field.to_string());
        }

        let profile = ModelProfile {
            chat_path: registration.defaults.chat_path.to_string(),
            max_tokens_field: config.max_tokens_field.clone().unwrap_or_default(),
            supports_tools: config.supports_tools,
            supports_vision: config.supports_vision,
            image_remote_url_only: config.image_remote_url_only,
        };

        let backend = (registration.make)(&config)?;

        Ok(LlmRuntime { profile, backend, http })
    }

    /// `claw_llm_runtime_chat`
    pub fn chat(&self, request: &ChatRequest, abort: &AtomicBool) -> Result<LlmResponse, LlmError> {
        self.backend.chat(self.http.as_ref(), &self.profile, request, abort)
    }

    /// `claw_llm_runtime_infer_media`
    pub fn infer_media(&self, request: &MediaRequest, abort: &AtomicBool) -> Result<String, LlmError> {
        self.backend.infer_media(self.http.as_ref(), &self.profile, request, abort)
    }

    pub fn profile(&self) -> &ModelProfile {
        &self.profile
    }
}
