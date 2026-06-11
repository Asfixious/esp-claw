//! LLM-driven auto-extraction (`claw_memory_extract.c`): the synchronous
//! extract/apply helpers plus the async worker. The C version uses a FreeRTOS
//! task + queue + per-job binary semaphore + singleton runtime; this port uses
//! `std::thread` + an `mpsc` channel + a `Condvar`-signalled job list and stores
//! the singleton `ClawApi` client inside the worker thread.
//!
//! The runtime / `EspIdfHttp` creation is gated to `target_os = "espidf"`; on
//! the host the worker is never started (async extract stays disabled), so the
//! crate still builds for `x86_64-unknown-linux-gnu`.

// The synchronous prepare/chat helpers and the worker plumbing are only
// instantiated by the espidf worker (`start_worker`); they are dead on host.
#![cfg_attr(not(target_os = "espidf"), allow(dead_code))]

use core::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use claw_api::{ChatError, ChatRequest, ClawApi, InitError};

use crate::api::{self, MemoryConfig};
use crate::consts::{MessageIntent, AUTO_EXTRACT_MAX_ITEMS, KEYWORDS_CAP, MAX_SUMMARIES, TAGS_CAP};
use crate::error::{MemoryError, MemoryResult};
use crate::item::{self, MemItem};
use crate::storage;
use crate::util;

/// `claw-api` is platform-agnostic; map its init error to the crate-internal
/// [`MemoryError`] (which carries the exact `esp_err_t` returned to C).
fn init_error_code(err: &InitError) -> MemoryError {
    match err {
        InitError::MissingApiKey
        | InitError::MissingModel
        | InitError::MissingBaseUrl
        | InitError::MissingBackendType => MemoryError::InvalidArg,
        InitError::UnknownBackend => MemoryError::NotSupported,
    }
}

/// Map a [`ChatError`] to the crate-internal [`MemoryError`].
fn chat_error_code(err: &ChatError) -> MemoryError {
    match err {
        ChatError::ToolsUnsupported => MemoryError::NotSupported,
        ChatError::InvalidToolsJson => MemoryError::InvalidArg,
        ChatError::Api(_) => MemoryError::Fail,
    }
}

/// Async extract worker stack. Larger than the C task's 6 KiB because the Rust
/// LLM/HTTP/TLS/serde path uses deeper frames; placed in PSRAM by `spawn_worker`.
const ASYNC_EXTRACT_STACK_SIZE: usize = 16 * 1024;
const ASYNC_EXTRACT_PRIORITY: u32 = 5;

const SYSTEM_PROMPT: &str = "You extract long-term memory candidates from the user's latest message for an ESP32 agent.\nReturn JSON only, with schema {\"intent\":\"none|forget|replace\",\"memories\":[{\"content\":\"...\",\"tags\":[\"...\"],\"keywords\":[\"...\"],\"source\":\"optional\"}]}.\nRules:\n- intent=forget when the user wants the assistant to stop remembering, delete, erase, or remove a memory. In that case return an empty memories array.\n- intent=replace when the user is correcting or revising a previously stated durable fact or preference. Output only the corrected durable fact.\n- intent=none for everything else, including requests to keep remembering something.\n- Extract only durable user-related long-term memory or reusable rules grounded in the current user message.\n- Never store instructions that primarily change the assistant's own persona, identity, role, tone, speech style, or persistent behavior. Those belong in profile memory such as soul.md or identity.md, not long-term memory. For those requests return an empty memories array.\n- Usually return 0 to 3 memories.\n- Normalize content into a concise memory fact, not the raw user quote.\n- Keep wording stable and minimal so the same fact maps to the same content across turns.\n- Preserve the user's language whenever possible; do not translate just to fit a preferred vocabulary.\n- Tags should be short retrieval labels stored by the system; prefer 1 to 2, and never add filler just to reach a limit.\n- Keywords should be exact retrieval keywords stored in keyword_index; prefer 1 to 3, and fewer is better when enough.\n- Keywords must be grounded in the normalized memory fact or its tags. Do not invent related synonyms or fine-grained expansions.\n- Do not output generic labels such as profile, preference, memory, information, fact, user data, or their equivalents in any language unless they are truly the best retrieval key.\n- Prefer concise, domain-specific concepts that appear directly in the user's message, such as a role, city, food, language, product, team, or reusable preference.\n- Good tags/keywords stay close to the user's wording; bad tags/keywords are vague category expansions, loose synonyms, or inferred related concepts.\n- If the user is correcting themselves, replacing one fact with another, or revising a prior statement about the user, extract only the corrected durable fact.\n- Avoid duplicates; if the same memory appears multiple ways in this turn, output it once.\n- If you cannot produce a precise tag, skip that memory instead of using a vague tag.\n- Never extract facts that are stated only by the assistant, only by previous session history, or only by existing memory labels.\n- If the user asks to forget, delete, remove, or stop remembering something, return {\"memories\":[]}.\n- If the user is asking the assistant to behave differently from now on, adopt a role, speak in a certain style every time, or maintain a persistent persona, return {\"intent\":\"none\",\"memories\":[]}.\n- If the current user message is a question, request, or recall prompt rather than a self-disclosure, return {\"memories\":[]}.\n- Ignore transient chit-chat, assistant-only information, and short-term tasks.\n- If there is no durable memory, return {\"memories\":[]}.\n";

// --- JSON document parsing ------------------------------------------------

/// `claw_memory_parse_llm_json_document`.
pub fn parse_llm_json_document(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if end >= start {
            if let Ok(v) = serde_json::from_str::<Value>(&text[start..=end]) {
                return Some(v);
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) {
        if end >= start {
            if let Ok(v) = serde_json::from_str::<Value>(&text[start..=end]) {
                return Some(v);
            }
        }
    }
    None
}

// --- LLM chat helper ------------------------------------------------------

/// `claw_memory_llm_chat_with_runtime`: a single user-message chat with no
/// tools. Returns the response text, or a `(MemoryError, message)` failure.
fn llm_chat_with_runtime(
    runtime: &ClawApi,
    system_prompt: &str,
    user_text: &str,
) -> Result<Option<String>, (MemoryError, String)> {
    let messages = serde_json::json!([{ "role": "user", "content": user_text }]);
    let request = ChatRequest {
        system_prompt,
        messages: &messages,
        tools_json: None,
    };
    let abort = AtomicBool::new(false);
    let response = runtime
        .chat(&request, &abort)
        .map_err(|e| (chat_error_code(&e), e.to_string()))?;
    if !response.tool_calls.is_empty() {
        return Err((MemoryError::NotSupported, "LLM returned unsupported tool calls".to_string()));
    }
    Ok(response.text)
}

/// `claw_memory_auto_extract_prepare_with_runtime`.
pub fn auto_extract_prepare_with_runtime(
    runtime: &ClawApi,
    user_text: &str,
) -> MemoryResult<(MessageIntent, Option<String>)> {
    if user_text.is_empty() {
        return Ok((MessageIntent::None, None));
    }

    let catalog = {
        let st = crate::state::lock();
        storage::summary_catalog_dup(&st)
    };
    let prompt = format!(
        "Existing summary labels:\n{}\nCurrent user message:\n{}\n",
        if catalog.is_empty() { "- (empty)\n".to_string() } else { catalog },
        user_text
    );

    let llm_text = match llm_chat_with_runtime(runtime, SYSTEM_PROMPT, &prompt) {
        Ok(t) => t,
        Err((_, msg)) => {
            log::warn!(
                "auto_extract llm failed: {}",
                if msg.is_empty() { "unknown_error" } else { &msg }
            );
            return Err(MemoryError::Fail);
        }
    };

    let mut intent = MessageIntent::None;
    match llm_text.as_deref().and_then(parse_llm_json_document) {
        Some(root) => {
            let intent_value = root.get("intent").and_then(|v| v.as_str());
            intent = MessageIntent::from_str(intent_value);
        }
        None => {
            log::warn!(
                "auto_extract llm returned invalid json for intent parse: {}",
                llm_text.as_deref().unwrap_or("")
            );
        }
    }
    if intent == MessageIntent::Forget {
        log::info!("auto_extract skipped explicit forget request");
    }
    Ok((intent, llm_text))
}

// --- candidate storage ----------------------------------------------------

fn copy_csv_field_from_json(json: &Value, key: &str, cap: usize) -> String {
    let value = match json.get(key) {
        Some(v) => v,
        None => return String::new(),
    };
    if let Some(s) = value.as_str() {
        return util::safe_copy(s, cap);
    }
    if let Some(arr) = value.as_array() {
        let mut out = String::new();
        for entry in arr.iter().take(MAX_SUMMARIES) {
            let entry = match entry.as_str() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let sep = if out.is_empty() { "" } else { "," };
            let candidate = format!("{}{}", sep, entry);
            if out.len() + candidate.len() >= cap {
                break;
            }
            out.push_str(&candidate);
        }
        return out;
    }
    String::new()
}

/// `claw_memory_store_extracted_candidate`.
fn store_extracted_candidate(
    candidate: &Value,
    intent: MessageIntent,
    out_summary: &mut Option<String>,
) -> MemoryResult<()> {
    if !candidate.is_object() {
        return Err(MemoryError::InvalidArg);
    }

    let mut item = MemItem::default();
    item.set_content(candidate.get("content").and_then(|v| v.as_str()).unwrap_or(""));
    item.set_source(candidate.get("source").and_then(|v| v.as_str()).unwrap_or(""));

    item.tags = copy_csv_field_from_json(candidate, "tags", TAGS_CAP);
    if item.tags.is_empty() {
        item.tags = copy_csv_field_from_json(candidate, "summary_labels", TAGS_CAP);
    }
    item.keywords = copy_csv_field_from_json(candidate, "keywords", KEYWORDS_CAP);
    if item.keywords.is_empty() {
        item.keywords = copy_csv_field_from_json(candidate, "tags", KEYWORDS_CAP);
    }
    if item.source.is_empty() {
        item.set_source("auto_llm");
    }
    item.content = util::trim_whitespace(&item.content).to_string();
    item.tags = util::trim_whitespace(&item.tags).to_string();
    item.keywords = util::trim_whitespace(&item.keywords).to_string();
    item::normalize_item_metadata(&mut item);

    if item.content.is_empty() {
        return Err(MemoryError::InvalidArg);
    }

    let changed = api::store_with_result(&mut item)?;
    if changed {
        api::replace_conflicting_items(&item, intent)?;
        item::append_item_summary_labels(&item, out_summary)?;
    } else {
        let key = item::build_item_key(&item);
        log::info!("auto_extract skipped duplicate key={}", key);
    }
    Ok(())
}

/// `claw_memory_auto_extract_apply_result`.
pub fn auto_extract_apply_result(
    llm_text: Option<&str>,
    intent: MessageIntent,
) -> MemoryResult<Option<String>> {
    let text = match llm_text {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(None),
    };
    if intent == MessageIntent::Forget {
        return Ok(None);
    }

    let root = match parse_llm_json_document(text) {
        Some(r) => r,
        None => {
            log::warn!("auto_extract llm returned invalid json: {}", text);
            return Err(MemoryError::Fail);
        }
    };

    let memories = if root.is_array() {
        Some(root.clone())
    } else {
        root.get("memories").cloned()
    };
    let memories = match memories {
        Some(Value::Array(a)) => a,
        _ => return Err(MemoryError::Fail),
    };

    let mut summary: Option<String> = None;
    let mut result: MemoryResult<()> = Ok(());
    let mut stored = 0;
    for (i, candidate) in memories.iter().take(AUTO_EXTRACT_MAX_ITEMS).enumerate() {
        match store_extracted_candidate(candidate, intent, &mut summary) {
            Ok(()) => stored += 1,
            Err(e) => {
                log::warn!(
                    "auto_extract candidate[{}] skipped err={}",
                    i,
                    e.code_name()
                );
                result = Err(e);
            }
        }
    }
    log::info!("auto_extract llm candidates={} stored={}", memories.len(), stored);
    result?;
    Ok(summary)
}

// --- async worker ---------------------------------------------------------

const SWEEP_AFTER: Duration = Duration::from_secs(60);

struct Job {
    request_id: u32,
    completed: bool,
    message_intent: MessageIntent,
    llm_text: Option<String>,
    completed_at: Option<Instant>,
}

struct WorkItem {
    request_id: u32,
    user_text: String,
    session_id: String,
}

struct AsyncState {
    enabled: bool,
    jobs: Vec<Job>,
    sender: Option<Sender<WorkItem>>,
}

impl AsyncState {
    fn new() -> AsyncState {
        AsyncState {
            enabled: false,
            jobs: Vec::new(),
            sender: None,
        }
    }
}

fn async_cell() -> &'static (Mutex<AsyncState>, Condvar) {
    static ASYNC: OnceLock<(Mutex<AsyncState>, Condvar)> = OnceLock::new();
    ASYNC.get_or_init(|| (Mutex::new(AsyncState::new()), Condvar::new()))
}

fn sweep_locked(state: &mut AsyncState) {
    let now = Instant::now();
    state.jobs.retain(|job| {
        !(job.completed
            && job
                .completed_at
                .is_some_and(|t| now.duration_since(t) >= SWEEP_AFTER))
    });
}

fn async_extract_deinit() {
    let (m, cv) = async_cell();
    let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
    state.jobs.clear();
    // Dropping the sender lets the worker's `recv()` return and the thread exit.
    state.sender = None;
    state.enabled = false;
    cv.notify_all();
}

fn worker_loop(runtime: std::sync::Arc<ClawApi>, rx: Receiver<WorkItem>) {
    while let Ok(work) = rx.recv() {
        let result = auto_extract_prepare_with_runtime(&runtime, &work.user_text);
        let (m, cv) = async_cell();
        let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = state.jobs.iter_mut().find(|j| j.request_id == work.request_id) {
            match result {
                Ok((intent, llm_text)) => {
                    job.message_intent = intent;
                    job.llm_text = llm_text;
                }
                Err(_) => {
                    job.message_intent = MessageIntent::None;
                    job.llm_text = None;
                }
            }
            job.completed = true;
            job.completed_at = Some(Instant::now());
        }
        let _ = work.session_id;
        cv.notify_all();
    }
}

#[cfg(target_os = "espidf")]
fn start_worker(config: &MemoryConfig) -> MemoryResult<()> {
    use std::sync::Arc;

    use claw_api::ClawApiConfig;

    let llm = &config.llm;
    let runtime_config = ClawApiConfig {
        api_key: llm.api_key.clone(),
        backend_type: llm.backend_type.clone().unwrap_or_default(),
        model: llm.model.clone(),
        base_url: llm.base_url.clone(),
        auth_type: llm.auth_type.clone(),
        max_tokens_field: llm.max_tokens_field.clone(),
        timeout_ms: llm.timeout_ms,
        max_tokens: llm.max_tokens,
        image_max_bytes: llm.image_max_bytes,
        supports_tools: llm.supports_tools,
        supports_vision: llm.supports_vision,
        image_remote_url_only: llm.image_remote_url_only,
    };

    let runtime = match ClawApi::init(runtime_config, Arc::new(claw_sys::EspIdfHttp)) {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            log::error!("Failed to init async memory extract runtime: {e}");
            async_extract_deinit();
            return Err(init_error_code(&e));
        }
    };

    let (tx, rx) = std::sync::mpsc::channel::<WorkItem>();
    // The worker drives an LLM runtime (HTTP + mbedTLS + serde_json), so it needs
    // a large PSRAM-backed stack like the C `claw_memory` async extract task
    // (which used CLAW_TASK_STACK_PREFER_PSRAM); a default pthread stack overflows.
    claw_sys::thread::spawn_worker(
        "claw_mem_extract",
        ASYNC_EXTRACT_STACK_SIZE,
        ASYNC_EXTRACT_PRIORITY,
        claw_sys::thread::NO_AFFINITY,
        move || worker_loop(runtime, rx),
    )
    .map_err(|_| ())
    .ok();

    let (m, _cv) = async_cell();
    let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
    state.sender = Some(tx);
    state.enabled = true;
    log::info!("Async memory extract worker ready");
    Ok(())
}

#[cfg(not(target_os = "espidf"))]
fn start_worker(_config: &MemoryConfig) -> MemoryResult<()> {
    // No EspIdfHttp transport off-target; async extract stays disabled so the
    // host build still compiles and links.
    log::debug!("Async memory extract unavailable on host build");
    Ok(())
}

/// `claw_memory_async_extract_init`.
pub fn async_extract_init(config: &MemoryConfig) -> MemoryResult<()> {
    async_extract_deinit();

    if !config.enable_async_extract_stage_note {
        return Ok(());
    }

    let llm = &config.llm;
    let complete = llm.api_key.as_deref().is_some_and(|s| !s.is_empty())
        && llm.model.as_deref().is_some_and(|s| !s.is_empty())
        && llm.backend_type.as_deref().is_some_and(|s| !s.is_empty());
    if !complete {
        log::info!("Async memory extract disabled: LLM config incomplete");
        return Ok(());
    }

    start_worker(config)
}

/// `claw_memory_async_extract_ensure_started`.
pub fn async_extract_ensure_started(
    request_id: u32,
    session_id: Option<&str>,
    user_text: Option<&str>,
) -> MemoryResult<()> {
    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    let user_text = match user_text {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    if request_id == 0 {
        return Ok(());
    }

    let (m, _cv) = async_cell();
    let sender;
    {
        let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
        if !state.enabled {
            return Ok(());
        }
        let tx = match &state.sender {
            Some(tx) => tx.clone(),
            None => return Err(MemoryError::InvalidState),
        };
        sweep_locked(&mut state);
        if state.jobs.iter().any(|j| j.request_id == request_id) {
            return Ok(());
        }
        state.jobs.push(Job {
            request_id,
            completed: false,
            message_intent: MessageIntent::None,
            llm_text: None,
            completed_at: None,
        });
        sender = tx;
    }

    let send_result = sender.send(WorkItem {
        request_id,
        user_text: user_text.to_string(),
        session_id: session_id.to_string(),
    });
    if send_result.is_err() {
        let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
        state.jobs.retain(|j| j.request_id != request_id);
        return Err(MemoryError::Timeout);
    }

    log::info!(
        "async extract job created request={} session={}",
        request_id,
        session_id
    );
    Ok(())
}

/// `claw_memory_async_extract_take_summary_list`.
pub fn async_extract_take_summary_list(request_id: u32, apply_result: bool) -> Option<String> {
    if request_id == 0 {
        return None;
    }
    let (m, cv) = async_cell();
    let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.enabled {
        return None;
    }

    loop {
        match guard.jobs.iter().position(|j| j.request_id == request_id) {
            None => {
                sweep_locked(&mut guard);
                return None;
            }
            Some(idx) => {
                if guard.jobs[idx].completed {
                    let job = guard.jobs.remove(idx);
                    drop(guard);
                    if !apply_result {
                        return None;
                    }
                    return auto_extract_apply_result(job.llm_text.as_deref(), job.message_intent)
                        .ok()
                        .flatten();
                }
                log::info!("stage note provider waiting request={}", request_id);
                guard = cv.wait(guard).unwrap_or_else(|e| e.into_inner());
                if !guard.enabled {
                    return None;
                }
            }
        }
    }
}
