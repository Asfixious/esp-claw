//! Anthropic-compatible backend, port of `claw_llm_backend_anthropic.c`.
//!
//! Converts OpenAI-style messages/tools to the Anthropic Messages API shape and
//! parses the Anthropic content-block response back into a [`LlmResponse`].

use core::sync::atomic::AtomicBool;

use serde_json::{json, Map, Value};

use claw_interfaces::error::{
    ESP_ERR_INVALID_ARG, ESP_ERR_NOT_SUPPORTED, ESP_FAIL,
};
use claw_interfaces::http::{ClawHttp, HttpHeader, HttpJsonRequest};

use super::super::backend::{BackendDefaults, BackendRegistration, LlmBackend};
use super::super::media::prepare_asset;
use super::super::types::{
    ChatRequest, LlmError, LlmResponse, MediaRequest, ModelProfile, PreparedKind, RuntimeConfig,
    ToolCall,
};
use super::common::join_url;

pub const ID: &str = "anthropic_compatible";
pub const AUTH_TYPE: &str = "none";
pub const CHAT_PATH: &str = "/messages";
pub const DEFAULT_MAX_TOKENS_FIELD: &str = "max_tokens";
const ANTHROPIC_VERSION: &str = "2023-06-01";

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

struct Anthropic {
    api_key: String,
    model: String,
    base_url: String,
    timeout_ms: u32,
    max_tokens: u32,
    image_max_bytes: usize,
}

/// `anthropic_init`
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
    Ok(Box::new(Anthropic {
        api_key: api_key.to_string(),
        model: model.to_string(),
        base_url: base_url.to_string(),
        timeout_ms: config.timeout_ms,
        max_tokens: config.max_tokens,
        image_max_bytes: config.image_max_bytes,
    }))
}

fn str_field<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(|v| v.as_str())
}

/// `anthropic_make_text_block`
fn make_text_block(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    Some(json!({"type": "text", "text": text}))
}

/// `anthropic_make_tool_use_block`
fn make_tool_use_block(tool_call: &Value) -> Option<Value> {
    if !tool_call.is_object() {
        return None;
    }
    let id = str_field(tool_call, "id")?;
    let function = tool_call.get("function");
    let name = function.and_then(|f| f.get("name")).and_then(|n| n.as_str())?;
    let args = function
        .and_then(|f| f.get("arguments"))
        .and_then(|a| a.as_str());
    let input = match args {
        Some(s) if !s.is_empty() => serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    };
    Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
}

/// `anthropic_duplicate_supported_block`
fn duplicate_supported_block(block: &Value) -> Option<Value> {
    let ty = str_field(block, "type")?;
    matches!(ty, "text" | "tool_use" | "tool_result" | "thinking" | "redacted_thinking")
        .then(|| block.clone())
}

/// `convert_messages_to_anthropic`
fn convert_messages_to_anthropic(messages: &Value) -> Result<Value, LlmError> {
    let mut out: Vec<Value> = Vec::new();
    let arr = match messages.as_array() {
        Some(a) => a,
        None => return Ok(Value::Array(out)),
    };

    let mut idx = 0usize;
    while idx < arr.len() {
        let msg = &arr[idx];
        let role = match str_field(msg, "role") {
            Some(r) if !r.is_empty() => r,
            _ => {
                idx += 1;
                continue;
            }
        };

        // Merge consecutive "tool"-role messages into one "user" message.
        if role == "tool" {
            let mut tool_blocks: Vec<Value> = Vec::new();
            while idx < arr.len() {
                let inner = &arr[idx];
                if str_field(inner, "role") != Some("tool") {
                    break;
                }
                let tid = str_field(inner, "tool_call_id").unwrap_or("");
                let content = str_field(inner, "content").unwrap_or("");
                let is_error = inner.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                tool_blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tid,
                    "content": content,
                    "is_error": is_error,
                }));
                idx += 1;
            }
            out.push(json!({"role": "user", "content": tool_blocks}));
            continue;
        }

        if role != "assistant" && role != "user" {
            idx += 1;
            continue;
        }

        let mut blocks: Vec<Value> = Vec::new();
        let content = msg.get("content");
        match content {
            Some(Value::String(s)) if !s.is_empty() => {
                if let Some(b) = make_text_block(s) {
                    blocks.push(b);
                }
            }
            Some(Value::Array(items)) => {
                for block in items {
                    if let Some(dup) = duplicate_supported_block(block) {
                        blocks.push(dup);
                    }
                }
            }
            _ => {}
        }

        if role == "assistant" {
            if let Some(reasoning) = str_field(msg, "reasoning_content").filter(|s| !s.is_empty()) {
                blocks.insert(0, json!({"type": "thinking", "thinking": reasoning}));
            }
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    match make_tool_use_block(tc) {
                        Some(b) => blocks.push(b),
                        None => {
                            return Err(LlmError::new(
                                claw_interfaces::error::ESP_ERR_NO_MEM,
                                "Out of memory converting messages",
                            ))
                        }
                    }
                }
            }
        }

        if blocks.is_empty() {
            idx += 1;
            continue;
        }

        out.push(json!({"role": role, "content": blocks}));
        idx += 1;
    }

    Ok(Value::Array(out))
}

/// `convert_tools_to_anthropic`. Returns `None` when there are no tools or the
/// JSON is invalid (the caller distinguishes the two).
fn convert_tools_to_anthropic(tools_json: Option<&str>) -> Option<Value> {
    let tools_json = tools_json.filter(|s| !s.is_empty())?;
    let parsed: Value = serde_json::from_str(tools_json).ok()?;
    let arr = parsed.as_array()?;

    let mut out: Vec<Value> = Vec::new();
    for item in arr {
        let (name, desc, schema) = if item.is_object() {
            if str_field(item, "type") == Some("function") {
                let function = item.get("function");
                (
                    function.and_then(|f| f.get("name")),
                    function.and_then(|f| f.get("description")),
                    function.and_then(|f| f.get("parameters")),
                )
            } else {
                (item.get("name"), item.get("description"), item.get("input_schema"))
            }
        } else {
            (None, None, None)
        };

        let name = match name.and_then(|n| n.as_str()).filter(|s| !s.is_empty()) {
            Some(n) => n,
            None => continue,
        };

        let mut tool = Map::new();
        tool.insert("name".to_string(), json!(name));
        if let Some(d) = desc.and_then(|d| d.as_str()) {
            tool.insert("description".to_string(), json!(d));
        }
        match schema {
            Some(s) => tool.insert("input_schema".to_string(), s.clone()),
            None => tool.insert("input_schema".to_string(), json!({})),
        };
        out.push(Value::Object(tool));
    }

    Some(Value::Array(out))
}

/// `parse_data_url`: split `data:<mime>;base64,<data>`.
fn parse_data_url(data_url: &str) -> Option<(String, String)> {
    const PREFIX: &str = "data:";
    const MARKER: &str = ";base64,";
    if !data_url.starts_with(PREFIX) {
        return None;
    }
    let marker_pos = data_url.find(MARKER)?;
    let mime = &data_url[PREFIX.len()..marker_pos];
    let data = &data_url[marker_pos + MARKER.len()..];
    if data.is_empty() {
        return None;
    }
    Some((mime.to_string(), data.to_string()))
}

/// `parse_chat_response` (Anthropic content-block form).
fn parse_chat_response(body: &str) -> Result<LlmResponse, LlmError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|_| LlmError::fail("Failed to parse LLM JSON response"))?;
    let content = match root.get("content") {
        Some(Value::Array(a)) => a,
        _ => return Err(LlmError::fail("LLM response missing content")),
    };

    let raw_message_json = serde_json::to_string(&json!({
        "role": "assistant",
        "content": Value::Array(content.clone()),
    }))
    .map_err(|_| LlmError::fail("Out of memory copying LLM raw message"))?;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in content {
        match str_field(block, "type") {
            Some("text") => {
                if let Some(t) = str_field(block, "text") {
                    text.push_str(t);
                }
            }
            Some("thinking") => {
                if let Some(t) = str_field(block, "thinking") {
                    reasoning.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = str_field(block, "id")
                    .ok_or_else(|| LlmError::fail("Out of memory copying tool call"))?;
                let name = str_field(block, "name")
                    .ok_or_else(|| LlmError::fail("Out of memory copying tool call"))?;
                let arguments_json = match block.get("input") {
                    Some(input) => serde_json::to_string(input)
                        .map_err(|_| LlmError::fail("Out of memory copying tool call"))?,
                    None => "{}".to_string(),
                };
                tool_calls.push(ToolCall { id: id.to_string(), name: name.to_string(), arguments_json });
            }
            _ => {}
        }
    }

    let text_opt = (!text.is_empty()).then_some(text);
    let reasoning_opt = (!reasoning.is_empty()).then_some(reasoning);

    if text_opt.is_none() && tool_calls.is_empty() && reasoning_opt.is_none() {
        return Err(LlmError::new(ESP_FAIL, "LLM returned empty response"));
    }

    Ok(LlmResponse {
        text: text_opt,
        reasoning_content: reasoning_opt,
        raw_message_json: Some(raw_message_json),
        tool_calls,
    })
}

impl Anthropic {
    /// `build_chat_body`
    fn build_chat_body(&self, request: &ChatRequest) -> Result<String, LlmError> {
        let messages = convert_messages_to_anthropic(request.messages)?;

        let mut body = Map::new();
        body.insert("model".to_string(), json!(self.model));
        body.insert("max_tokens".to_string(), json!(self.max_tokens));
        body.insert("system".to_string(), json!(request.system_prompt));
        body.insert("messages".to_string(), messages);

        let tools = convert_tools_to_anthropic(request.tools_json);
        if request.tools_json.map(|s| !s.is_empty()).unwrap_or(false) && tools.is_none() {
            return Err(LlmError::new(ESP_ERR_INVALID_ARG, "Invalid tools JSON"));
        }
        if let Some(tools) = tools {
            if tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                body.insert("tools".to_string(), tools);
                body.insert("tool_choice".to_string(), json!({"type": "auto"}));
            }
        }

        serde_json::to_string(&Value::Object(body))
            .map_err(|_| LlmError::fail("Out of memory serializing request"))
    }

    fn headers(&self) -> [(&'static str, String); 2] {
        [
            ("x-api-key", self.api_key.clone()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ]
    }
}

impl LlmBackend for Anthropic {
    /// `anthropic_chat`
    fn chat(
        &self,
        http: &dyn ClawHttp,
        profile: &ModelProfile,
        request: &ChatRequest,
        abort: &AtomicBool,
    ) -> Result<LlmResponse, LlmError> {
        let post_data = self.build_chat_body(request)?;
        let url = join_url(&self.base_url, &profile.chat_path);
        let header_storage = self.headers();
        let headers: Vec<HttpHeader> = header_storage
            .iter()
            .map(|(n, v)| HttpHeader { name: n, value: v })
            .collect();

        let http_request = HttpJsonRequest {
            url: &url,
            body: &post_data,
            api_key: None,
            auth_type: Some("none"),
            timeout_ms: self.timeout_ms,
            headers: &headers,
        };
        let response = http
            .post_json(&http_request, abort)
            .map_err(|e| LlmError::new(e.err, e.message))?;
        parse_chat_response(&response.body)
    }

    /// `anthropic_infer_media`
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
        if prepared.kind != PreparedKind::DataUrl {
            return Err(LlmError::new(
                ESP_ERR_NOT_SUPPORTED,
                "Anthropic backend requires local image data",
            ));
        }
        let (mime, base64_data) = parse_data_url(&prepared.payload)
            .ok_or_else(|| LlmError::fail("Failed to prepare Anthropic image payload"))?;

        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": request.system_prompt.unwrap_or(""),
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": user_prompt},
                    {"type": "image", "source": {"type": "base64", "media_type": mime, "data": base64_data}}
                ]
            }]
        });
        let post_data = serde_json::to_string(&body)
            .map_err(|_| LlmError::fail("Out of memory serializing media request"))?;
        let url = join_url(&self.base_url, &profile.chat_path);
        let header_storage = self.headers();
        let headers: Vec<HttpHeader> = header_storage
            .iter()
            .map(|(n, v)| HttpHeader { name: n, value: v })
            .collect();

        let never = AtomicBool::new(false);
        let http_request = HttpJsonRequest {
            url: &url,
            body: &post_data,
            api_key: None,
            auth_type: Some("none"),
            timeout_ms: self.timeout_ms,
            headers: &headers,
        };
        let response = http
            .post_json(&http_request, &never)
            .map_err(|e| LlmError::new(e.err, e.message))?;

        let parsed = parse_chat_response(&response.body)?;
        match parsed.text {
            Some(t) if !t.is_empty() => Ok(t),
            _ => Err(LlmError::new(ESP_FAIL, "LLM returned empty media response")),
        }
    }
}
