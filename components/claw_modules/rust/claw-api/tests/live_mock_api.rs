//! Integration tests against a publicly hosted **mock** LLM endpoint.
//!
//! Unlike the in-process `MockHttp` unit tests in `lib.rs` (which assert the
//! request/response wire format with no network), these exercise the *real*
//! transport + parse round-trip over HTTPS using a free, keyless mock service:
//! [MockAI](https://mock-ai.fly.dev). MockAI needs no API key and echoes the
//! last user message back as the assistant reply, so we can assert the full
//! path end-to-end without a real provider or secret.
//!
//! They are `#[ignore]`d because they hit a third-party service over the
//! network (which can rate-limit, change, or go down). Run them explicitly:
//!
//! ```text
//! cargo test -p claw-api --test live_mock_api --target x86_64-unknown-linux-gnu -- --ignored
//! ```
//!
//! ## Provider coverage
//!
//! DeepSeek, Qwen, MiniMax, Kimi (Moonshot), GLM (Zhipu), and OpenAI itself are
//! all OpenAI-**compatible**: they share the exact wire format of the
//! `openai_compatible` backend and differ only by `base_url`/`model`. So the
//! per-provider cases below run that one backend with each provider's config
//! (the real base URLs are recorded for reference), pointed at the mock.
//! Anthropic/Claude uses the separate `anthropic_compatible` backend
//! (`/messages`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use claw_api::{ChatJsonRequest, ChatRequest, ClawApi, ClawApiConfig};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use serde_json::json;

/// Free, keyless mock LLM service. Serves OpenAI shape at `/chat/completions`
/// and Anthropic shape at `/messages`, echoing the user content back.
const MOCK_BASE_URL: &str = "https://mock-ai.fly.dev";

/// A real `reqwest`-backed [`ClawHttp`] for the live tests.
///
/// MockAI routes by SDK User-Agent: an agent containing `OpenAI` maps to the
/// OpenAI API, one containing `Anthropic` (and not `OpenAI`) maps to the
/// Anthropic API, and unknown agents are rejected. So each test uses the
/// transport whose `user_agent` matches the backend under test.
struct ReqwestHttp {
    user_agent: String,
}

impl ReqwestHttp {
    /// Transport that MockAI routes to its OpenAI-compatible endpoint.
    fn openai() -> Arc<Self> {
        Arc::new(ReqwestHttp {
            user_agent: "claw-api-itest OpenAI/1.0".into(),
        })
    }

    /// Transport that MockAI routes to its Anthropic endpoint.
    fn anthropic() -> Arc<Self> {
        Arc::new(ReqwestHttp {
            user_agent: "claw-api-itest Anthropic/1.0".into(),
        })
    }
}

impl ClawHttp for ReqwestHttp {
    fn post_json(
        &self,
        request: &HttpJsonRequest,
        abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        if abort.load(Ordering::Acquire) {
            return Err(HttpError::Aborted);
        }

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(request.timeout_ms as u64))
            .build()
            .map_err(|_| HttpError::ClientInitFailed)?;

        let mut builder = client
            .post(request.url)
            .header("Content-Type", "application/json")
            .header("User-Agent", &self.user_agent)
            .body(request.body.to_string());

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
        let status_code = response.status().as_u16() as i32;
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

/// An OpenAI-compatible provider: only `model`/`base_url` differ between them.
struct Provider {
    name: &'static str,
    model: &'static str,
    /// The provider's real base URL (for reference; tests hit the mock instead).
    real_base_url: &'static str,
}

const OPENAI_COMPATIBLE_PROVIDERS: &[Provider] = &[
    Provider {
        name: "OpenAI",
        model: "gpt-4o-mini",
        real_base_url: "https://api.openai.com/v1",
    },
    Provider {
        name: "DeepSeek",
        model: "deepseek-chat",
        real_base_url: "https://api.deepseek.com",
    },
    Provider {
        name: "Qwen",
        model: "qwen-plus",
        real_base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
    },
    Provider {
        name: "MiniMax",
        model: "MiniMax-Text-01",
        real_base_url: "https://api.minimaxi.com/v1",
    },
    Provider {
        name: "Kimi",
        model: "moonshot-v1-8k",
        real_base_url: "https://api.moonshot.cn/v1",
    },
    Provider {
        name: "GLM",
        model: "glm-4",
        real_base_url: "https://open.bigmodel.cn/api/paas/v4",
    },
];

fn openai_compatible_api(model: &str) -> ClawApi {
    ClawApi::init(
        ClawApiConfig {
            api_key: Some("mock-key".into()), // any non-empty string; mock ignores it
            backend_type: "openai_compatible".into(),
            model: Some(model.into()),
            base_url: Some(MOCK_BASE_URL.into()),
            supports_tools: true,
            supports_vision: true,
            ..Default::default()
        },
        ReqwestHttp::openai(),
    )
    .expect("init openai_compatible")
}

#[test]
#[ignore = "hits hosted mock LLM endpoint over the network; run with --ignored"]
fn openai_compatible_providers_roundtrip() {
    for provider in OPENAI_COMPATIBLE_PROVIDERS {
        // The real base URL is recorded only for documentation.
        assert!(
            provider.real_base_url.starts_with("https://"),
            "{} should document a real https base url",
            provider.name
        );

        let api = openai_compatible_api(provider.model);
        // MockAI echoes the last user message content, so a unique marker lets
        // us assert the full request->transport->response->parse round-trip.
        let marker = format!("roundtrip-{}", provider.name);
        let messages = json!([{ "role": "user", "content": marker }]);
        let abort = AtomicBool::new(false);

        let resp = api
            .chat(&ChatRequest::new("be an echo", &messages), &abort)
            .unwrap_or_else(|e| panic!("{} chat failed: {e}", provider.name));

        assert_eq!(
            resp.text.as_deref(),
            Some(marker.as_str()),
            "{} should echo the user content back",
            provider.name
        );
    }
}

#[test]
#[ignore = "hits hosted mock LLM endpoint over the network; run with --ignored"]
fn anthropic_roundtrip() {
    let api = ClawApi::init(
        ClawApiConfig {
            api_key: Some("mock-key".into()),
            backend_type: "anthropic_compatible".into(),
            model: Some("claude-3-5-sonnet".into()),
            base_url: Some(MOCK_BASE_URL.into()),
            supports_tools: true,
            supports_vision: true,
            ..Default::default()
        },
        ReqwestHttp::anthropic(),
    )
    .expect("init anthropic_compatible");

    let messages = json!([{ "role": "user", "content": "roundtrip-Anthropic" }]);
    let abort = AtomicBool::new(false);

    let resp = api
        .chat(&ChatRequest::new("be an echo", &messages), &abort)
        .expect("anthropic chat failed");

    assert_eq!(resp.text.as_deref(), Some("roundtrip-Anthropic"));
}

#[test]
#[ignore = "hits hosted mock LLM endpoint over the network; run with --ignored"]
fn chat_json_roundtrip() {
    #[derive(serde::Deserialize)]
    struct Sentiment {
        label: String,
        score: i32,
    }

    let api = openai_compatible_api("gpt-4o-mini");

    // MockAI echoes the user content verbatim, so sending a JSON object string
    // lets us exercise the chat_json parse path end-to-end.
    let payload = r#"{"label":"positive","score":1}"#;
    let messages = json!([{ "role": "user", "content": payload }]);
    let schema = r#"{
        "type": "object",
        "properties": { "label": { "type": "string" }, "score": { "type": "integer" } },
        "required": ["label", "score"]
    }"#;
    let abort = AtomicBool::new(false);

    let resp = api
        .chat_json::<Sentiment>(
            &ChatJsonRequest::new("classify", &messages).with_output_schema("sentiment", schema),
            &abort,
        )
        .expect("chat_json failed");

    let output = resp.output.expect("expected parsed JSON output");
    assert_eq!(output.label, "positive");
    assert_eq!(output.score, 1);
}
