//! LLM runtime, backends, types, and media preparation (port of `src/llm/`).

pub mod backend;
pub mod backends;
pub mod media;
pub mod runtime;
pub mod types;

#[cfg(test)]
mod tests {
    use super::runtime::LlmRuntime;
    use super::types::{ChatRequest, RuntimeConfig};
    use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
    use core::sync::atomic::AtomicBool;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    struct MockHttp {
        reply: String,
        last_body: Mutex<Option<String>>,
        last_url: Mutex<Option<String>>,
    }

    impl MockHttp {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(MockHttp {
                reply: reply.to_string(),
                last_body: Mutex::new(None),
                last_url: Mutex::new(None),
            })
        }
    }

    impl ClawHttp for MockHttp {
        fn post_json(
            &self,
            request: &HttpJsonRequest,
            _abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            *self.last_body.lock().unwrap() = Some(request.body.to_string());
            *self.last_url.lock().unwrap() = Some(request.url.to_string());
            Ok(HttpResponse { status_code: 200, body: self.reply.clone() })
        }
    }

    fn cfg(backend: &str, base_url: &str) -> RuntimeConfig {
        RuntimeConfig {
            api_key: Some("key".into()),
            backend_type: backend.into(),
            model: Some("model-x".into()),
            base_url: Some(base_url.into()),
            supports_tools: true,
            supports_vision: true,
            ..Default::default()
        }
    }

    #[test]
    fn openai_chat_text() {
        let http = MockHttp::new(r#"{"choices":[{"message":{"role":"assistant","content":"hi there"}}]}"#);
        let rt = LlmRuntime::init(cfg("openai_compatible", "https://api.example.com/v1"), http.clone()).unwrap();
        let messages = json!([{"role": "user", "content": "hello"}]);
        let abort = AtomicBool::new(false);
        let resp = rt
            .chat(&ChatRequest { system_prompt: "sys", messages: &messages, tools_json: None }, &abort)
            .unwrap();
        assert_eq!(resp.text.as_deref(), Some("hi there"));

        // URL joined with one slash; body carries system + user messages.
        assert_eq!(http.last_url.lock().unwrap().as_deref(), Some("https://api.example.com/v1/chat/completions"));
        let body: Value = serde_json::from_str(http.last_body.lock().unwrap().as_deref().unwrap()).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "sys");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(body["model"], "model-x");
    }

    #[test]
    fn openai_tool_calls_parsed() {
        let reply = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[
            {"id":"call_1","function":{"name":"files","arguments":"{\"p\":\"/x\"}"}}]}}]}"#;
        let http = MockHttp::new(reply);
        let rt = LlmRuntime::init(cfg("openai_compatible", "https://api.example.com"), http).unwrap();
        let messages = json!([{"role": "user", "content": "list"}]);
        let abort = AtomicBool::new(false);
        let resp = rt
            .chat(&ChatRequest { system_prompt: "s", messages: &messages, tools_json: None }, &abort)
            .unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].name, "files");
        assert_eq!(resp.tool_calls[0].arguments_json, r#"{"p":"/x"}"#);
    }

    #[test]
    fn anthropic_converts_tool_role_to_user_and_parses() {
        let reply = r#"{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"done"},
            {"type":"tool_use","id":"tu1","name":"foo","input":{"a":1}}]}"#;
        let http = MockHttp::new(reply);
        let rt = LlmRuntime::init(cfg("anthropic_compatible", "https://api.anthropic.com/v1"), http.clone()).unwrap();

        // assistant with tool_calls, then a tool result message
        let messages = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "tu1", "function": {"name": "foo", "arguments": "{\"a\":1}"}}
            ]},
            {"role": "tool", "tool_call_id": "tu1", "content": "result-text"}
        ]);
        let abort = AtomicBool::new(false);
        let resp = rt
            .chat(&ChatRequest { system_prompt: "sys", messages: &messages, tools_json: None }, &abort)
            .unwrap();
        assert_eq!(resp.text.as_deref(), Some("done"));
        assert_eq!(resp.reasoning_content.as_deref(), Some("hmm"));
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "foo");

        // Verify the request conversion: tool role becomes a user message with a
        // tool_result block; assistant carries a tool_use block.
        let body: Value = serde_json::from_str(http.last_body.lock().unwrap().as_deref().unwrap()).unwrap();
        assert_eq!(body["system"], "sys");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        let assistant = &msgs[1];
        assert_eq!(assistant["role"], "assistant");
        let a_blocks = assistant["content"].as_array().unwrap();
        assert!(a_blocks.iter().any(|b| b["type"] == "tool_use" && b["name"] == "foo"));
        let tool_user = &msgs[2];
        assert_eq!(tool_user["role"], "user");
        let t_blocks = tool_user["content"].as_array().unwrap();
        assert_eq!(t_blocks[0]["type"], "tool_result");
        assert_eq!(t_blocks[0]["tool_use_id"], "tu1");
        assert_eq!(t_blocks[0]["content"], "result-text");
    }

    #[test]
    fn anthropic_converts_tools() {
        let http = MockHttp::new(r#"{"content":[{"type":"text","text":"ok"}]}"#);
        let rt = LlmRuntime::init(cfg("anthropic_compatible", "https://api.anthropic.com"), http.clone()).unwrap();
        let messages = json!([{"role": "user", "content": "hi"}]);
        let tools = r#"[{"type":"function","function":{"name":"foo","description":"d","parameters":{"type":"object"}}}]"#;
        let abort = AtomicBool::new(false);
        rt.chat(&ChatRequest { system_prompt: "s", messages: &messages, tools_json: Some(tools) }, &abort)
            .unwrap();
        let body: Value = serde_json::from_str(http.last_body.lock().unwrap().as_deref().unwrap()).unwrap();
        let tools_out = body["tools"].as_array().unwrap();
        assert_eq!(tools_out[0]["name"], "foo");
        assert_eq!(tools_out[0]["description"], "d");
        assert_eq!(tools_out[0]["input_schema"]["type"], "object");
        assert_eq!(body["tool_choice"]["type"], "auto");
    }

    #[test]
    fn unknown_backend_rejected() {
        let http = MockHttp::new("{}");
        let err = match LlmRuntime::init(cfg("nope", "https://x"), http) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert_eq!(err.err, claw_interfaces::error::ESP_ERR_NOT_SUPPORTED);
    }
}
