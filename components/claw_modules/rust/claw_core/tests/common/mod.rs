//! Shared test harness for the `BaseAgent` integration tests.
//!
//! Every helper here favours *strictness* so tests fail loudly on the unexpected:
//! the HTTP doubles panic when the agent calls the LLM more often than scripted,
//! which turns a stray (or missing) LLM round into a hard failure instead of a
//! silent false pass.

#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use claw_api::{ClawApi, ClawApiConfig};
use claw_core::agent::{AgentId, BaseAgent, BaseAgentBuilder, TickOutcome};
use claw_core::{
    FsSkillRegistry, SkillId, SkillRegistry, SkillSet, ToolError, ToolHandler, ToolInvocation,
    ToolOutput,
};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use claw_interfaces::{ClawFs, FsError};
use claw_memory::{
    CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
    MemoryTaskPool, PoolConfig,
};
use serde_json::{json, Value};

// ===========================================================================
// Canned LLM response bodies (OpenAI-compatible `choices` shape)
// ===========================================================================

/// An assistant turn that returns plain text and hands control back.
pub fn body_plain_text(text: &str) -> String {
    json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
}

/// An assistant turn that issues a single tool call. `arguments_json` is the raw
/// JSON *string* the model passes as the call arguments.
pub fn body_tool_call(id: &str, name: &str, arguments_json: &str) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": id,
                    "function": { "name": name, "arguments": arguments_json }
                }]
            }
        }]
    })
    .to_string()
}

/// The built-in `end_conversation` tool call (terminates the task).
pub fn body_end_conversation(final_message: &str) -> String {
    body_tool_call(
        "e1",
        "end_conversation",
        &json!({ "final_message": final_message }).to_string(),
    )
}

/// The built-in `request_approval` tool call (pauses for a human decision).
pub fn body_request_approval(summary: &str) -> String {
    body_tool_call(
        "a1",
        "request_approval",
        &json!({ "summary": summary }).to_string(),
    )
}

/// A call to the `echo` test tool (see [`EchoTool`]).
pub fn body_echo_call(input: &str) -> String {
    body_tool_call("t1", "echo", &json!({ "input": input }).to_string())
}

// ===========================================================================
// HTTP doubles — all strict (panic on an unscripted call)
// ===========================================================================

/// One scripted LLM round: a 200 body, or a transport error.
type ScriptStep = Result<String, String>;

/// Serves scripted responses in order; panics if the agent calls more often than
/// scripted, so an unexpected LLM round is caught.
pub struct ScriptedHttp {
    steps: Mutex<VecDeque<ScriptStep>>,
}

impl ScriptedHttp {
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
        }
    }

    fn next_step(&self) -> ScriptStep {
        self.steps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .expect("ScriptedHttp: agent called the LLM more times than scripted")
    }
}

impl ClawHttp for ScriptedHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        match self.next_step() {
            Ok(body) => Ok(HttpResponse {
                status_code: 200,
                body,
            }),
            Err(message) => Err(HttpError::RequestFailed(message)),
        }
    }
}

/// Like [`ScriptedHttp`] but also records every request body so tests can assert
/// on the context the agent sent to the model.
pub struct CapturingHttp {
    steps: Mutex<VecDeque<ScriptStep>>,
    captured: Mutex<Vec<String>>,
}

impl CapturingHttp {
    pub fn new(bodies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(bodies.into_iter().map(Ok).collect()),
            captured: Mutex::new(Vec::new()),
        })
    }

    /// Every request body the agent sent, parsed as JSON, in call order.
    pub fn captured_bodies(&self) -> Vec<Value> {
        self.captured
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|s| serde_json::from_str(s).unwrap_or(Value::Null))
            .collect()
    }

    /// Number of LLM calls the agent has made so far.
    pub fn call_count(&self) -> usize {
        self.captured.lock().unwrap_or_else(|p| p.into_inner()).len()
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
        let step = self
            .steps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .expect("CapturingHttp: agent called the LLM more times than scripted");
        match step {
            Ok(body) => Ok(HttpResponse {
                status_code: 200,
                body,
            }),
            Err(message) => Err(HttpError::RequestFailed(message)),
        }
    }
}

/// Always fails the LLM round.
pub struct FailingHttp;

impl ClawHttp for FailingHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        Err(HttpError::RequestFailed("simulated failure".into()))
    }
}

/// Panics if called — for tests that must never reach the LLM (e.g. build errors).
pub struct NeverHttp;

impl ClawHttp for NeverHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        panic!("NeverHttp: the LLM must not be called in this test");
    }
}

// ===========================================================================
// LLM builders
// ===========================================================================

fn build_llm(http: Arc<dyn ClawHttp>, supports_tools: bool) -> ClawApi {
    ClawApi::init(
        ClawApiConfig {
            api_key: Some("sk-test".into()),
            backend_type: "openai_compatible".into(),
            model: Some("gpt-test".into()),
            base_url: Some("https://example.invalid".into()),
            supports_tools,
            ..Default::default()
        },
        http,
    )
    .expect("init llm")
}

/// Tool-capable LLM serving the given plain bodies in order (strict).
pub fn scripted_llm(bodies: Vec<String>) -> ClawApi {
    build_llm(Arc::new(ScriptedHttp::new(bodies.into_iter().map(Ok).collect())), true)
}

/// Tool-capable LLM whose rounds may be successes or transport errors (strict).
pub fn scripted_llm_steps(steps: Vec<ScriptStep>) -> ClawApi {
    build_llm(Arc::new(ScriptedHttp::new(steps)), true)
}

/// Tool-capable LLM that records requests; returns the API plus the capture handle.
pub fn capturing_llm(bodies: Vec<String>) -> (ClawApi, Arc<CapturingHttp>) {
    let http = CapturingHttp::new(bodies);
    let llm = build_llm(Arc::clone(&http) as Arc<dyn ClawHttp>, true);
    (llm, http)
}

/// Tool-capable LLM whose every round fails.
pub fn failing_llm() -> ClawApi {
    build_llm(Arc::new(FailingHttp), true)
}

/// LLM that does NOT support tools, serving the given plain bodies (strict).
pub fn scripted_llm_no_tools(bodies: Vec<String>) -> ClawApi {
    build_llm(Arc::new(ScriptedHttp::new(bodies.into_iter().map(Ok).collect())), false)
}

/// Tool-capable LLM that must never be called (panics if it is).
pub fn never_called_llm() -> ClawApi {
    build_llm(Arc::new(NeverHttp), true)
}

// ===========================================================================
// Filesystem doubles
// ===========================================================================

/// `ClawFs` over the real disk using absolute paths (for conversation memory).
pub struct AbsFs;

impl ClawFs for AbsFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(path).map_err(map_io)
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let mut file = std::fs::File::open(path).map_err(map_io)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FsError::Io(e.to_string()))?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)
            .map_err(|e| FsError::Io(e.to_string()))?;
        Ok(buf)
    }

    fn len(&self, path: &str) -> Result<u64, FsError> {
        std::fs::metadata(path).map(|m| m.len()).map_err(map_io)
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

    fn list_dir(&self, path: &str) -> Result<Vec<String>, FsError> {
        let entries = std::fs::read_dir(path).map_err(map_io)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| FsError::Io(e.to_string()))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }
}

/// `ClawFs` rooted at a base directory, so virtual paths stay portable (skills).
pub struct RootedFs {
    base: PathBuf,
}

impl RootedFs {
    fn full(&self, path: &str) -> PathBuf {
        self.base.join(path)
    }
}

impl ClawFs for RootedFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(self.full(path)).map_err(map_io)
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let mut file = std::fs::File::open(self.full(path)).map_err(map_io)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FsError::Io(e.to_string()))?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)
            .map_err(|e| FsError::Io(e.to_string()))?;
        Ok(buf)
    }

    fn len(&self, path: &str) -> Result<u64, FsError> {
        std::fs::metadata(self.full(path))
            .map(|m| m.len())
            .map_err(map_io)
    }

    fn write_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        std::fs::write(self.full(path), data).map_err(|e| FsError::Io(e.to_string()))
    }

    fn append(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> {
        Err(FsError::Io("append unsupported in skills fs".into()))
    }

    fn exists(&self, path: &str) -> bool {
        self.full(path).exists()
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        match std::fs::remove_file(self.full(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(FsError::Io(e.to_string())),
        }
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, FsError> {
        let entries = std::fs::read_dir(self.full(path)).map_err(map_io)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| FsError::Io(e.to_string()))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }
}

fn map_io(error: std::io::Error) -> FsError {
    if error.kind() == std::io::ErrorKind::NotFound {
        FsError::NotFound
    } else {
        FsError::Io(error.to_string())
    }
}

// ===========================================================================
// Compactor / tools
// ===========================================================================

/// Never compacts.
pub struct StubCompactor;

impl Compactor for StubCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(Vec::new())
    }
}

/// A trivial caller tool named `echo` that echoes its arguments back.
pub struct EchoTool;

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

// ===========================================================================
// Memory / agent builders
// ===========================================================================

/// A fresh background memory pool.
pub fn test_pool() -> Arc<MemoryTaskPool> {
    Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("memory pool"))
}

/// `<crate>/output/<name>/`, wiped clean and recreated. Use a UNIQUE `name` per
/// test (collisions across tests corrupt each other's transcripts).
pub fn test_output_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("output")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create output dir");
    dir
}

/// Real disk-backed conversation memory.
pub fn test_memory(
    agent_id: AgentId,
    dir: impl Into<String>,
    pool: Arc<MemoryTaskPool>,
) -> ConversationMemory {
    ConversationMemory::new(
        agent_id.0,
        ConversationConfig::new(dir),
        ConversationDeps {
            fs: Arc::new(AbsFs),
            pool,
            compactor: Arc::new(StubCompactor),
        },
    )
}

/// A `BaseAgentBuilder` over fresh disk memory.
pub fn agent_builder(llm: ClawApi, agent_id: AgentId, dir: impl Into<String>) -> BaseAgentBuilder {
    BaseAgent::builder(llm, test_memory(agent_id, dir, test_pool()))
}

/// A builder plus a cloned read-only view of the same memory, so a test can
/// inspect the committed transcript without going through the agent.
pub fn builder_with_view(
    llm: ClawApi,
    agent_id: AgentId,
    dir: impl Into<String>,
) -> (BaseAgentBuilder, ConversationMemory) {
    let memory = test_memory(agent_id, dir, test_pool());
    let view = memory.clone();
    (BaseAgent::builder(llm, memory), view)
}

// ===========================================================================
// Drivers / assertions
// ===========================================================================

/// Pump until the task hands back an answer (`Yielded`) or ends (`Ended`),
/// returning that text. Panics on `Failed` or any other non-progress outcome so
/// an unexpected pause/approval/cancel surfaces instead of hanging.
pub fn run_to_completion(agent: &mut BaseAgent) -> String {
    loop {
        match agent.tick() {
            TickOutcome::Working => continue,
            TickOutcome::Yielded { text } => return text,
            TickOutcome::Ended { final_message } => return final_message,
            TickOutcome::Failed(error) => panic!("unexpected agent failure: {error}"),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}

/// The `content` strings of every message in the committed transcript, in order.
pub fn transcript_contents(view: &ConversationMemory) -> Vec<String> {
    view.messages()
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|m| m.get("content").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ===========================================================================
// Skills
// ===========================================================================

fn skills_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// A skill registry scanned over the `tests/data/skills` fixtures.
pub fn skill_registry() -> Arc<dyn SkillRegistry> {
    Arc::new(
        FsSkillRegistry::scan(
            Arc::new(RootedFs {
                base: skills_data_dir(),
            }),
            "skills",
        )
        .expect("scan skills fixtures"),
    )
}

/// An empty skill set over the fixture registry.
pub fn skill_set() -> SkillSet {
    SkillSet::new(skill_registry())
}

/// The id of the first fixture skill (chosen dynamically, not hard-coded).
pub fn first_skill_id() -> SkillId {
    skill_registry()
        .catalog()
        .first()
        .expect("at least one fixture skill")
        .id()
        .clone()
}
