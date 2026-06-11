//! Helpers shared by the LLM backends.

use serde_json::Value;

use super::super::errors::ClawApiError;
use super::super::types::{LlmResponse, ToolCall};

/// `join_url` from the backends: join `base_url` and `path` with exactly one
/// slash between them.
pub fn join_url(base_url: &str, path: &str) -> String {
    let base_has_slash = base_url.ends_with('/');
    let path_has_slash = path.starts_with('/');
    if base_has_slash && path_has_slash {
        format!("{base_url}{}", &path[1..])
    } else if !base_has_slash && !path_has_slash {
        format!("{base_url}/{path}")
    } else {
        format!("{base_url}{path}")
    }
}

/// Parse an OpenAI chat-completions response, mirroring `parse_chat_response`
/// in `claw_llm_backend_openai_compatible.c`.
pub fn parse_openai_chat_response(body: &str) -> Result<LlmResponse, ClawApiError> {
    let root: Value = serde_json::from_str(body).map_err(|_| ClawApiError::Parse)?;

    let message = root
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c0| c0.get("message"));
    let message = match message {
        Some(m) if m.is_object() => m,
        _ => return Err(ClawApiError::MalformedResponse("response missing message")),
    };

    if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return Err(ClawApiError::MalformedResponse("response message is not assistant"));
    }

    let raw_message_json = serde_json::to_string(message)
        .map_err(|_| ClawApiError::ApiError("out of memory copying raw message"))?;

    let text = message
        .get("content")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // reasoning_content: kept even when empty, as long as it is a string.
    let reasoning_content = message
        .get("reasoning_content")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());

    let mut tool_calls = Vec::new();
    if let Some(arr) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in arr {
            let function = tc.get("function");
            let id = tc.get("id");
            let name = function.and_then(|f| f.get("name"));
            let args = function.and_then(|f| f.get("arguments"));
            if id.is_none() || function.is_none() || name.is_none() || args.is_none() {
                return Err(ClawApiError::MalformedResponse("malformed tool call"));
            }
            match (id.unwrap().as_str(), name.unwrap().as_str(), args.unwrap().as_str()) {
                (Some(id), Some(name), Some(args)) => tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments_json: args.to_string(),
                }),
                _ => return Err(ClawApiError::MalformedResponse("malformed tool call")),
            }
        }
    }

    if text.is_none() && tool_calls.is_empty() {
        return Err(ClawApiError::EmptyResponse);
    }

    Ok(LlmResponse { text, reasoning_content, raw_message_json: Some(raw_message_json), tool_calls })
}
