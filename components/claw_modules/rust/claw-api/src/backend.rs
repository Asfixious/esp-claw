//! Backend trait + registration, port of the `claw_llm_backend_vtable_t` /
//! `claw_llm_backend_registration_t` plus the built-in registry from
//! `claw_llm_backend_custom.c`.

use core::sync::atomic::AtomicBool;

use claw_interface::http::ClawHttp;
use serde_json::Value;

use super::backends::{anthropic, openai_compatible};
use super::errors::{ChatError, InferMediaError, InitError};
use super::types::{
    ChatJsonRequest, ChatRequest, ClawApiConfig, LlmResponse, MediaRequest, ModelProfile,
};

/// A constructed backend instance (`backend_ctx` + the vtable methods in C).
pub trait LlmBackend: Send + Sync {
    fn chat(
        &self,
        http: &dyn ClawHttp,
        profile: &ModelProfile,
        request: &ChatRequest,
        abort: &AtomicBool,
    ) -> Result<LlmResponse, ChatError>;

    /// Structured JSON chat. OpenAI uses API `response_format` when supported;
    /// Anthropic and others use prompt fallback until API-level support lands.
    fn chat_json(
        &self,
        http: &dyn ClawHttp,
        profile: &ModelProfile,
        request: &ChatJsonRequest<'_>,
        schema_name: &str,
        schema: &Value,
        abort: &AtomicBool,
    ) -> Result<LlmResponse, ChatError>;

    fn infer_media(
        &self,
        http: &dyn ClawHttp,
        profile: &ModelProfile,
        request: &MediaRequest,
        abort: &AtomicBool,
    ) -> Result<String, InferMediaError>;
}

/// `claw_llm_backend_defaults_t`
#[derive(Clone, Copy)]
pub struct BackendDefaults {
    pub auth_type: &'static str,
    pub chat_path: &'static str,
    pub max_tokens_field: &'static str,
}

/// `claw_llm_backend_registration_t` — the factory replaces the `init` vtable
/// entry (it constructs the backend instance from the resolved config).
pub struct BackendRegistration {
    pub defaults: BackendDefaults,
    pub make: fn(&ClawApiConfig) -> Result<Box<dyn LlmBackend>, InitError>,
}

/// Built-in registrations, mirroring `find_builtin_backend_registration`.
pub fn find_builtin_registration(id: &str) -> Option<BackendRegistration> {
    if id.is_empty() {
        return None;
    }
    if id == openai_compatible::ID {
        return Some(openai_compatible::registration());
    }
    if id == anthropic::ID {
        return Some(anthropic::registration());
    }
    None
}
