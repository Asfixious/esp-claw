//! Integration tests for [`claw_core::agent::BaseAgent`].
//!
//! Scripted tests drive the state machine with canned HTTP responses.
//! Memory tests use a real `DiskFs` writing to `output/<test_name>/` under
//! the crate root so that transcript files can be inspected after a run.
//! The live test talks to a real LLM and verifies multi-turn memory continuity.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use claw_api::{ClawApi, ClawApiConfig};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use claw_interfaces::{ClawFs, FsError};
use claw_memory::{CompactError, Compactor, MemoryTaskPool, PoolConfig};
use claw_core::agent::{AgentError, AgentId, BaseAgent, BaseAgentConfig, BaseAgentState, RunParams};
use claw_core::{Tool, ToolError, ToolGroup, ToolHandler, ToolInvocation, ToolOutput, ToolSet};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Helpers: env / live API
// ---------------------------------------------------------------------------

fn load_local_env() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env.local");
    if path.is_file() {
        dotenvy::from_path(&path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    }
}

fn local_env_path() -> PathBuf {
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
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "live test requires CLAW_LLM_API_KEY in {} (file exists but key is missing or empty)",
                env_path.display()
            )
        })
}

fn local_base_url() -> String {
    load_local_env();
    std::env::var("CLAW_LLM_BASE_URL")
        .expect("CLAW_LLM_BASE_URL must be set in .env.local")
}

fn local_model() -> String {
    load_local_env();
    std::env::var("CLAW_LLM_MODEL")
        .expect("CLAW_LLM_MODEL must be set in .env.local")
}

// ---------------------------------------------------------------------------
// Helpers: output directory
// ---------------------------------------------------------------------------

/// Returns `<crate>/output/<name>/`, wiped clean and recreated on every call.
fn test_output_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("output")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create output dir");
    dir
}

// ---------------------------------------------------------------------------
// Helpers: real DiskFs
// ---------------------------------------------------------------------------

struct DiskFs;

impl ClawFs for DiskFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FsError::NotFound
            } else {
                FsError::Io(e.to_string())
            }
        })
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let mut file = std::fs::File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FsError::NotFound
            } else {
                FsError::Io(e.to_string())
            }
        })?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FsError::Io(e.to_string()))?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)
            .map_err(|e| FsError::Io(e.to_string()))?;
        Ok(buf)
    }

    fn len(&self, path: &str) -> Result<u64, FsError> {
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    FsError::NotFound
                } else {
                    FsError::Io(e.to_string())
                }
            })
    }

    fn write_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, data).map_err(|e| FsError::Io(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| FsError::Io(e.to_string()))
    }

    fn append(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        file.write_all(data).map_err(|e| FsError::Io(e.to_string()))
    }

    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(FsError::Io(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers: HTTP stubs
// ---------------------------------------------------------------------------

const PLAIN_TEXT_BODY: &str =
    r#"{"choices":[{"message":{"role":"assistant","content":"pong"}}]}"#;

const TOOL_CALL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"t1","function":{"name":"echo","arguments":"{\"input\":\"hello\"}"}}]}}]}"#;

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
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .expect("ScriptedHttp: no more canned responses — test called the LLM more times than expected");
        Ok(HttpResponse { status_code: 200, body })
    }
}

/// Scripted HTTP that also records every request body so tests can inspect
/// what context the agent actually sent to the LLM.
struct CapturingHttp {
    bodies: Mutex<VecDeque<String>>,
    captured: Mutex<Vec<String>>,
}

impl CapturingHttp {
    fn new(bodies: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(bodies.into_iter().map(str::to_string).collect()),
            captured: Mutex::new(Vec::new()),
        })
    }

    fn captured_bodies(&self) -> Vec<Value> {
        self.captured
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|s| serde_json::from_str(s).unwrap_or(Value::Null))
            .collect()
    }
}

impl ClawHttp for CapturingHttp {
    fn post_json(
        &self,
        request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        self.captured
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request.body.to_string());
        let body = self
            .bodies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or_else(|| PLAIN_TEXT_BODY.into());
        Ok(HttpResponse { status_code: 200, body })
    }
}

struct FailingHttp;

impl ClawHttp for FailingHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        Err(HttpError::RequestFailed("simulated failure".into()))
    }
}

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

        if let Some(api_key) = request.api_key.filter(|v| !v.is_empty()) {
            builder = builder.header("Authorization", format!("Bearer {api_key}"));
        }
        for header in request.headers {
            builder = builder.header(header.name, header.value);
        }

        let response = builder
            .send()
            .map_err(|err| HttpError::RequestFailed(err.to_string()))?;
        let status_code = response.status().as_u16() as i32;
        let body = response
            .text()
            .map_err(|err| HttpError::RequestFailed(err.to_string()))?;

        if status_code != 200 {
            return Err(HttpError::UnexpectedStatus(format!(
                "HTTP {status_code}: {body}"
            )));
        }
        Ok(HttpResponse { status_code, body })
    }
}

// ---------------------------------------------------------------------------
// Helpers: memory stubs
// ---------------------------------------------------------------------------

/// Skips compaction entirely.
struct StubCompactor;

impl Compactor for StubCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Helpers: tools
// ---------------------------------------------------------------------------

struct EchoTool;

impl ToolHandler for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"echo","description":"Echo the arguments back"}}"#
    }
    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: format!("echo:{}", call.arguments_json),
            ok: true,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers: agent / LLM builders
// ---------------------------------------------------------------------------

fn test_pool() -> Arc<MemoryTaskPool> {
    Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("pool"))
}

fn scripted_llm(bodies: Vec<&str>) -> ClawApi {
    let http = Arc::new(ScriptedHttp {
        bodies: Mutex::new(bodies.into_iter().map(str::to_string).collect()),
    });
    make_llm(http)
}

fn make_llm(http: Arc<dyn ClawHttp>) -> ClawApi {
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
    .expect("llm")
}

fn live_llm() -> ClawApi {
    let api_key = require_local_api_key();
    ClawApi::init(
        ClawApiConfig {
            api_key: Some(api_key),
            backend_type: "openai_compatible".into(),
            model: Some(local_model()),
            base_url: Some(local_base_url()),
            supports_tools: false,
            timeout_ms: 60_000,
            ..Default::default()
        },
        Arc::new(LiveHttp),
    )
    .expect("live llm")
}

fn agent_config(agent_id: AgentId, dir: impl Into<String>) -> BaseAgentConfig {
    BaseAgentConfig {
        agent_id,
        memory_dir: dir.into(),
        fs: Arc::new(DiskFs),
        compactor: Arc::new(StubCompactor),
    }
}

fn run_to_completion(agent: &mut BaseAgent) -> String {
    loop {
        match agent.tick() {
            BaseAgentState::Completed(text) => return text,
            BaseAgentState::Working => continue,
            other => panic!("unexpected state: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: AgentId
// ---------------------------------------------------------------------------

#[test]
fn agent_id_wire_format() {
    let id = AgentId(7);
    assert_eq!(id.to_string(), "agent-7");

    let parsed: AgentId = "agent-7".parse().expect("parse");
    assert_eq!(parsed, id);
}

#[test]
fn agent_id_serializes_to_prefixed_string() {
    let value = serde_json::to_value(AgentId(3)).expect("serialize");
    assert_eq!(value, json!("agent-3"));
}

// ---------------------------------------------------------------------------
// Tests: BaseAgent state machine
// ---------------------------------------------------------------------------

#[test]
fn tick_returns_idle_before_run() {
    let dir = test_output_dir("tick_returns_idle_before_run");
    let mut agent = BaseAgent::new(scripted_llm(vec![]), test_pool(), agent_config(AgentId(1), dir.display().to_string()));

    assert!(!agent.is_running());
    assert!(matches!(agent.tick(), BaseAgentState::Idle));
}

#[test]
fn run_returns_busy_while_task_in_progress() {
    let dir = test_output_dir("run_returns_busy_while_task_in_progress");
    let mut agent = BaseAgent::new(scripted_llm(vec![]), test_pool(), agent_config(AgentId(1), dir.display().to_string()));

    agent.run(RunParams {
        goal: "first",
        system_prompt: String::new(),
    }).expect("first run");

    assert!(agent.is_running());

    let error = agent
        .run(RunParams {
            goal: "second",
            system_prompt: String::new(),
        })
        .unwrap_err();
    assert!(matches!(error, AgentError::Busy));
}

#[test]
fn single_turn_returns_completed() {
    let dir = test_output_dir("single_turn_returns_completed");
    let mut agent = BaseAgent::new(
        scripted_llm(vec![PLAIN_TEXT_BODY]),
        test_pool(),
        agent_config(AgentId(1), dir.display().to_string()),
    );

    agent.run(RunParams {
        goal: "say pong",
        system_prompt: "You are a test assistant.".into(),
    }).expect("run");

    assert!(agent.is_running());
    let text = run_to_completion(&mut agent);
    assert!(!agent.is_running());
    assert_eq!(text, "pong");
}

#[test]
fn tool_round_returns_working_then_completed() {
    let dir = test_output_dir("tool_round_returns_working_then_completed");
    // First LLM call returns a tool call; second returns plain text.
    let llm = scripted_llm(vec![TOOL_CALL_BODY, PLAIN_TEXT_BODY]);
    let tools = ToolSet::from_groups([ToolGroup::new("echo_group", [Tool::new(EchoTool)])])
        .expect("build tool set");
    let mut agent = BaseAgent::new(llm, test_pool(), agent_config(AgentId(1), dir.display().to_string()))
        .with_tools(tools);

    agent.run(RunParams {
        goal: "use the echo tool then answer",
        system_prompt: String::new(),
    }).expect("run");

    // First tick: LLM asks for tool → tools execute → Working (tool round done,
    // no user-facing answer yet).
    assert!(matches!(agent.tick(), BaseAgentState::Working));
    // Agent is still running after a tool round.
    assert!(agent.is_running());

    // Second tick: LLM returns plain text → Completed.
    assert!(matches!(agent.tick(), BaseAgentState::Completed(_)));
    assert!(!agent.is_running());
}

#[test]
fn failed_http_transitions_to_failed_state() {
    let dir = test_output_dir("failed_http_transitions_to_failed_state");
    let llm = make_llm(Arc::new(FailingHttp));
    let mut agent = BaseAgent::new(llm, test_pool(), agent_config(AgentId(1), dir.display().to_string()));

    agent.run(RunParams {
        goal: "hello",
        system_prompt: String::new(),
    }).expect("run");

    assert!(matches!(agent.tick(), BaseAgentState::Failed(_)));
    assert!(!agent.is_running());
}

#[test]
fn interrupt_before_tick_returns_interrupted() {
    let dir = test_output_dir("interrupt_before_tick_returns_interrupted");
    let mut agent = BaseAgent::new(
        scripted_llm(vec![PLAIN_TEXT_BODY]),
        test_pool(),
        agent_config(AgentId(1), dir.display().to_string()),
    );

    agent.run(RunParams {
        goal: "hello",
        system_prompt: String::new(),
    }).expect("run");

    agent.interrupt_handle().interrupt();
    // Interrupted — preempted before LLM HTTP, so the agent stays running
    // and will attempt the next iteration on the following tick.
    assert!(matches!(agent.tick(), BaseAgentState::Interrupted));
    assert!(agent.is_running());

    // The interrupt flag was consumed; the next tick proceeds to the LLM.
    assert!(matches!(agent.tick(), BaseAgentState::Completed(_)));
    assert!(!agent.is_running());
}

#[test]
fn tick_is_idle_after_terminal_state() {
    let dir = test_output_dir("tick_is_idle_after_terminal_state");
    let mut agent = BaseAgent::new(
        scripted_llm(vec![PLAIN_TEXT_BODY]),
        test_pool(),
        agent_config(AgentId(1), dir.display().to_string()),
    );

    agent.run(RunParams { goal: "hi", system_prompt: String::new() })
        .expect("run");
    assert!(matches!(agent.tick(), BaseAgentState::Completed(_)));

    assert!(matches!(agent.tick(), BaseAgentState::Idle));
    assert!(matches!(agent.tick(), BaseAgentState::Idle));
}

// ---------------------------------------------------------------------------
// Tests: memory persistence (DiskFs)
// ---------------------------------------------------------------------------

#[test]
fn memory_written_to_disk_after_turn() {
    let dir = test_output_dir("memory_written_to_disk_after_turn");
    let data_file = dir.join("conversation-1.jsonl");

    let mut agent = BaseAgent::new(
        scripted_llm(vec![PLAIN_TEXT_BODY]),
        test_pool(),
        agent_config(AgentId(1), dir.display().to_string()),
    );

    agent.run(RunParams {
        goal: "write something to disk",
        system_prompt: String::new(),
    }).expect("run");
    run_to_completion(&mut agent);

    // The conversation data file must exist and contain JSONL records.
    let content = std::fs::read_to_string(&data_file)
        .unwrap_or_else(|_| panic!("conversation data file not found: {}", data_file.display()));
    assert!(!content.is_empty(), "data file is empty");
    // Each non-empty line must parse as a JSON object.
    for line in content.lines().filter(|l| !l.is_empty()) {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|err| panic!("data file has invalid JSON line: {err}\nLine: {line}"));
    }
}

#[test]
fn second_turn_includes_first_turn_context() {
    let dir = test_output_dir("second_turn_includes_first_turn_context");

    // Turn 1: complete a turn so the user + assistant messages are committed.
    {
        let http = CapturingHttp::new(vec![PLAIN_TEXT_BODY]);
        let mut agent = BaseAgent::new(
            make_llm(Arc::clone(&http) as Arc<dyn ClawHttp>),
            test_pool(),
            agent_config(AgentId(1), dir.display().to_string()),
        );
        agent.run(RunParams {
            goal: "my secret word is BANANA",
            system_prompt: String::new(),
        }).expect("run turn 1");
        run_to_completion(&mut agent);
    }

    // Turn 2: new agent instance, same agent_id and dir → loads persisted transcript.
    let http2 = CapturingHttp::new(vec![PLAIN_TEXT_BODY]);
    {
        let mut agent = BaseAgent::new(
            make_llm(Arc::clone(&http2) as Arc<dyn ClawHttp>),
            test_pool(),
            agent_config(AgentId(1), dir.display().to_string()),
        );
        agent.run(RunParams {
            goal: "what was my secret word?",
            system_prompt: String::new(),
        }).expect("run turn 2");
        run_to_completion(&mut agent);
    }

    // The second LLM call must have received the first turn's messages in context.
    let bodies = http2.captured_bodies();
    assert!(!bodies.is_empty(), "no LLM calls captured for turn 2");
    let second_call_body = bodies[0].to_string();
    assert!(
        second_call_body.contains("BANANA"),
        "turn 1 context (BANANA) missing from turn 2 LLM request:\n{second_call_body}"
    );
}

// ---------------------------------------------------------------------------
// Live test: multi-turn chat with real LLM
// ---------------------------------------------------------------------------

#[test]
fn live_two_turn_chat_uses_memory() {
    let dir = test_output_dir("live_two_turn_chat_uses_memory");
    let pool = test_pool();

    // Turn 1 — plant a memorable fact.
    {
        let mut agent = BaseAgent::new(
            live_llm(),
            Arc::clone(&pool),
            agent_config(AgentId(1), dir.display().to_string()),
        );
        agent.run(RunParams {
            goal: "Remember this code word: FLAMINGO. Reply with exactly: acknowledged",
            system_prompt: "You are a test assistant. Be brief and exact.".into(),
        }).expect("run turn 1");

        let text = run_to_completion(&mut agent);
        assert!(
            text.to_lowercase().contains("acknowledged"),
            "turn 1 unexpected: {text}"
        );
    }

    // Turn 2 — new agent instance loads the persisted transcript and recalls the fact.
    {
        let mut agent = BaseAgent::new(
            live_llm(),
            pool,
            agent_config(AgentId(1), dir.display().to_string()),
        );
        agent.run(RunParams {
            goal: "What was the code word I gave you?",
            system_prompt: "You are a test assistant. Be brief and exact.".into(),
        }).expect("run turn 2");

        let text = run_to_completion(&mut agent);
        assert!(
            text.to_uppercase().contains("FLAMINGO"),
            "turn 2 did not recall FLAMINGO: {text}"
        );
    }
}
