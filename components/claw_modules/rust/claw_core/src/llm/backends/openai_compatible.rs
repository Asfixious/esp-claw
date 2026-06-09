//! OpenAI-compatible backend, port of `claw_llm_backend_openai_compatible.c`.

use core::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use claw_interfaces::error::{ESP_ERR_INVALID_ARG, ESP_ERR_NOT_SUPPORTED, ESP_FAIL};
use claw_interfaces::http::{ClawHttp, HttpJsonRequest};

use super::super::backend::{BackendDefaults, BackendRegistration, LlmBackend};
use super::super::media::prepare_asset;
use super::super::types::{
    ChatRequest, LlmError, LlmResponse, MediaRequest, ModelProfile, RuntimeConfig,
};
use super::common::{join_url, parse_openai_chat_response};

pub const ID: &str = "openai_compatible";
pub const CHAT_PATH: &str = "/chat/completions";
pub const AUTH_TYPE: &str = "bearer";
pub const DEFAULT_MAX_TOKENS_FIELD: &str = "max_tokens";

pub fn registration() -> BackendRegistration {
    BackendRegistration {
        id: ID,
        defaults: BackendDefaults {
            auth_type: AUTH_TYPE,
            chat_path: CHAT_PATH,
            max_tokens_field: DEFAULT_MAX_TOKENS_FIELD,
        },
        make,
    }
}

struct OpenAiCompatible {
    api_key: String,
    model: String,
    base_url: String,
    auth_type: String,
    timeout_ms: u32,
    max_tokens: u32,
    image_max_bytes: usize,
}

/// `openai_compatible_init`
fn make(config: &RuntimeConfig) -> Result<Box<dyn LlmBackend>, LlmError> {
    let api_key = config.api_key.as_deref().unwrap_or("");
    if api_key.is_empty() {
        return Err(LlmError::new(ESP_ERR_INVALID_ARG, "LLM API key is empty"));
    }
    let model = config.model.as_deref().unwrap_or("");
    if model.is_empty() {
        return Err(LlmError::new(ESP_ERR_INVALID_ARG, "LLM model is empty"));
    }
    let base_url = config.base_url.as_deref().unwrap_or("");
    if base_url.is_empty() {
        return Err(LlmError::new(ESP_ERR_INVALID_ARG, "LLM base_url is empty"));
    }
    let auth_type = match config.auth_type.as_deref() {
        Some(a) if !a.is_empty() => a,
        _ => "bearer",
    };

    Ok(Box::new(OpenAiCompatible {
        api_key: api_key.to_string(),
        model: model.to_string(),
        base_url: base_url.to_string(),
        auth_type: auth_type.to_string(),
        timeout_ms: config.timeout_ms,
        max_tokens: config.max_tokens,
        image_max_bytes: config.image_max_bytes,
    }))
}

impl OpenAiCompatible {
    /// `build_chat_body`
    fn build_chat_body(&self, profile: &ModelProfile, request: &ChatRequest) -> Result<String, LlmError> {
        let mut messages: Vec<Value> = Vec::new();
        messages.push(json!({"role": "system", "content": request.system_prompt}));
        if let Some(arr) = request.messages.as_array() {
            messages.extend(arr.iter().cloned());
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(self.model));
        body.insert(profile.max_tokens_field.clone(), json!(self.max_tokens));
        body.insert("messages".to_string(), Value::Array(messages));

        if let Some(tools_json) = request.tools_json.filter(|s| !s.is_empty()) {
            if !profile.supports_tools {
                return Err(LlmError::new(
                    ESP_ERR_NOT_SUPPORTED,
                    "Selected backend does not support tool calls",
                ));
            }
            let tools: Value = serde_json::from_str(tools_json)
                .map_err(|_| LlmError::new(ESP_ERR_INVALID_ARG, "Invalid tools JSON"))?;
            if !tools.is_array() {
                return Err(LlmError::new(ESP_ERR_INVALID_ARG, "Invalid tools JSON"));
            }
            body.insert("tools".to_string(), tools);
        }

        serde_json::to_string(&Value::Object(body))
            .map_err(|_| LlmError::fail("Out of memory serializing request"))
    }
}

impl LlmBackend for OpenAiCompatible {
    /// `openai_compatible_chat`
    fn chat(
        &self,
        http: &dyn ClawHttp,
        profile: &ModelProfile,
        request: &ChatRequest,
        abort: &AtomicBool,
    ) -> Result<LlmResponse, LlmError> {
        let post_data = self.build_chat_body(profile, request)?;
        let url = join_url(&self.base_url, &profile.chat_path);

        let http_request = HttpJsonRequest {
            url: &url,
            body: &post_data,
            api_key: Some(&self.api_key),
            auth_type: Some(&self.auth_type),
            timeout_ms: self.timeout_ms,
            headers: &[],
        };
        let response = http
            .post_json(&http_request, abort)
            .map_err(|e| LlmError::new(e.err, e.message))?;
        parse_openai_chat_response(&response.body)
    }

    /// `openai_compatible_infer_media`
    fn infer_media(
        &self,
        http: &dyn ClawHttp,
        profile: &ModelProfile,
        request: &MediaRequest,
        _abort: &AtomicBool,
    ) -> Result<String, LlmError> {
        if !profile.supports_vision {
            return Err(LlmError::new(
                ESP_ERR_NOT_SUPPORTED,
                "Selected profile does not support media inference",
            ));
        }
        let user_prompt = request.user_prompt.unwrap_or("");
        if user_prompt.is_empty() || request.media.is_empty() {
            return Err(LlmError::new(ESP_ERR_INVALID_ARG, "media request is incomplete"));
        }

        let prepared = prepare_asset(&request.media[0], profile, self.image_max_bytes)?;

        let body = json!({
            "model": self.model,
            // max_tokens field name is dynamic; insert separately below.
        });
        let mut body = body.as_object().unwrap().clone();
        body.insert(profile.max_tokens_field.clone(), json!(self.max_tokens));
        let messages = json!([
            {"role": "system", "content": request.system_prompt.unwrap_or("")},
            {"role": "user", "content": [
                {"type": "text", "text": user_prompt},
                {"type": "image_url", "image_url": {"url": prepared.payload}}
            ]}
        ]);
        body.insert("messages".to_string(), messages);

        let post_data = serde_json::to_string(&Value::Object(body))
            .map_err(|_| LlmError::fail("Out of memory serializing media request"))?;
        let url = join_url(&self.base_url, &profile.chat_path);

        // The C media path does not wire an abort flag.
        let never = AtomicBool::new(false);
        let http_request = HttpJsonRequest {
            url: &url,
            body: &post_data,
            api_key: Some(&self.api_key),
            auth_type: Some(&self.auth_type),
            timeout_ms: self.timeout_ms,
            headers: &[],
        };
        let response = http
            .post_json(&http_request, &never)
            .map_err(|e| LlmError::new(e.err, e.message))?;

        let parsed = parse_openai_chat_response(&response.body)?;
        match parsed.text {
            Some(t) if !t.is_empty() => Ok(t),
            _ => Err(LlmError::new(ESP_FAIL, "LLM returned empty media response")),
        }
    }
}
