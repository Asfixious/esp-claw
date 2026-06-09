//! LLM data types, port of `claw_llm_types.h` (the cJSON-bearing structs are
//! re-expressed with `serde_json`).

use claw_interfaces::error::{EspErr, ESP_FAIL};

/// An LLM-layer failure: the `esp_err_t` plus the message the C code would have
/// written to `*out_error_message`.
#[derive(Debug, Clone)]
pub struct LlmError {
    pub err: EspErr,
    pub message: String,
}

impl LlmError {
    pub fn new(err: EspErr, message: impl Into<String>) -> Self {
        LlmError { err, message: message.into() }
    }
    pub fn fail(message: impl Into<String>) -> Self {
        LlmError { err: ESP_FAIL, message: message.into() }
    }
}

/// `claw_llm_tool_call_t`
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

/// `claw_llm_response_t`
#[derive(Clone, Debug, Default)]
pub struct LlmResponse {
    pub text: Option<String>,
    pub reasoning_content: Option<String>,
    pub raw_message_json: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// `claw_llm_runtime_config_t`
#[derive(Clone, Debug, Default)]
pub struct RuntimeConfig {
    pub api_key: Option<String>,
    pub backend_type: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub auth_type: Option<String>,
    pub max_tokens_field: Option<String>,
    pub timeout_ms: u32,
    pub max_tokens: u32,
    pub image_max_bytes: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub image_remote_url_only: bool,
}

/// `claw_llm_model_profile_t`
#[derive(Clone, Debug, Default)]
pub struct ModelProfile {
    pub chat_path: String,
    pub max_tokens_field: String,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub image_remote_url_only: bool,
}

/// `claw_llm_chat_request_t` (the `cJSON *messages` is a JSON array `Value`).
pub struct ChatRequest<'a> {
    pub system_prompt: &'a str,
    pub messages: &'a serde_json::Value,
    pub tools_json: Option<&'a str>,
}

/// `claw_media_asset_kind_t`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    LocalPath,
    RemoteUrl,
    InlineBytes,
}

/// `claw_media_asset_t`
#[derive(Clone, Debug)]
pub struct MediaAsset {
    pub kind: AssetKind,
    pub path: Option<String>,
    pub url: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub mime_type: Option<String>,
}

/// `claw_llm_media_request_t`
pub struct MediaRequest<'a> {
    pub system_prompt: Option<&'a str>,
    pub user_prompt: Option<&'a str>,
    pub media: &'a [MediaAsset],
}

/// `claw_media_prepared_kind_t`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedKind {
    DataUrl,
    RemoteUrl,
}

/// `claw_media_prepared_t`
#[derive(Clone, Debug)]
pub struct Prepared {
    pub kind: PreparedKind,
    pub payload: String,
    pub mime_type: String,
    pub original_size: usize,
}
