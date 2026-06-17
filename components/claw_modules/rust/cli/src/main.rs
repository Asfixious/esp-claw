//! Interactive chat CLI backed by [`BaseAgent`] with persistent memory.
//!
//! Reads LLM config from `claw_core/.env.local` (same file as the integration
//! tests). Conversation memory is written to `claw_core/output/chat/`.
//!
//! ```
//! cargo run -p claw-base-agent --target x86_64-unknown-linux-gnu
//! ```

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use claw_api::{ClawApi, ClawApiConfig};
use claw_core::agent::{ApprovalDecision, BaseAgent, TickOutcome};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
use claw_interfaces::{ClawFs, FsError};
use claw_memory::{
    CompactError, Compactor, ConversationConfig, ConversationDeps, ConversationMemory,
    MemoryTaskPool, PoolConfig,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// ClawFs: real disk
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
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path).map_err(|e| FsError::Io(e.to_string()))?;
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
            .map_err(|_| FsError::NotFound)
    }

    fn write_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let tmp = format!("{path}.tmp");
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
        }
        std::fs::write(&tmp, data).map_err(|e| FsError::Io(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| FsError::Io(e.to_string()))
    }

    fn append(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        use std::io::Write;
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
        }
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
        let entries = std::fs::read_dir(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FsError::NotFound
            } else {
                FsError::Io(e.to_string())
            }
        })?;
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

// ---------------------------------------------------------------------------
// ClawHttp: real network via reqwest blocking
// ---------------------------------------------------------------------------

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

struct StubCompactor;

impl Compactor for StubCompactor {
    fn compact(&self, _window: &[Value]) -> Result<Vec<Value>, CompactError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn load_env() {
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../claw_core/.env.local");
    if env_path.is_file() {
        dotenvy::from_path(&env_path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e}", env_path.display()));
    }
}

fn make_llm() -> ClawApi {
    let api_key = std::env::var("CLAW_LLM_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .expect("CLAW_LLM_API_KEY must be set");
    let base_url = std::env::var("CLAW_LLM_BASE_URL")
        .expect("CLAW_LLM_BASE_URL must be set");
    let model = std::env::var("CLAW_LLM_MODEL")
        .expect("CLAW_LLM_MODEL must be set");

    ClawApi::init(
        ClawApiConfig {
            api_key: Some(api_key),
            backend_type: "openai_compatible".into(),
            model: Some(model),
            base_url: Some(base_url),
            supports_tools: false,
            timeout_ms: 60_000,
            ..Default::default()
        },
        Arc::new(LiveHttp),
    )
    .expect("failed to init LLM client")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

const MEMORY_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../claw_core/output/chat");
const SYSTEM_PROMPT: &str = "You are a helpful, concise assistant.";
const AGENT_ID: usize = 1;

fn main() {
    load_env();

    let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("memory pool"));
    let memory = ConversationMemory::new(
        AGENT_ID,
        ConversationConfig::new(MEMORY_DIR),
        ConversationDeps {
            fs: Arc::new(DiskFs),
            pool,
            compactor: Arc::new(StubCompactor),
        },
    );
    let memory_view = memory.clone();
    let mut agent = BaseAgent::builder(make_llm(), memory)
        .with_system_prompt(SYSTEM_PROMPT)
        .build()
        .expect("failed to build agent");

    eprintln!("Memory: {MEMORY_DIR}");
    eprintln!("Type your message and press Enter. Empty line or Ctrl-D to quit.\n");

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().expect("flush stdout");

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let input = line.trim();
        if input.is_empty() {
            break;
        }

        if input == "/messages" {
            let messages = memory_view.messages();
            println!(
                "{}",
                serde_json::to_string_pretty(&messages)
                    .unwrap_or_else(|e| format!("(serialize error: {e})"))
            );
            println!();
            continue;
        }

        agent.run(input);

        loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                TickOutcome::Yielded { text } => {
                    println!("\n{text}\n");
                    break;
                }
                TickOutcome::Ended { final_message } => {
                    println!("\n{final_message}\n");
                    break;
                }
                TickOutcome::Failed(error) => {
                    eprintln!("agent error: {error}");
                    break;
                }
                TickOutcome::AwaitingApproval { id, summary } => {
                    eprintln!("approval requested [{id}]: {summary} — auto-approving");
                    if let Err(error) = agent.resolve_approval(id, ApprovalDecision::Approved) {
                        eprintln!("failed to resolve approval [{id}]: {error}");
                        break;
                    }
                    // Keep pumping so the queued decision resolves next tick.
                }
                TickOutcome::Cancelled { .. } | TickOutcome::Idle => break,
            }
        }
    }

    eprintln!("Goodbye.");
}
