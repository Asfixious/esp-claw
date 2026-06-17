//! In-crate tests for `BaseAgent` soft-hide tool gating.
//!
//! These live inside the crate (rather than `tests/`) because the gating hooks
//! they drive — `set_active_tools` / `set_tail_note` — are `pub(crate)`: the
//! mechanism is consumed by the in-crate semantic agents, not the public API.
//! A small self-contained harness (scripted HTTP + in-memory FS) keeps them
//! hermetic without the integration-test `tests/common` helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use claw_api::{ClawApi, ClawApiConfig};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use claw_interfaces::{ClawFs, FsError};
use claw_memory::{
    CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
    MemoryTaskPool, PoolConfig,
};
use serde_json::{json, Value};

use crate::agent::{AgentId, AgentRunError, AgentState, BaseAgent, BaseAgentBuilder, TickOutcome};
use crate::tools::{AllowedTools, Tool, ToolError, ToolHandler, ToolInvocation, ToolOutput, ToolSet};

// ---------------------------------------------------------------------------
// HTTP doubles (strict: panic if called more than scripted)
// ---------------------------------------------------------------------------

/// Serves scripted LLM bodies in order.
struct ScriptedHttp {
    steps: Mutex<VecDeque<String>>,
}

impl ScriptedHttp {
    fn new(bodies: Vec<String>) -> Self {
        Self {
            steps: Mutex::new(bodies.into_iter().collect()),
        }
    }
}

impl ClawHttp for ScriptedHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        let body = self
            .steps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .expect("ScriptedHttp: LLM called more times than scripted");
        Ok(HttpResponse {
            status_code: 200,
            body,
        })
    }
}

/// Like [`ScriptedHttp`] but records each request body so a test can inspect
/// what the agent actually sent to the model.
struct CapturingHttp {
    steps: Mutex<VecDeque<String>>,
    captured: Mutex<Vec<String>>,
}

impl CapturingHttp {
    fn new(bodies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(bodies.into_iter().collect()),
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
            .steps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .expect("CapturingHttp: LLM called more times than scripted");
        Ok(HttpResponse {
            status_code: 200,
            body,
        })
    }
}

// ---------------------------------------------------------------------------
// In-memory FS + stub compactor (hermetic conversation memory)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemFs {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemFs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.files.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ClawFs for MemFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        self.lock().get(path).cloned().ok_or(FsError::NotFound)
    }
    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let files = self.lock();
        let bytes = files.get(path).ok_or(FsError::NotFound)?;
        let start = usize::try_from(offset).map_err(|_| FsError::Io("offset overflow".into()))?;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| FsError::Io("read_at past end of file".into()))?;
        Ok(bytes[start..end].to_vec())
    }
    fn len(&self, path: &str) -> Result<u64, FsError> {
        self.lock()
            .get(path)
            .map(|b| u64::try_from(b.len()).unwrap_or(u64::MAX))
            .ok_or(FsError::NotFound)
    }
    fn write_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.lock().insert(path.to_string(), data.to_vec());
        Ok(())
    }
    fn append(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.lock()
            .entry(path.to_string())
            .or_default()
            .extend_from_slice(data);
        Ok(())
    }
    fn exists(&self, path: &str) -> bool {
        self.lock().contains_key(path)
    }
    fn remove(&self, path: &str) -> Result<(), FsError> {
        self.lock().remove(path);
        Ok(())
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, FsError> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut names = std::collections::BTreeSet::new();
        for key in self.lock().keys() {
            if let Some(rest) = key.strip_prefix(&prefix) {
                if let Some(name) = rest.split('/').next().filter(|n| !n.is_empty()) {
                    names.insert(name.to_string());
                }
            }
        }
        Ok(names.into_iter().collect())
    }
}

/// Never compacts.
struct StubCompactor;

impl Compactor for StubCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Test tools
// ---------------------------------------------------------------------------

/// Echoes its arguments back; used as the "allowed" tool.
struct EchoTool;

impl ToolHandler for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"echo","description":"Echo"}}"#
    }
    fn invoke(&self, call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: format!("echo:{}", call.arguments_json),
            ok: true,
        })
    }
}

/// Writes something; used as the "disallowed" tool.
struct WriterTool;

impl ToolHandler for WriterTool {
    fn name(&self) -> &'static str {
        "writer"
    }
    fn schema(&self) -> &'static str {
        r#"{"type":"function","function":{"name":"writer","description":"Write"}}"#
    }
    fn invoke(&self, _call: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: "wrote".into(),
            ok: true,
        })
    }
}

fn caller_tools() -> ToolSet {
    ToolSet::new([Tool::new(EchoTool), Tool::new(WriterTool)]).expect("tool set")
}

// ---------------------------------------------------------------------------
// Builders / drivers
// ---------------------------------------------------------------------------

fn build_llm(http: Arc<dyn ClawHttp>) -> ClawApi {
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
    .expect("init llm")
}

fn scripted_llm(bodies: Vec<String>) -> ClawApi {
    build_llm(Arc::new(ScriptedHttp::new(bodies)))
}

fn test_memory(agent_id: AgentId) -> ConversationMemory {
    let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("memory pool"));
    ConversationMemory::new(
        agent_id.0,
        ConversationConfig::new(format!("/mem/agent-{}", agent_id.0)),
        ConversationDeps {
            fs: Arc::new(MemFs::default()),
            pool,
            compactor: Arc::new(StubCompactor),
        },
    )
}

/// A builder plus a cloned read-only view of the same memory.
fn builder_with_view(llm: ClawApi, agent_id: AgentId) -> (BaseAgentBuilder, ConversationMemory) {
    let memory = test_memory(agent_id);
    let view = memory.clone();
    (BaseAgent::builder(llm, memory), view)
}

fn body_plain_text(text: &str) -> String {
    json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
}

fn body_tool_call(id: &str, name: &str, arguments_json: &str) -> String {
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

fn body_end_conversation(final_message: &str) -> String {
    body_tool_call(
        "e1",
        "end_conversation",
        &json!({ "final_message": final_message }).to_string(),
    )
}

fn run_to_completion(agent: &mut BaseAgent) -> String {
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

fn transcript_contents(view: &ConversationMemory) -> Vec<String> {
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

fn first_tool_message(messages: &Value) -> Option<Value> {
    messages
        .as_array()?
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .cloned()
}

/// True when every assistant `tool_calls[].id` has a matching `tool` message.
fn no_dangling_tool_calls(messages: &Value) -> bool {
    let Some(items) = messages.as_array() else {
        return false;
    };
    let mut expected: Vec<String> = Vec::new();
    let mut satisfied: Vec<String> = Vec::new();
    for message in items {
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    expected.push(id.to_string());
                }
            }
        }
        if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
            satisfied.push(id.to_string());
        }
    }
    expected.iter().all(|id| satisfied.contains(id))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A disallowed tool call is refused with a *matched* tool error (no dangling
/// call) and, within the retry budget, the agent keeps working and self-corrects.
#[test]
fn disallowed_tool_is_refused_with_matched_error() {
    let (builder, view) = builder_with_view(
        scripted_llm(vec![
            body_tool_call("t1", "writer", "{}"),
            body_end_conversation("done"),
        ]),
        AgentId(1),
    );
    let mut agent = builder.with_tools(caller_tools()).build().expect("build");
    agent
        .set_active_tools(AllowedTools::new(["end_conversation"]))
        .expect("gating while idle");

    agent.run("go");
    assert_eq!(run_to_completion(&mut agent), "done");

    let messages = view.messages();
    assert!(no_dangling_tool_calls(&messages), "blocked call left dangling");
    let tool_message = first_tool_message(&messages).expect("a tool message was committed");
    assert_eq!(tool_message["tool_call_id"], "t1");
    assert_eq!(tool_message["is_error"], true);
    let content = tool_message["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("not available in the current phase"),
        "unexpected blocked-tool content: {content}"
    );
}

/// With the default budget (1), a second *consecutive* blocked round fails the
/// task with `ToolNotPermitted` naming the refused tool.
#[test]
fn two_consecutive_blocks_fail_the_task() {
    let (builder, _view) = builder_with_view(
        scripted_llm(vec![
            body_tool_call("t1", "writer", "{}"),
            body_tool_call("t2", "writer", "{}"),
        ]),
        AgentId(1),
    );
    let mut agent = builder.with_tools(caller_tools()).build().expect("build");
    agent
        .set_active_tools(AllowedTools::new(["end_conversation"]))
        .expect("gating while idle");

    agent.run("go");
    // First block: nudged, still working.
    assert!(matches!(agent.tick(), TickOutcome::Working));
    // Second consecutive block: budget exhausted -> failed.
    match agent.tick() {
        TickOutcome::Failed(AgentRunError::ToolNotPermitted { name }) => assert_eq!(name, "writer"),
        other => panic!("expected ToolNotPermitted, got {other:?}"),
    }
    // Failed leaves the agent idle and reusable.
    assert!(!agent.is_running());
}

/// A clean tool round between two blocks resets the counter, so the budget of 1
/// is never exceeded and the task completes.
#[test]
fn clean_round_resets_block_counter() {
    let (builder, _view) = builder_with_view(
        scripted_llm(vec![
            body_tool_call("t1", "writer", "{}"), // block (count 1)
            body_tool_call("t2", "echo", "{}"),   // clean (reset to 0)
            body_tool_call("t3", "writer", "{}"), // block (count 1 again)
            body_end_conversation("done"),
        ]),
        AgentId(1),
    );
    let mut agent = builder.with_tools(caller_tools()).build().expect("build");
    // echo is permitted (the clean round); writer is not.
    agent
        .set_active_tools(AllowedTools::new(["echo", "end_conversation"]))
        .expect("gating while idle");

    agent.run("go");
    assert_eq!(run_to_completion(&mut agent), "done");
}

/// `0` retries fails on the very first disallowed call.
#[test]
fn zero_retries_fails_on_first_block() {
    let (builder, _view) =
        builder_with_view(scripted_llm(vec![body_tool_call("t1", "writer", "{}")]), AgentId(1));
    let mut agent = builder
        .with_tools(caller_tools())
        .with_tool_block_retries(0)
        .build()
        .expect("build");
    agent
        .set_active_tools(AllowedTools::new(["end_conversation"]))
        .expect("gating while idle");

    agent.run("go");
    match agent.tick() {
        TickOutcome::Failed(AgentRunError::ToolNotPermitted { name }) => assert_eq!(name, "writer"),
        other => panic!("expected ToolNotPermitted, got {other:?}"),
    }
}

/// With no allow-set, gating is off: a tool that gating *would* block runs
/// normally (the pre-gating behaviour is preserved).
#[test]
fn ungated_when_no_allow_set() {
    let (builder, view) = builder_with_view(
        scripted_llm(vec![
            body_tool_call("t1", "writer", "{}"),
            body_end_conversation("done"),
        ]),
        AgentId(1),
    );
    // Note: set_active_tools is intentionally NOT called.
    let mut agent = builder.with_tools(caller_tools()).build().expect("build");

    agent.run("go");
    assert_eq!(run_to_completion(&mut agent), "done");
    // The writer tool actually executed.
    assert!(transcript_contents(&view).iter().any(|c| c == "wrote"));
}

/// Clearing the gating restores the defaults: the previously blocked tool runs
/// again and no tail note is appended to the request.
#[test]
fn clearing_gating_restores_ungated_and_no_note() {
    let http = CapturingHttp::new(vec![
        body_tool_call("t1", "writer", "{}"),
        body_end_conversation("done"),
    ]);
    let llm = build_llm(Arc::clone(&http) as Arc<dyn ClawHttp>);
    let (builder, view) = builder_with_view(llm, AgentId(1));
    let mut agent = builder.with_tools(caller_tools()).build().expect("build");

    // Gate, then immediately ungate before running.
    agent
        .set_active_tools(AllowedTools::new(["end_conversation"]))
        .expect("gating while idle");
    agent.clear_active_tools().expect("clear gating while idle");
    agent.set_tail_note("PHASE: should not appear");
    agent.clear_tail_note();

    agent.run("go");
    assert_eq!(run_to_completion(&mut agent), "done");

    // The writer tool executed (gating was cleared).
    assert!(transcript_contents(&view).iter().any(|c| c == "wrote"));

    // No request carried the cleared note as a trailing message.
    for body in http.captured_bodies() {
        if let Some(messages) = body["messages"].as_array() {
            assert!(
                messages
                    .iter()
                    .all(|m| m.get("content").and_then(Value::as_str)
                        != Some("PHASE: should not appear")),
                "cleared tail note still reached the model"
            );
        }
    }
}

/// Tool gating is a pre-run policy: once a task is queued/running, changing the
/// allow-set is rejected with `ToolGatingError` and leaves gating untouched.
#[test]
fn gating_change_rejected_once_task_is_queued() {
    use crate::agent::ToolGatingError;

    let (builder, view) = builder_with_view(
        scripted_llm(vec![
            body_tool_call("t1", "writer", "{}"),
            body_end_conversation("done"),
        ]),
        AgentId(1),
    );
    let mut agent = builder.with_tools(caller_tools()).build().expect("build");

    // Gate to end_conversation only, then start the task. `run` projects the
    // state to Running, so any further gating change must be refused.
    agent
        .set_active_tools(AllowedTools::new(["end_conversation"]))
        .expect("gating while idle");
    agent.run("go");

    // Attempting to widen the allow-set now is rejected and is a no-op.
    let rejected = agent.set_active_tools(AllowedTools::new(["writer"]));
    assert!(matches!(
        rejected,
        Err(ToolGatingError {
            state: AgentState::Running
        })
    ));
    assert!(matches!(
        agent.clear_active_tools(),
        Err(ToolGatingError { .. })
    ));

    // The original gating still holds: writer stays blocked (the task self-
    // corrects to end_conversation).
    assert_eq!(run_to_completion(&mut agent), "done");
    assert!(
        !transcript_contents(&view).iter().any(|c| c == "wrote"),
        "writer ran despite being gated out"
    );
}

/// The transient tail note is appended to the request the model sees (last
/// message) but is never written to memory.
#[test]
fn tail_note_reaches_model_but_not_memory() {
    let note = "PHASE: intake. Available tools: echo.";
    let http = CapturingHttp::new(vec![body_plain_text("hi there")]);
    let llm = build_llm(Arc::clone(&http) as Arc<dyn ClawHttp>);
    let (builder, view) = builder_with_view(llm, AgentId(1));
    let mut agent = builder.build().expect("build");
    agent.set_tail_note(note);

    agent.run("hello");
    assert_eq!(run_to_completion(&mut agent), "hi there");

    // The request carried the note as the final (user) message.
    let body = http.captured_bodies().pop().expect("one captured request");
    let messages = body["messages"].as_array().expect("messages array");
    let last = messages.last().expect("at least one message");
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], note);

    // Memory holds the real turn but not the transient note.
    let committed = transcript_contents(&view);
    assert!(committed.iter().any(|c| c == "hello"));
    assert!(committed.iter().any(|c| c == "hi there"));
    assert!(
        !committed.iter().any(|c| c == note),
        "tail note leaked into memory"
    );
}
