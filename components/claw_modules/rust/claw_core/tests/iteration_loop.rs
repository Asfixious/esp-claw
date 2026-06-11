//! Public API tests for [`claw_core::iteration_loop`].

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use claw_api::{ClawApi, ClawApiConfig};
use claw_cap::{CapabilityContext, CapabilityError, CapabilityInvokeResult, CapabilityInvoker};
use claw_core::iteration_loop::{
    AppendedMessages, ChatMessages, IterationLoop, IterationLoopError, IterationLoopPhase,
    IterationResult, IterationStep, RequestControl, SystemPrompt, ToolSet,
};
use claw_core::request::RequestItem;
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use serde_json::{json, Value};

const PLAIN_TEXT_BODY: &str =
    r#"{"choices":[{"message":{"role":"assistant","content":"hello from model"}}]}"#;

const TOOL_CALL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"t1","function":{"name":"files","arguments":"{}"}}]}}]}"#;

const TOOL_CALL_EMPTY_NAME_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"t1","function":{"name":"","arguments":"{}"}}]}}]}"#;

fn load_local_env() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env.local");
    let _ = dotenvy::from_path(path);
}

fn local_env_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".env.local")
}

fn require_local_api_key() -> String {
    let env_path = local_env_path();
    if !env_path.is_file() {
        panic!(
            "live test requires {} — copy .env.example to .env.local and set CLAW_LLM_API_KEY",
            env_path.display()
        );
    }

    load_local_env();
    std::env::var("CLAW_LLM_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "live test requires CLAW_LLM_API_KEY in {} (file exists but key is missing or empty)",
                env_path.display()
            )
        })
}

fn local_base_url() -> String {
    load_local_env();
    std::env::var("CLAW_LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".into())
}

fn local_model() -> String {
    load_local_env();
    std::env::var("CLAW_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into())
}

/// Host-only HTTP client for optional live LLM tests (reads API key from `.env.local`).
struct LiveHttp;

impl ClawHttp for LiveHttp {
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
            .body(request.body.to_string());

        if let Some(api_key) = request.api_key.filter(|value| !value.is_empty()) {
            match request.auth_type.unwrap_or("bearer") {
                "api-key" => {
                    builder = builder.header("api-key", api_key);
                }
                "none" => {}
                _ => {
                    builder = builder.header("Authorization", format!("Bearer {api_key}"));
                }
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

        if status_code != 200 {
            return Err(HttpError::UnexpectedStatus(format!(
                "HTTP {status_code}: {body}"
            )));
        }

        Ok(HttpResponse { status_code, body })
    }
}

struct ScriptedHttp {
    bodies: Mutex<VecDeque<String>>,
}

impl ClawHttp for ScriptedHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| PLAIN_TEXT_BODY.into());
        Ok(HttpResponse {
            status_code: 200,
            body,
        })
    }
}

struct FailingHttp;

impl ClawHttp for FailingHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        Err(HttpError::RequestFailed("simulated transport failure".into()))
    }
}

fn test_llm(bodies: Vec<&str>) -> ClawApi {
    let http = Arc::new(ScriptedHttp {
        bodies: Mutex::new(bodies.into_iter().map(str::to_string).collect()),
    });
    ClawApi::init(
        ClawApiConfig {
            api_key: Some("sk-test".into()),
            backend_type: "openai_compatible".into(),
            model: Some("gpt-test".into()),
            base_url: Some("https://example.invalid".into()),
            supports_tools: true,
            ..Default::default()
        },
        http,
    )
    .expect("test llm init")
}

fn test_request() -> RequestItem {
    RequestItem {
        request_id: 42,
        user_text: "hi".into(),
        session_id: Some("sess-1".into()),
        source_channel: Some("cli".into()),
        source_chat_id: Some("chat-1".into()),
        ..Default::default()
    }
}

struct MockControl {
    phase: Mutex<IterationLoopPhase>,
    abort_flag: Arc<AtomicBool>,
    interrupts: Mutex<VecDeque<String>>,
    interrupt_on_drain_call: Mutex<Option<usize>>,
    drain_calls: Mutex<usize>,
    interrupt_http_abort: AtomicBool,
}

impl MockControl {
    fn new() -> Self {
        MockControl {
            phase: Mutex::new(IterationLoopPhase::Idle),
            abort_flag: Arc::new(AtomicBool::new(false)),
            interrupts: Mutex::new(VecDeque::new()),
            interrupt_on_drain_call: Mutex::new(None),
            drain_calls: Mutex::new(0),
            interrupt_http_abort: AtomicBool::new(false),
        }
    }

    fn queue_interrupt(&self, text: impl Into<String>) {
        self.interrupts.lock().unwrap().push_back(text.into());
    }

    fn deliver_interrupt_on_drain_call(&self, call_number: usize) {
        *self.interrupt_on_drain_call.lock().unwrap() = Some(call_number);
    }

    fn set_interrupt_http_abort(&self, enabled: bool) {
        self.interrupt_http_abort.store(enabled, Ordering::Release);
    }

    fn phase(&self) -> IterationLoopPhase {
        *self.phase.lock().unwrap()
    }
}

impl RequestControl for MockControl {
    fn set_phase(&self, phase: IterationLoopPhase) {
        *self.phase.lock().unwrap() = phase;
    }

    fn abort_flag(&self) -> &Arc<AtomicBool> {
        &self.abort_flag
    }

    fn take_user_interrupt_http_abort(&self, _request_id: u32) -> bool {
        self.interrupt_http_abort.load(Ordering::Acquire)
    }

    fn dequeue_inserted_user_inputs(&self, _session_id: &str, max: usize) -> Vec<String> {
        let mut queue = self.interrupts.lock().unwrap();
        let mut out = Vec::new();
        while out.len() < max {
            match queue.pop_front() {
                Some(text) => out.push(text),
                None => break,
            }
        }

        if out.is_empty() {
            if let Some(call_number) = *self.interrupt_on_drain_call.lock().unwrap() {
                let mut calls = self.drain_calls.lock().unwrap();
                *calls += 1;
                if *calls == call_number {
                    out.push("late interrupt".into());
                }
            }
        }

        out
    }

    fn clear_user_interrupt_abort(&self, _request_id: u32) {}
}

struct EchoCapability;

impl CapabilityInvoker for EchoCapability {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        _context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        Ok(CapabilityInvokeResult {
            output: format!("{capability_name}:{input_json}"),
            ok: true,
        })
    }
}

struct FailingCapability;

impl CapabilityInvoker for FailingCapability {
    fn invoke(
        &self,
        _capability_name: &str,
        _input_json: &str,
        _context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        Err(CapabilityError::NotFound)
    }
}

struct SoftFailCapability;

impl CapabilityInvoker for SoftFailCapability {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        _context: &CapabilityContext,
    ) -> Result<CapabilityInvokeResult, CapabilityError> {
        Ok(CapabilityInvokeResult {
            output: format!("soft-fail:{capability_name}:{input_json}"),
            ok: false,
        })
    }
}

fn test_llm_with_http(http: Arc<dyn ClawHttp>) -> ClawApi {
    ClawApi::init(
        ClawApiConfig {
            api_key: Some("sk-test".into()),
            backend_type: "openai_compatible".into(),
            model: Some("gpt-test".into()),
            base_url: Some("https://example.invalid".into()),
            supports_tools: true,
            ..Default::default()
        },
        http,
    )
    .expect("test llm init")
}

fn run_step<'a>(
    llm: &'a ClawApi,
    control: &'a MockControl,
    capability_invoker: Option<&'a dyn CapabilityInvoker>,
    request: &'a RequestItem,
    messages: &'a Value,
    system_prompt: &'a str,
) -> Result<IterationResult, IterationLoopError> {
    let step = IterationStep {
        request,
        system_prompt: SystemPrompt(system_prompt),
        messages: ChatMessages(messages),
        tools: ToolSet::none(),
    };
    let iteration = IterationLoop {
        llm,
        capability_invoker,
        control,
    };
    iteration.run(step)
}

#[test]
fn appended_messages_empty_and_extend() {
    let mut tail = AppendedMessages::empty();
    assert_eq!(tail.0, json!([]));

    let mut extra = AppendedMessages(json!([{"role":"user","content":"more"}]));
    tail.extend(&extra);
    assert_eq!(tail.0.as_array().unwrap().len(), 1);

    extra.extend(&AppendedMessages::empty());
    assert_eq!(extra.0, json!([{"role":"user","content":"more"}]));

    let mut non_array_dest = AppendedMessages(json!({"not": "array"}));
    non_array_dest.extend(&AppendedMessages(json!([{"role":"user","content":"x"}])));
    assert_eq!(non_array_dest.0, json!({"not": "array"}));

    let mut empty = AppendedMessages::empty();
    empty.extend(&AppendedMessages(json!("not-an-array")));
    assert!(empty.0.as_array().unwrap().is_empty());
}

#[test]
fn tool_set_exposes_tools_json() {
    assert_eq!(ToolSet::none().as_json(), None);
    let tools = r#"[{"type":"function"}]"#;
    assert_eq!(ToolSet::new(Some(tools)).as_json(), Some(tools));
}

#[test]
fn system_prompt_borrows_text() {
    let prompt = SystemPrompt("You are helpful.");
    assert_eq!(prompt.as_ref(), "You are helpful.");
}

#[test]
fn run_returns_plain_text_outcome() {
    let llm = test_llm(vec![PLAIN_TEXT_BODY]);
    let control = MockControl::new();
    let request = test_request();
    let messages = json!([{"role":"user","content":"hi"}]);

    let result = run_step(&llm, &control, None, &request, &messages, "system")
        .expect("plain text iteration");

    match result {
        IterationResult::PlainText(outcome) => {
            assert_eq!(outcome.text, "hello from model");
            assert!(outcome.raw_message_json.is_some());
        }
        other => panic!("expected PlainText, got {other:?}"),
    }
    assert_eq!(control.phase(), IterationLoopPhase::Finalizing);
}

#[test]
fn run_executes_tools_and_records_runs() {
    let llm = test_llm(vec![TOOL_CALL_BODY]);
    let control = MockControl::new();
    let echo = EchoCapability;
    let request = test_request();
    let messages = json!([]);

    let result = run_step(
        &llm,
        &control,
        Some(&echo),
        &request,
        &messages,
        "system",
    )
    .expect("tool iteration");

    match result {
        IterationResult::Tools(outcome) => {
            assert_eq!(outcome.runs.len(), 1);
            assert_eq!(outcome.runs[0].name, "files");
            assert!(outcome.runs[0].ok);

            let appended = outcome.appended.0.as_array().unwrap();
            assert_eq!(appended.len(), 2);
            assert_eq!(appended[0]["role"], "assistant");
            assert_eq!(appended[1]["role"], "tool");
            assert_eq!(appended[1]["tool_call_id"], "t1");
            assert_eq!(appended[1]["content"], "files:{}");
            assert_eq!(appended[1]["is_error"], false);
        }
        other => panic!("expected Tools, got {other:?}"),
    }
    assert_eq!(control.phase(), IterationLoopPhase::RunningTool);
}

#[test]
fn run_returns_interrupted_when_user_input_queued() {
    let llm = test_llm(vec![PLAIN_TEXT_BODY]);
    let control = MockControl::new();
    control.queue_interrupt("wait, also this");
    let request = test_request();
    let messages = json!([]);

    let result = run_step(&llm, &control, None, &request, &messages, "system")
        .expect("interrupted iteration");

    match result {
        IterationResult::Interrupted(outcome) => {
            let appended = outcome.appended.0.as_array().unwrap();
            assert_eq!(appended.len(), 1);
            assert_eq!(appended[0]["role"], "user");
            assert_eq!(appended[0]["content"], "wait, also this");
        }
        other => panic!("expected Interrupted, got {other:?}"),
    }
    assert_eq!(control.phase(), IterationLoopPhase::BeforeLlmHttp);
}

#[test]
fn run_errors_when_tool_calls_without_capability_invoker() {
    let llm = test_llm(vec![TOOL_CALL_BODY]);
    let control = MockControl::new();
    let request = test_request();
    let messages = json!([]);

    let error = run_step(&llm, &control, None, &request, &messages, "system")
        .expect_err("expected missing invoker");

    assert!(matches!(
        error,
        IterationLoopError::MissingCapabilityInvoker
    ));
}

#[test]
fn run_returns_interrupted_after_tool_call_before_execution() {
    let llm = test_llm(vec![TOOL_CALL_BODY]);
    let control = MockControl::new();
    control.deliver_interrupt_on_drain_call(2);
    let request = test_request();
    let messages = json!([]);

    let result = run_step(&llm, &control, None, &request, &messages, "system")
        .expect("interrupted after tool call");

    match result {
        IterationResult::Interrupted(outcome) => {
            let appended = outcome.appended.0.as_array().unwrap();
            assert_eq!(appended.len(), 1);
            assert_eq!(appended[0]["content"], "late interrupt");
        }
        other => panic!("expected Interrupted, got {other:?}"),
    }
    assert_eq!(control.phase(), IterationLoopPhase::AfterLlmBeforeTool);
}

#[test]
fn run_returns_interrupted_when_http_aborted_with_queued_input() {
    let llm = test_llm_with_http(Arc::new(FailingHttp));
    let control = MockControl::new();
    control.set_interrupt_http_abort(true);
    control.queue_interrupt("stop during http");
    let request = test_request();
    let messages = json!([]);

    let result = run_step(&llm, &control, None, &request, &messages, "system")
        .expect("interrupted during http");

    match result {
        IterationResult::Interrupted(outcome) => {
            let appended = outcome.appended.0.as_array().unwrap();
            assert_eq!(appended.len(), 1);
            assert_eq!(appended[0]["content"], "stop during http");
        }
        other => panic!("expected Interrupted, got {other:?}"),
    }
}

#[test]
fn run_propagates_chat_errors_without_interrupt() {
    let llm = test_llm_with_http(Arc::new(FailingHttp));
    let control = MockControl::new();
    let request = test_request();
    let messages = json!([]);

    let error = run_step(&llm, &control, None, &request, &messages, "system")
        .expect_err("expected chat transport error");

    assert!(matches!(error, IterationLoopError::Chat(_)));
}

#[test]
fn run_records_soft_failing_tool_with_null_name() {
    let llm = test_llm(vec![TOOL_CALL_EMPTY_NAME_BODY]);
    let control = MockControl::new();
    let soft_fail = SoftFailCapability;
    let request = test_request();
    let messages = json!([]);

    let result = run_step(
        &llm,
        &control,
        Some(&soft_fail),
        &request,
        &messages,
        "system",
    )
    .expect("soft-fail tool iteration");

    match result {
        IterationResult::Tools(outcome) => {
            assert_eq!(outcome.runs.len(), 1);
            assert_eq!(outcome.runs[0].name, "(null)");
            assert!(!outcome.runs[0].ok);

            let appended = outcome.appended.0.as_array().unwrap();
            assert_eq!(appended.len(), 2);
            assert_eq!(appended[1]["is_error"], true);
        }
        other => panic!("expected Tools, got {other:?}"),
    }
}

#[test]
fn run_propagates_capability_errors() {
    let llm = test_llm(vec![TOOL_CALL_BODY]);
    let control = MockControl::new();
    let failing = FailingCapability;
    let request = test_request();
    let messages = json!([]);

    let error = run_step(
        &llm,
        &control,
        Some(&failing),
        &request,
        &messages,
        "system",
    )
    .expect_err("expected capability error");

    assert!(matches!(
        error,
        IterationLoopError::Capability(CapabilityError::NotFound)
    ));
}

#[test]
fn live_plain_text_when_api_key_configured() {
    let api_key = require_local_api_key();

    let http = Arc::new(LiveHttp);
    let llm = ClawApi::init(
        ClawApiConfig {
            api_key: Some(api_key),
            backend_type: "openai_compatible".into(),
            model: Some(local_model()),
            base_url: Some(local_base_url()),
            supports_tools: true,
            timeout_ms: 60_000,
            ..Default::default()
        },
        http,
    )
    .expect("live llm init");

    let control = MockControl::new();
    let request = test_request();
    let messages = json!([{"role":"user","content":"Reply with exactly: pong"}]);

    let result = run_step(
        &llm,
        &control,
        None,
        &request,
        &messages,
        "You are a test assistant. Be brief.",
    )
    .expect("live iteration");

    match result {
        IterationResult::PlainText(outcome) => {
            assert!(
                outcome.text.to_lowercase().contains("pong"),
                "unexpected model text: {}",
                outcome.text
            );
        }
        other => panic!("expected PlainText from live model, got {other:?}"),
    }
}
