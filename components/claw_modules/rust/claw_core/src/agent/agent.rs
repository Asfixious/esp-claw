//! The single generic, flat agent — one runtime, configured by data.
//!
//! There is no semantic FSM. [`GenericAgent`] is a thin wrapper over a
//! [`BaseAgent`](crate::agent::base_agent::BaseAgent): it forwards commands and
//! ticks straight through, and the only thing that distinguishes one agent
//! "kind" from another is its [`AgentConfig`] — the system prompt, the tool set,
//! the skills, and whether it may spawn. The model drives its own flow (ReAct):
//! it reads, acts, and answers freely, ending a task only via the built-in
//! `end_conversation` tool.
//!
//! # The data/code boundary
//!
//! A kind's behavior is defined on the filesystem at compile time under
//! `resources/agents/<kind>/` and baked in with [`agent_manifest!`]. A manifest
//! is **pure data** (a system prompt plus the *names* of capabilities/skills);
//! the actual tool/skill *handlers* are code, so turning a manifest into a
//! runnable [`AgentConfig`] needs an [`AgentResolver`] that maps names to
//! handlers (provided by the firmware, faked in tests). The two halves meet in
//! [`AgentConfig::from_manifest`]:
//!
//! ```text
//!   agent_manifest!("worker")        // compile time: bake bytes
//!     -> AgentManifest (data)
//!     -> AgentConfig::from_manifest(manifest, resolver, spawner, responder)  // runtime: parse + resolve
//!     -> GenericAgent::new(id, llm, memory_config, memory_deps, config)  // wrap BaseAgent
//! ```

use std::sync::Arc;

use claw_api::{ClawApi, RetryPolicy};
use claw_memory::{ConversationConfig, ConversationDeps, ConversationMemory};
use serde::Deserialize;

use crate::agent::base_agent::{
    AgentCommand, AgentCommandError, AgentId, BaseAgent, BaseAgentBuildError, TickOutcome,
};
use crate::agent::internal_tools::{
    respond_to_approval_tool_group, spawn_tool_group, ApprovalResponder, Spawner,
};
use crate::agent::{append_child_result, Agent};
use crate::skills::{SkillError, SkillId, SkillSet};
use crate::tools::{Tool, ToolSet, ToolSetError};

// ===========================================================================
// AgentKind
// ===========================================================================

/// Which agent *template* (role) to instantiate — the directory name under
/// `resources/agents/<kind>/`.
///
/// A kind is a **type**, one-to-many with running instances: every spawned agent
/// gets a unique [`AgentId`], but many of them can share the same `AgentKind`.
/// The spawning model picks the kind when it delegates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentKind(Box<str>);

impl AgentKind {
    /// Wrap a role name as a kind.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into().into_boxed_str())
    }

    /// The kind as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ===========================================================================
// Manifest: compile-time-baked agent definition (pure data)
// ===========================================================================

/// A compile-time-baked agent definition: a system prompt plus the raw JSON of
/// the capability/skill name lists and the `agent.json` metadata.
///
/// All fields are `&'static str` (`include_str!`-ed bytes); no JSON is parsed and
/// no handler is resolved until [`AgentConfig::from_manifest`] runs at boot.
#[derive(Clone, Copy, Debug)]
pub struct AgentManifest {
    /// `instructions.md` — the agent's persona/process guidance (system prompt).
    pub instructions: &'static str,
    /// `capabilities/capabilities.json` — a schema-versioned object whose
    /// `capabilities` field lists capability (tool) names.
    pub capabilities_json: &'static str,
    /// `skills/skills.json` — a schema-versioned object whose `skills` field
    /// lists skill ids.
    pub skills_json: &'static str,
    /// `agent.json` — the kind's metadata header (kind, description, spawn, runtime).
    pub agent_json: &'static str,
}

/// Bake an [`AgentManifest`] for `$kind` from `resources/agents/<kind>/` at
/// compile time.
///
/// Crate-internal on purpose: `env!("CARGO_MANIFEST_DIR")` expands at the call
/// site, so this must be invoked inside `claw_core` (where the resources live).
/// External callers consume the baked kinds through [`builtin_manifest`].
macro_rules! agent_manifest {
    ($kind:literal) => {
        $crate::agent::agent::AgentManifest {
            instructions: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/agents/",
                $kind,
                "/instructions.md"
            )),
            capabilities_json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/agents/",
                $kind,
                "/capabilities/capabilities.json"
            )),
            skills_json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/agents/",
                $kind,
                "/skills/skills.json"
            )),
            agent_json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/agents/",
                $kind,
                "/agent.json"
            )),
        }
    };
}

/// The conversation root manifest.
static CONVERSATION_MANIFEST: AgentManifest = agent_manifest!("conversation");
/// The worker manifest.
static WORKER_MANIFEST: AgentManifest = agent_manifest!("worker");

/// Look up a firmware-baked agent manifest by kind name.
///
/// Returns `None` for an unknown kind; the caller turns that into an error rather
/// than substituting a default template.
pub fn builtin_manifest(kind: &str) -> Option<&'static AgentManifest> {
    match kind {
        "conversation" => Some(&CONVERSATION_MANIFEST),
        "worker" => Some(&WORKER_MANIFEST),
        _ => None,
    }
}

/// The set of kinds baked into the firmware (for validating spawn requests).
pub const BUILTIN_KINDS: &[&str] = &["conversation", "worker"];

// ===========================================================================
// Resolver: the name -> handler boundary (code, injected)
// ===========================================================================

/// Resolves the *names* in a manifest to the *code* that backs them.
///
/// A manifest only carries capability/skill names; the handlers live in firmware
/// (or a test double). The resolver is the injected boundary that turns each name
/// into a real [`Tool`] / [`SkillSet`]. An unknown name is **not** silently
/// dropped — the resolver returns `None`/an error and the build fails.
pub trait AgentResolver: Send + Sync {
    /// Resolve a capability name to its [`Tool`], or `None` if this resolver has
    /// no such capability.
    fn resolve_tool(&self, name: &str) -> Option<Tool>;

    /// Build a [`SkillSet`] with `skill_ids` loaded.
    ///
    /// Returns `Ok(None)` when no skills are configured (`skill_ids` empty) or the
    /// resolver has no skill support; `Ok(Some(set))` once the ids are loaded.
    ///
    /// # Errors
    ///
    /// [`SkillError`] if a requested skill id is unknown to the resolver.
    fn build_skills(&self, skill_ids: &[SkillId]) -> Result<Option<SkillSet>, SkillError>;
}

// ===========================================================================
// AgentConfig: the typed seam GenericAgent consumes
// ===========================================================================

/// A fully-resolved agent configuration — the typed seam between *where the
/// config came from* (a manifest, a builder, a test) and *the agent that runs
/// it*. [`GenericAgent`] consumes only this and never touches the filesystem.
pub struct AgentConfig {
    kind: AgentKind,
    system_prompt: String,
    tools: Vec<Tool>,
    skills: Option<SkillSet>,
    spawner: Option<Arc<dyn Spawner>>,
    /// Set only for a session root: gives it the `respond_to_approval` tool so it
    /// can feed user verdicts back to waiting subagents.
    approval_responder: Option<Arc<dyn ApprovalResponder>>,
    retry_policy: RetryPolicy,
    tool_block_retries: Option<u32>,
}

impl AgentConfig {
    /// Start building a config for `kind` by hand (used by tests and CLIs that do
    /// not go through a manifest).
    pub fn builder(kind: impl Into<String>) -> AgentConfigBuilder {
        AgentConfigBuilder {
            kind: AgentKind::new(kind),
            system_prompt: String::new(),
            tools: Vec::new(),
            skills: None,
            spawner: None,
            approval_responder: None,
            retry_policy: RetryPolicy::default(),
            tool_block_retries: None,
        }
    }

    /// Resolve a compile-time [`AgentManifest`] into a runnable config.
    ///
    /// Parses the manifest's JSON, resolves every capability/skill name through
    /// `resolver`, and attaches `spawner` when the kind may spawn. The `kind`
    /// argument selects which template was loaded and is validated against the
    /// manifest's own `agent.json` `kind`.
    ///
    /// `approval_responder` is passed straight through (the registry supplies it
    /// only for a session root); it gives the agent the `respond_to_approval`
    /// tool. Unlike `spawner` it is not gated by the manifest — being the root is a
    /// runtime property the registry decides, not a kind property.
    ///
    /// # Errors
    ///
    /// [`AgentConfigError`] when the JSON is malformed, the declared kind does not
    /// match, a capability name has no handler, or a skill id is unknown.
    pub fn from_manifest(
        kind: &str,
        manifest: &AgentManifest,
        resolver: &dyn AgentResolver,
        spawner: Option<Arc<dyn Spawner>>,
        approval_responder: Option<Arc<dyn ApprovalResponder>>,
    ) -> Result<AgentConfig, AgentConfigError> {
        let meta: AgentMeta = serde_json::from_str(manifest.agent_json)
            .map_err(|error| AgentConfigError::Metadata(error.to_string()))?;
        if meta.kind != kind {
            return Err(AgentConfigError::KindMismatch {
                requested: kind.to_string(),
                declared: meta.kind,
            });
        }

        let capabilities: CapabilitiesManifest =
            serde_json::from_str(manifest.capabilities_json)
                .map_err(|error| AgentConfigError::Capabilities(error.to_string()))?;
        let mut tools = Vec::with_capacity(capabilities.capabilities.len());
        for name in &capabilities.capabilities {
            let tool = resolver
                .resolve_tool(name)
                .ok_or_else(|| AgentConfigError::UnknownCapability(name.clone()))?;
            tools.push(tool);
        }

        let skills_manifest: SkillsManifest = serde_json::from_str(manifest.skills_json)
            .map_err(|error| AgentConfigError::Skills(error.to_string()))?;
        let skill_ids: Vec<SkillId> = skills_manifest
            .skills
            .into_iter()
            .map(SkillId::new)
            .collect();
        let skills = resolver.build_skills(&skill_ids)?;

        // Spawning is honored only when the kind allows it and a spawner is wired.
        let spawner = if meta.spawn.enabled { spawner } else { None };

        Ok(AgentConfig {
            kind: AgentKind::new(meta.kind),
            system_prompt: manifest.instructions.trim().to_string(),
            tools,
            skills,
            spawner,
            approval_responder,
            retry_policy: RetryPolicy::new(meta.runtime.retries),
            tool_block_retries: meta.runtime.tool_block_retries,
        })
    }

    /// This config's kind.
    pub fn kind(&self) -> &AgentKind {
        &self.kind
    }
}

/// Builder for an [`AgentConfig`] assembled by hand.
#[must_use = "an AgentConfigBuilder does nothing until `.build()` is called"]
pub struct AgentConfigBuilder {
    kind: AgentKind,
    system_prompt: String,
    tools: Vec<Tool>,
    skills: Option<SkillSet>,
    spawner: Option<Arc<dyn Spawner>>,
    approval_responder: Option<Arc<dyn ApprovalResponder>>,
    retry_policy: RetryPolicy,
    tool_block_retries: Option<u32>,
}

impl AgentConfigBuilder {
    /// Set the agent's system prompt (persona/process guidance).
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Set the caller-provided capability tools. The built-in control tools
    /// (`end_conversation`, `request_approval`) and the spawn tool are added
    /// automatically at [`GenericAgent::new`].
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools = tools.into_iter().collect();
        self
    }

    /// Set the agent's skills.
    pub fn with_skills(mut self, skills: SkillSet) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Allow this agent to spawn subagents through `spawner`.
    pub fn with_spawner(mut self, spawner: Arc<dyn Spawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Give this agent (a session root) the `respond_to_approval` tool, backed by
    /// `responder`, so it can resolve subagents' approval requests.
    pub fn with_approval_responder(mut self, responder: Arc<dyn ApprovalResponder>) -> Self {
        self.approval_responder = Some(responder);
        self
    }

    /// Override the per-iteration LLM [`RetryPolicy`].
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Override how many consecutive gating-blocked tool rounds to tolerate.
    pub fn with_tool_block_retries(mut self, retries: u32) -> Self {
        self.tool_block_retries = Some(retries);
        self
    }

    /// Finish into an [`AgentConfig`].
    pub fn build(self) -> AgentConfig {
        AgentConfig {
            kind: self.kind,
            system_prompt: self.system_prompt,
            tools: self.tools,
            skills: self.skills,
            spawner: self.spawner,
            approval_responder: self.approval_responder,
            retry_policy: self.retry_policy,
            tool_block_retries: self.tool_block_retries,
        }
    }
}

/// The `agent.json` metadata header for one kind.
#[derive(Debug, Deserialize)]
struct AgentMeta {
    #[allow(dead_code)]
    schema_version: u32,
    kind: String,
    #[allow(dead_code)]
    description: String,
    spawn: SpawnMeta,
    runtime: RuntimeMeta,
}

/// The `capabilities/capabilities.json` document: a schema-versioned list of
/// capability (tool) names this kind may use.
#[derive(Debug, Deserialize)]
struct CapabilitiesManifest {
    #[allow(dead_code)]
    schema_version: u32,
    capabilities: Vec<String>,
}

/// The `skills/skills.json` document: a schema-versioned list of skill ids this
/// kind loads.
#[derive(Debug, Deserialize)]
struct SkillsManifest {
    #[allow(dead_code)]
    schema_version: u32,
    skills: Vec<String>,
}

/// The `spawn` block of `agent.json`.
#[derive(Debug, Deserialize)]
struct SpawnMeta {
    enabled: bool,
    #[allow(dead_code)]
    allowed_kinds: Vec<String>,
}

/// The `runtime` block of `agent.json`.
#[derive(Debug, Deserialize)]
struct RuntimeMeta {
    retries: u32,
    #[serde(default)]
    tool_block_retries: Option<u32>,
}

/// Failure turning an [`AgentManifest`] into an [`AgentConfig`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum AgentConfigError {
    /// `agent.json` was not valid JSON for the expected metadata shape.
    #[error("invalid agent.json: {0}")]
    Metadata(String),
    /// `capabilities.json` was not a valid JSON array of names.
    #[error("invalid capabilities.json: {0}")]
    Capabilities(String),
    /// `skills.json` was not a valid JSON array of ids.
    #[error("invalid skills.json: {0}")]
    Skills(String),
    /// The kind requested does not match the one declared in `agent.json`.
    #[error("kind mismatch: requested '{requested}', manifest declares '{declared}'")]
    KindMismatch {
        /// The kind the loader asked for.
        requested: String,
        /// The kind the manifest's `agent.json` declares.
        declared: String,
    },
    /// A capability name in the manifest has no handler in the resolver.
    #[error("unknown capability: {0}")]
    UnknownCapability(String),
    /// Building the skill set failed (e.g. an unknown skill id).
    #[error(transparent)]
    Skill(#[from] SkillError),
}

// ===========================================================================
// GenericAgent: the flat Agent over BaseAgent
// ===========================================================================

/// The one agent type: a flat ReAct loop over a [`BaseAgent`], configured by an
/// [`AgentConfig`]. No semantic FSM — `tick` forwards straight to the base.
pub struct GenericAgent {
    id: AgentId,
    kind: AgentKind,
    /// A view sharing inner state with the base agent's memory, for reads and
    /// persistence inspection (a cheap `Arc` clone of the live transcript).
    memory: ConversationMemory,
    base: BaseAgent,
}

impl GenericAgent {
    /// Build a generic agent with `id` over `llm`, configured by `config`.
    ///
    /// The agent constructs its **own** [`ConversationMemory`] from the injected
    /// `memory_config` (base dir + tuning) and `memory_deps` (fs/pool/compactor),
    /// keyed by its own `id`. Keeping memory construction inside the agent means a
    /// caller cannot wire a transcript that belongs to a different agent: the
    /// conversation identity always follows the agent identity.
    ///
    /// The config's capability tools are merged with the spawn tool (when a
    /// spawner is set); the base agent then adds its built-in control tools.
    ///
    /// # Errors
    ///
    /// [`GenericAgentBuildError`] when tool assembly or base construction fails.
    pub fn new(
        id: AgentId,
        llm: ClawApi,
        memory_config: ConversationConfig,
        memory_deps: ConversationDeps,
        config: AgentConfig,
    ) -> Result<Self, GenericAgentBuildError> {
        // Memory identity follows agent identity: the conversation is keyed by the
        // agent's own id, so the transcript can never be mismatched by the caller.
        let memory = ConversationMemory::new(id.0, memory_config, memory_deps);
        let memory_view = memory.clone();

        let mut tool_set = ToolSet::new(config.tools)?;
        if let Some(spawner) = config.spawner {
            tool_set.extend_with_group(spawn_tool_group(spawner, id))?;
        }
        if let Some(responder) = config.approval_responder {
            tool_set.extend_with_group(respond_to_approval_tool_group(responder))?;
        }

        let mut base_builder = BaseAgent::builder(llm, memory)
            .with_system_prompt(config.system_prompt)
            .with_tools(tool_set)
            .with_retry_policy(config.retry_policy);
        if let Some(retries) = config.tool_block_retries {
            base_builder = base_builder.with_tool_block_retries(retries);
        }
        if let Some(skills) = config.skills {
            base_builder = base_builder.with_skills(skills);
        }
        let base = base_builder.build()?;

        Ok(Self {
            id,
            kind: config.kind,
            memory: memory_view,
            base,
        })
    }

    /// This agent's kind.
    pub fn kind(&self) -> &AgentKind {
        &self.kind
    }

    /// A read-only view of this agent's conversation memory.
    ///
    /// Shares inner state with the live agent (cheap `Arc` clone), so reads always
    /// reflect the current transcript. Intended for inspection and persistence,
    /// not direct mutation (the agent owns writes through its tick loop).
    pub fn memory(&self) -> &ConversationMemory {
        &self.memory
    }
}

impl Agent for GenericAgent {
    fn id(&self) -> AgentId {
        self.id
    }

    fn send_command(&mut self, command: AgentCommand) -> Result<(), AgentCommandError> {
        self.base.send_command(command)
    }

    fn deliver_child_result(&mut self, child: AgentId, text: String, ok: bool) {
        append_child_result(&mut self.base, child, text, ok);
    }

    fn tick(&mut self) -> TickOutcome {
        // Flat: no FSM, no per-phase gating — the model drives its own flow.
        self.base.tick()
    }

    fn transcript(&self) -> Option<serde_json::Value> {
        Some(self.memory.messages())
    }
}

/// Failure assembling a [`GenericAgent`].
#[derive(Debug, thiserror::Error)]
pub enum GenericAgentBuildError {
    /// Assembling the tool set failed (e.g. a duplicate tool name).
    #[error(transparent)]
    Tools(#[from] ToolSetError),
    /// The underlying [`BaseAgent`] could not be built.
    #[error(transparent)]
    Base(#[from] BaseAgentBuildError),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use claw_api::{ClawApi, ClawApiConfig};
    use claw_interface::{MemFs, ScriptedHttp};
    use claw_memory::{
        ConversationConfig, ConversationDeps, ConversationMemory, MemoryTaskPool, NoopCompactor,
        PoolConfig,
    };
    use serde_json::{json, Value};

    use super::*;

    fn scripted_llm(bodies: Vec<String>) -> ClawApi {
        ClawApi::init(
            ClawApiConfig {
                api_key: Some("sk-test".into()),
                backend_type: "openai_compatible".into(),
                model: Some("gpt-test".into()),
                base_url: Some("https://example.invalid".into()),
                supports_tools: true,
                ..Default::default()
            },
            Arc::new(ScriptedHttp::new(bodies)),
        )
        .expect("init llm")
    }

    /// The ingredients [`GenericAgent::new`] needs to build its own memory: a
    /// base config plus the in-memory collaborators. The agent keys the
    /// conversation by its own id.
    fn memory_ingredients(agent_id: AgentId) -> (ConversationConfig, ConversationDeps) {
        let pool = Arc::new(MemoryTaskPool::new(PoolConfig::default()).expect("memory pool"));
        (
            ConversationConfig::new(format!("/mem/agent-{}", agent_id.0)),
            ConversationDeps {
                fs: Arc::new(MemFs::default()),
                pool,
                compactor: Arc::new(NoopCompactor),
            },
        )
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

    fn transcript_contents(memory: &ConversationMemory) -> Vec<String> {
        memory
            .messages()
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

    fn drive(agent: &mut GenericAgent) -> TickOutcome {
        loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                other => return other,
            }
        }
    }

    /// A test resolver that maps names to no-op echo tools and supports no skills.
    struct StaticResolver;

    impl AgentResolver for StaticResolver {
        fn resolve_tool(&self, _name: &str) -> Option<Tool> {
            None
        }
        fn build_skills(&self, _skill_ids: &[SkillId]) -> Result<Option<SkillSet>, SkillError> {
            Ok(None)
        }
    }

    #[test]
    fn flat_agent_answers_directly() {
        let (mem_config, mem_deps) = memory_ingredients(AgentId(1));
        let config = AgentConfig::builder("conversation")
            .with_system_prompt("be helpful")
            .build();
        let mut agent = GenericAgent::new(
            AgentId(1),
            scripted_llm(vec![body_plain_text("hi")]),
            mem_config,
            mem_deps,
            config,
        )
        .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("hello".into()))
            .unwrap();
        match drive(&mut agent) {
            TickOutcome::Yielded { text } => assert_eq!(text, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(agent.kind().as_str(), "conversation");
    }

    #[test]
    fn end_conversation_ends_the_task() {
        let (mem_config, mem_deps) = memory_ingredients(AgentId(1));
        let body = body_tool_call(
            "e1",
            "end_conversation",
            &json!({ "final_message": "bye" }).to_string(),
        );
        let config = AgentConfig::builder("worker").build();
        let mut agent = GenericAgent::new(
            AgentId(1),
            scripted_llm(vec![body]),
            mem_config,
            mem_deps,
            config,
        )
        .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("go".into()))
            .unwrap();
        match drive(&mut agent) {
            TickOutcome::Ended { final_message } => assert_eq!(final_message, "bye"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn child_result_is_appended_as_message() {
        let (mem_config, mem_deps) = memory_ingredients(AgentId(1));
        let config = AgentConfig::builder("conversation").build();
        let mut agent = GenericAgent::new(
            AgentId(1),
            scripted_llm(vec![body_plain_text("ack")]),
            mem_config,
            mem_deps,
            config,
        )
        .unwrap();

        agent.deliver_child_result(AgentId(7), "subtask output".into(), true);
        let _ = drive(&mut agent);

        assert!(transcript_contents(agent.memory())
            .iter()
            .any(|c| c.contains("[subagent agent-7 ok] subtask output")));
    }

    #[test]
    fn respond_to_approval_tool_is_wired_when_a_responder_is_configured() {
        use std::sync::Mutex;

        use crate::agent::internal_tools::ApprovalVerdict;

        #[derive(Default)]
        struct RecordingResponder(Mutex<Vec<(AgentId, ApprovalVerdict, Option<String>)>>);
        impl ApprovalResponder for RecordingResponder {
            fn respond(&self, target: AgentId, verdict: ApprovalVerdict, note: Option<String>) {
                self.0.lock().unwrap().push((target, verdict, note));
            }
        }

        let responder = Arc::new(RecordingResponder::default());
        let (mem_config, mem_deps) = memory_ingredients(AgentId(1));
        let config = AgentConfig::builder("conversation")
            .with_approval_responder(Arc::clone(&responder) as Arc<dyn ApprovalResponder>)
            .build();
        let mut agent = GenericAgent::new(
            AgentId(1),
            scripted_llm(vec![
                body_tool_call(
                    "a1",
                    "respond_to_approval",
                    &json!({ "agent": "agent-7", "verdict": "no", "note": "too risky" })
                        .to_string(),
                ),
                body_plain_text("handled"),
            ]),
            mem_config,
            mem_deps,
            config,
        )
        .unwrap();

        agent
            .send_command(AgentCommand::AppendMessage("the user said no".into()))
            .unwrap();
        let _ = drive(&mut agent);

        let recorded = responder.0.lock().unwrap();
        assert_eq!(
            recorded.as_slice(),
            &[(
                AgentId(7),
                ApprovalVerdict::No,
                Some("too risky".to_string())
            )]
        );
    }

    #[test]
    fn builtin_manifests_resolve_with_a_resolver() {
        for kind in BUILTIN_KINDS {
            let manifest = builtin_manifest(kind).expect("manifest exists");
            let config = AgentConfig::from_manifest(kind, manifest, &StaticResolver, None, None)
                .unwrap_or_else(|error| panic!("kind {kind} failed: {error}"));
            assert_eq!(config.kind().as_str(), *kind);
            assert!(
                !config.system_prompt.is_empty(),
                "kind {kind} has no prompt"
            );
        }
    }

    #[test]
    fn unknown_kind_has_no_manifest() {
        assert!(builtin_manifest("nope").is_none());
    }

    #[test]
    fn kind_mismatch_is_rejected() {
        let manifest = builtin_manifest("worker").expect("worker manifest");
        let result =
            AgentConfig::from_manifest("conversation", manifest, &StaticResolver, None, None);
        assert!(matches!(result, Err(AgentConfigError::KindMismatch { .. })));
    }
}
