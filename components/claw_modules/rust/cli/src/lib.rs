//! Shared scaffolding for the agent CLIs.
//!
//! Both binaries in this crate (`base-agent` and `conversation-agent-chat`) drive
//! a real agent against a live LLM with on-disk conversation memory. The platform
//! dependencies are identical, so the real-disk [`ClawFs`], live [`ClawHttp`], the
//! no-op [`Compactor`], and the env/LLM/memory wiring live here once.
//!
//! LLM config is read from `claw_core/.env.local` (the same file the integration
//! tests use): `CLAW_LLM_API_KEY`, `CLAW_LLM_BASE_URL`, `CLAW_LLM_MODEL`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use claw_api::{ClawApi, ClawApiConfig};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use claw_interfaces::DiskFs;
use claw_memory::{
    CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
    MemoryTaskPool, PoolConfig,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// ClawHttp: real network via reqwest blocking
// ---------------------------------------------------------------------------

/// A [`ClawHttp`] backed by a blocking `reqwest` client.
pub struct LiveHttp;

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
            .timeout(Duration::from_millis(u64::from(request.timeout_ms)))
            .build()
            .map_err(|_| HttpError::ClientInitFailed)?;

        let mut builder = client
            .post(request.url)
            .header("Content-Type", "application/json")
            .body(request.body.to_string());

        if let Some(key) = request.api_key.filter(|v| !v.is_empty()) {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        for header in request.headers {
            builder = builder.header(header.name, header.value);
        }

        let response = builder
            .send()
            .map_err(|err| HttpError::RequestFailed(err.to_string()))?;
        let status_code = i32::from(response.status().as_u16());
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
// Compactor: stub (no background summarisation)
// ---------------------------------------------------------------------------

/// A [`Compactor`] that never compacts.
pub struct StubCompactor;

impl Compactor for StubCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Config / wiring
// ---------------------------------------------------------------------------

/// Load `claw_core/.env.local` into the process environment if present.
///
/// # Panics
///
/// If the file exists but cannot be parsed — a misconfigured env file is a setup
/// error the operator should see, not silently ignore.
pub fn load_env() {
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../claw_core/.env.local");
    if env_path.is_file() {
        dotenvy::from_path(&env_path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e}", env_path.display()));
    }
}

/// Build a live LLM client from the `CLAW_LLM_*` environment variables.
///
/// `supports_tools` enables tool-calling for agents that need it (the
/// conversation agent); pass `false` for a plain chat agent.
///
/// # Panics
///
/// If any required `CLAW_LLM_*` variable is missing, or the client cannot init —
/// there is no safe default for these, so fail loudly.
pub fn make_llm(supports_tools: bool) -> ClawApi {
    let api_key = std::env::var("CLAW_LLM_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .expect("CLAW_LLM_API_KEY must be set");
    let base_url = std::env::var("CLAW_LLM_BASE_URL").expect("CLAW_LLM_BASE_URL must be set");
    let model = std::env::var("CLAW_LLM_MODEL").expect("CLAW_LLM_MODEL must be set");

    ClawApi::init(
        ClawApiConfig {
            api_key: Some(api_key),
            backend_type: "openai_compatible".into(),
            model: Some(model),
            base_url: Some(base_url),
            supports_tools,
            timeout_ms: 60_000,
            ..Default::default()
        },
        Arc::new(LiveHttp),
    )
    .expect("failed to init LLM client")
}

/// Build an on-disk conversation memory at `memory_dir` plus a cloned read-only
/// view of the same memory (handy for a `/messages` command).
///
/// # Panics
///
/// If the background memory task pool cannot be created.
pub fn make_memory(agent_id: usize, memory_dir: &str) -> (ConversationMemory, ConversationMemory) {
    let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("memory pool"));
    let memory = ConversationMemory::new(
        agent_id,
        ConversationConfig::new(memory_dir),
        ConversationDeps {
            fs: Arc::new(DiskFs::absolute()),
            pool,
            compactor: Arc::new(StubCompactor),
        },
    );
    let view = memory.clone();
    (memory, view)
}
