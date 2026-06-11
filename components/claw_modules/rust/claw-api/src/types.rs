//! LLM data types, port of `claw_llm_types.h` (the cJSON-bearing structs are
//! re-expressed with `serde_json`). Error types live in [`crate::errors`].

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

/// `claw_llm_runtime_config_t` — the inputs to [`crate::ClawApi::init`].
#[derive(Clone, Debug, Default)]
pub struct ClawApiConfig {
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

impl<'a> ChatRequest<'a> {
    /// A tool-less chat request.
    pub fn new(system_prompt: &'a str, messages: &'a serde_json::Value) -> Self {
        ChatRequest { system_prompt, messages, tools_json: None }
    }

    /// Attach an OpenAI-style tools JSON array.
    pub fn with_tools(mut self, tools_json: &'a str) -> Self {
        self.tools_json = Some(tools_json);
        self
    }
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

impl MediaAsset {
    /// An asset backed by a local file path.
    pub fn local_path(path: impl Into<String>) -> Self {
        MediaAsset {
            kind: AssetKind::LocalPath,
            path: Some(path.into()),
            url: None,
            bytes: None,
            mime_type: None,
        }
    }

    /// An asset referenced by a remote URL.
    pub fn remote_url(url: impl Into<String>) -> Self {
        MediaAsset {
            kind: AssetKind::RemoteUrl,
            path: None,
            url: Some(url.into()),
            bytes: None,
            mime_type: None,
        }
    }

    /// An asset carrying inline bytes with an explicit MIME type.
    pub fn inline_bytes(bytes: Vec<u8>, mime_type: impl Into<String>) -> Self {
        MediaAsset {
            kind: AssetKind::InlineBytes,
            path: None,
            url: None,
            bytes: Some(bytes),
            mime_type: Some(mime_type.into()),
        }
    }

    /// Override the MIME type (otherwise inferred from the file extension).
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }
}

/// `claw_llm_media_request_t`
pub struct MediaRequest<'a> {
    pub system_prompt: Option<&'a str>,
    pub user_prompt: Option<&'a str>,
    pub media: &'a [MediaAsset],
}

impl<'a> MediaRequest<'a> {
    /// A media request over the given assets, with no prompts set yet.
    pub fn new(media: &'a [MediaAsset]) -> Self {
        MediaRequest { system_prompt: None, user_prompt: None, media }
    }

    pub fn with_system_prompt(mut self, system_prompt: &'a str) -> Self {
        self.system_prompt = Some(system_prompt);
        self
    }

    pub fn with_user_prompt(mut self, user_prompt: &'a str) -> Self {
        self.user_prompt = Some(user_prompt);
        self
    }
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
