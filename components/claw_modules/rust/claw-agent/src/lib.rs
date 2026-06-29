//! `claw_agent` — the concise external interface to the claw agent system.
//!
//! Everything under `rust/` (the orchestrator, the agent factory, channels,
//! memory, and the LLM client) is wired together here behind one small surface:
//!
//! - [`AgentSystem`] is a ready-to-drive agent runtime. Build it with the
//!   host-friendly [`AgentSystem::on_disk`] (real disk memory + live HTTP) or the
//!   fully injectable [`AgentSystem::builder`] (for tests / custom backends).
//! - [`Chat`] is a single conversation: [`Chat::send`] hands the model a message
//!   and returns its reply text(s). A [`Chat`] owns its own session, so several
//!   chats on one [`AgentSystem`] stay isolated.
//!
//! Internally a message goes: [`Chat::send`] -> [`AgentSystem`] ingress ->
//! `claw_core::Orchestrator` (drives the per-session agent graph synchronously)
//! -> egress -> reply text returned to the caller.
//!
//! # Examples
//!
//! ```no_run
//! use claw_agent::{AgentSystem, ClawApiConfig};
//!
//! let llm = ClawApiConfig {
//!     api_key: Some("sk-...".into()),
//!     backend_type: "openai_compatible".into(),
//!     model: Some("gpt-4o-mini".into()),
//!     base_url: Some("https://api.openai.com/v1".into()),
//!     supports_tools: true,
//!     ..Default::default()
//! };
//!
//! let system = AgentSystem::on_disk(llm, "/tmp/claw-mem")?;
//! let chat = system.chat();
//! for reply in chat.send("hello") {
//!     println!("{reply}");
//! }
//! # Ok::<(), claw_agent::AgentError>(())
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use claw_core::agent::FsAgentFactory;
use claw_core::{
    ChannelEgress, ChannelEgressHub, ChannelIngressSink, ChannelTransport, InboundMessage,
    Orchestrator, RecordingTransport,
};
use claw_interface::http::ClawHttp;
use claw_interface::{RealHttp, StdThread};
use claw_memory::{MemoryTaskPool, NoopCompactor, PoolConfig};

// Re-exported so callers can configure the system without depending on the lower
// crates directly. These names are also used internally below.
pub use claw_api::{ClawApi, ClawApiConfig};
pub use claw_core::agent::{AgentResolver, MapAgentResolver};
pub use claw_core::SessionId;
pub use claw_interface::{ClawFs, DiskFs};
pub use claw_memory::ConversationDeps;

/// The channel id outbound replies are routed through. Callers never see it; it
/// only needs to be stable so the egress transport and reply route agree.
const DEFAULT_CHANNEL: &str = "claw";

/// What can go wrong while building an [`AgentSystem`].
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// No LLM config was provided to the builder.
    #[error("LLM config is required")]
    MissingLlmConfig,
    /// No memory base directory was provided to the builder.
    #[error("memory directory is required")]
    MissingMemoryDir,
    /// No memory collaborators (fs/pool/compactor) were provided to the builder.
    #[error("memory collaborators are required")]
    MissingMemoryDeps,
    /// The background memory task pool could not start (e.g. thread spawn failed).
    #[error("failed to start the memory task pool: {0}")]
    MemoryPool(#[from] std::io::Error),
}

/// A ready-to-drive agent runtime.
///
/// Wraps a `claw_core::Orchestrator` plus the egress transport replies come back
/// on. Open a [`Chat`] with [`chat`](AgentSystem::chat) (one session) or drive
/// raw sessions with [`new_session`](AgentSystem::new_session) +
/// [`send`](AgentSystem::send).
pub struct AgentSystem {
    orchestrator: Arc<Orchestrator>,
    /// Records outbound replies; drained after each synchronous `send`.
    transport: Arc<RecordingTransport>,
    channel: String,
    /// Monotonic source for inbound `message_id`s.
    next_message_id: AtomicU64,
}

impl AgentSystem {
    /// Start a fully injectable builder. Use this for tests or custom backends;
    /// for the common host case prefer [`AgentSystem::on_disk`].
    ///
    /// `F` is the concrete [`ClawFs`] backing conversation memory (e.g.
    /// `Arc<DiskFs>` on a host, an in-memory double in tests).
    pub fn builder<F: ClawFs + Clone + 'static>() -> AgentSystemBuilder<F> {
        AgentSystemBuilder::default()
    }

    /// Build a host agent system backed by real disk memory and a live HTTP
    /// transport, with no extra capabilities/skills resolver.
    ///
    /// `memory_dir` is the base directory under which each agent keys its own
    /// conversation files.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::MemoryPool`] if the background memory task pool
    /// cannot be created.
    pub fn on_disk(
        llm: ClawApiConfig,
        memory_dir: impl Into<String>,
    ) -> Result<AgentSystem, AgentError> {
        let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default(), StdThread::default())?);
        let memory_deps = ConversationDeps {
            fs: Arc::new(DiskFs::absolute()),
            pool,
            compactor: Arc::new(NoopCompactor),
        };
        AgentSystem::builder::<Arc<DiskFs>>()
            .llm(llm)
            .memory_dir(memory_dir)
            .memory_deps(memory_deps)
            .build()
    }

    /// Create a fresh, isolated conversation session and return its id.
    pub fn new_session(&self) -> SessionId {
        self.orchestrator.session_create()
    }

    /// Open a new [`Chat`] (its own session) on this system.
    pub fn chat(&self) -> Chat<'_> {
        let session = self.new_session();
        Chat {
            system: self,
            session,
        }
    }

    /// Deliver `text` to `session` and return the reply text(s) the agent
    /// produced this turn.
    ///
    /// The orchestrator drives the session's agent graph synchronously, so by the
    /// time this returns every reply (and any surfaced approval prompt) for this
    /// turn has been routed and is collected here. Pending approvals appear as
    /// reply text tagged `[approval needed ...]`.
    pub fn send(&self, session: SessionId, text: impl Into<String>) -> Vec<String> {
        let id = self.next_message_id.fetch_add(1, Ordering::Relaxed);
        self.orchestrator.push_user_message(InboundMessage {
            message_id: format!("m{id}"),
            channel: self.channel.clone(),
            chat_id: session.to_wire(),
            sender_id: None,
            session_id: session.to_wire(),
            text: text.into(),
        });
        self.transport
            .drain_sent()
            .into_iter()
            .map(|message| message.text)
            .collect()
    }
}

/// A single conversation bound to one session of an [`AgentSystem`].
///
/// Borrows the system so several chats can run against it without giving up
/// ownership; each chat keeps its own [`SessionId`].
pub struct Chat<'system> {
    system: &'system AgentSystem,
    session: SessionId,
}

impl Chat<'_> {
    /// This chat's underlying session id.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Send a message and return the agent's reply text(s) for this turn.
    pub fn send(&self, text: impl Into<String>) -> Vec<String> {
        self.system.send(self.session, text)
    }
}

/// Builder for [`AgentSystem`]. Required: an LLM config, a memory directory, and
/// memory collaborators. Optional: the HTTP transport (defaults to live
/// [`RealHttp`]), the capability/skill [`AgentResolver`] (defaults to empty), and
/// the egress channel id.
pub struct AgentSystemBuilder<F: ClawFs + Clone + 'static> {
    llm_config: Option<ClawApiConfig>,
    http: Option<Arc<dyn ClawHttp>>,
    resolver: Option<Arc<dyn AgentResolver>>,
    memory_dir: Option<String>,
    memory_deps: Option<ConversationDeps<F>>,
    channel: String,
}

impl<F: ClawFs + Clone + 'static> Default for AgentSystemBuilder<F> {
    fn default() -> Self {
        Self {
            llm_config: None,
            http: None,
            resolver: None,
            memory_dir: None,
            memory_deps: None,
            channel: DEFAULT_CHANNEL.to_string(),
        }
    }
}

impl<F: ClawFs + Clone + 'static> AgentSystemBuilder<F> {
    /// Required: the LLM client config every agent is minted from.
    pub fn llm(mut self, config: ClawApiConfig) -> Self {
        self.llm_config = Some(config);
        self
    }

    /// Override the HTTP transport (default: live [`RealHttp`]). Inject a scripted
    /// double here in tests.
    pub fn http(mut self, http: Arc<dyn ClawHttp>) -> Self {
        self.http = Some(http);
        self
    }

    /// Override the capability/skill resolver (default: empty
    /// [`MapAgentResolver`]).
    pub fn resolver(mut self, resolver: Arc<dyn AgentResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Required: base directory for conversation memory.
    pub fn memory_dir(mut self, dir: impl Into<String>) -> Self {
        self.memory_dir = Some(dir.into());
        self
    }

    /// Required: the memory collaborators (fs/pool/compactor) cloned into each
    /// agent.
    pub fn memory_deps(mut self, deps: ConversationDeps<F>) -> Self {
        self.memory_deps = Some(deps);
        self
    }

    /// Override the egress channel id (default: `"claw"`). Rarely needed.
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = channel.into();
        self
    }

    /// Assemble the [`AgentSystem`].
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::MissingLlmConfig`], [`AgentError::MissingMemoryDir`],
    /// or [`AgentError::MissingMemoryDeps`] when a required input was not set.
    pub fn build(self) -> Result<AgentSystem, AgentError> {
        let llm_config = self.llm_config.ok_or(AgentError::MissingLlmConfig)?;
        let memory_dir = self.memory_dir.ok_or(AgentError::MissingMemoryDir)?;
        let memory_deps = self.memory_deps.ok_or(AgentError::MissingMemoryDeps)?;
        let http = self.http.unwrap_or_else(|| Arc::new(RealHttp::new()));
        let resolver = self
            .resolver
            .unwrap_or_else(|| Arc::new(MapAgentResolver::new()));

        let factory = Arc::new(FsAgentFactory::new(
            resolver,
            llm_config,
            http,
            memory_dir,
            memory_deps,
        ));

        let transport = RecordingTransport::new(self.channel.clone());
        let egress = Arc::new(ChannelEgressHub::new());
        egress.register(Arc::clone(&transport) as Arc<dyn ChannelTransport>);

        let orchestrator = Orchestrator::builder()
            .config_egress(egress as Arc<dyn ChannelEgress>)
            .with_agent_factory(factory)
            .build();

        Ok(AgentSystem {
            orchestrator,
            transport,
            channel: self.channel,
            next_message_id: AtomicU64::new(1),
        })
    }
}
