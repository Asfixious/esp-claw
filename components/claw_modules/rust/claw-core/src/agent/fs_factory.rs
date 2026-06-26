//! [`FsAgentFactory`] — the production [`AgentFactory`].
//!
//! This is the concrete counterpart to the test factories: it turns a kind into a
//! real, running [`GenericAgent`] by tying the four pieces together:
//!
//! 1. the kind's compile-time-baked [`AgentManifest`](super::manifest::AgentManifest)
//!    (pure data: prompt + capability/skill *names*),
//! 2. an injected [`AgentResolver`] that maps those names to handler *code*,
//! 3. a live LLM client minted per agent from a shared config + transport, and
//! 4. per-agent on-disk conversation memory (one base dir; the agent keys its own
//!    files by id).
//!
//! The orchestrator instance calls
//! [`create_agent`](AgentFactory::create_agent) for every root and subagent,
//! handing it the graph host (always) and the root flag (only a session root gets
//! the `respond_to_approval` tool). The `goal` is seeded as the agent's first
//! user message so it starts working immediately.

use std::sync::Arc;

use claw_api::{ClawApi, ClawApiConfig};
use claw_interface::http::ClawHttp;
use claw_interface::ClawFs;
use claw_memory::{ConversationConfig, ConversationDeps};

use crate::agent::agent::{AgentKind, GenericAgent};
use crate::agent::base_agent::{AgentCommand, AgentId};
use crate::agent::config::{AgentConfig, AgentResolver};
use crate::agent::graph::GraphHost;
use crate::agent::registry::AgentFactory;
use crate::agent::Agent;

/// Builds real [`GenericAgent`]s from compile-time manifests, an injected
/// resolver, a shared LLM config/transport, and shared memory collaborators.
///
/// Construct one and hand it to the orchestrator via
/// [`with_agent_factory`](crate::OrchestratorBuilder::with_agent_factory); the
/// registry then uses it for every agent in every session.
pub struct FsAgentFactory<F: ClawFs + Clone + 'static> {
    /// Maps a manifest's capability/skill *names* to handler code.
    resolver: Arc<dyn AgentResolver>,
    /// Template for the per-agent LLM client (cloned, then `init`-ed per agent so
    /// each agent owns an independent client over the shared transport).
    llm_config: ClawApiConfig,
    /// The shared HTTP transport every minted client speaks through.
    http: Arc<dyn ClawHttp>,
    /// Base directory for conversation files; each agent keys its own files by id.
    memory_dir: String,
    /// Template memory collaborators (fs/pool/compactor) cloned into each agent's
    /// own [`ConversationDeps`]. `F` is a concrete, statically dispatched
    /// [`ClawFs`]; it must be `Clone` because every agent gets its own handle
    /// (use `Arc<ConcreteFs>` for a shared backend — `Arc<T>` is itself `ClawFs`).
    memory_deps: ConversationDeps<F>,
}

impl<F: ClawFs + Clone + 'static> FsAgentFactory<F> {
    /// Build a factory over the injected `resolver`, an LLM `llm_config` +
    /// `http` transport, and the memory base dir + collaborators.
    ///
    /// `memory_deps` is the template the firmware/host already knows how to build
    /// (real disk fs + task pool + compactor on device, in-memory doubles in
    /// tests); its `Arc`s are cloned per agent.
    pub fn new(
        resolver: Arc<dyn AgentResolver>,
        llm_config: ClawApiConfig,
        http: Arc<dyn ClawHttp>,
        memory_dir: impl Into<String>,
        memory_deps: ConversationDeps<F>,
    ) -> Self {
        Self {
            resolver,
            llm_config,
            http,
            memory_dir: memory_dir.into(),
            memory_deps,
        }
    }

    /// A fresh [`ConversationDeps`] sharing this factory's collaborators (a cheap
    /// `fs` clone — typically an `Arc` bump — plus `Arc` clones of pool/compactor).
    fn clone_memory_deps(&self) -> ConversationDeps<F> {
        ConversationDeps {
            fs: self.memory_deps.fs.clone(),
            pool: Arc::clone(&self.memory_deps.pool),
            compactor: Arc::clone(&self.memory_deps.compactor),
        }
    }
}

impl<F: ClawFs + Clone + 'static> AgentFactory for FsAgentFactory<F> {
    fn create_agent(
        &self,
        id: AgentId,
        kind: &AgentKind,
        goal: String,
        host: Arc<dyn GraphHost>,
        is_root: bool,
    ) -> Result<Box<dyn Agent>, String> {
        // The config is pure data; which graph tools attach is decided in
        // `GenericAgent::new` from the manifest's `spawn.enabled`, the host's
        // presence, and `is_root`.
        let config = AgentConfig::resolve(kind.as_str(), self.resolver.as_ref())
            .map_err(|error| format!("resolving config for kind '{kind}': {error}"))?;

        let llm = ClawApi::init(self.llm_config.clone(), Arc::clone(&self.http))
            .map_err(|error| format!("initializing LLM for {id}: {error}"))?;

        let memory_config = ConversationConfig::new(self.memory_dir.clone());
        let mut agent = GenericAgent::new(
            id,
            llm,
            memory_config,
            self.clone_memory_deps(),
            config,
            Some(host),
            is_root,
        )
        .map_err(|error| format!("building {kind} agent {id}: {error}"))?;

        // The goal is the agent's first task: seed it as a user message so the
        // agent has something to work on as soon as it is ticked.
        if !goal.trim().is_empty() {
            agent
                .send_command(AgentCommand::AppendMessage(goal))
                .map_err(|error| format!("seeding goal for {id}: {error}"))?;
        }

        Ok(Box::new(agent))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use claw_interface::{MemFs, ScriptedHttp, StdThread};
    use claw_memory::{MemoryTaskPool, NoopCompactor, PoolConfig};
    use serde_json::json;

    use super::*;
    use crate::agent::base_agent::TickOutcome;
    use crate::agent::graph::GraphEffect;
    use claw_tool::Tool;
    use claw_skill::{SkillError, SkillId, SkillSet};

    /// A resolver with no capabilities or skills — enough for the built-in
    /// manifests, whose capability/skill lists are currently empty.
    struct EmptyResolver;
    impl AgentResolver for EmptyResolver {
        fn resolve_tool(&self, _name: &str) -> Option<Tool> {
            None
        }
        fn build_skills(&self, _ids: &[SkillId]) -> Result<Option<SkillSet>, SkillError> {
            Ok(None)
        }
    }

    /// A graph host that is never expected to fire in these single-agent tests.
    struct NoopHost;
    impl GraphHost for NoopHost {
        fn next_id(&self) -> AgentId {
            AgentId(0)
        }
        fn emit(&self, _requester: AgentId, _effect: GraphEffect) {}
        fn snapshot(&self) -> Vec<crate::agent::graph::AgentSnapshot> {
            Vec::new()
        }
    }

    fn body_plain_text(text: &str) -> String {
        json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }

    fn factory(bodies: Vec<String>) -> FsAgentFactory<Arc<MemFs>> {
        let llm_config = ClawApiConfig {
            api_key: Some("sk-test".into()),
            backend_type: "openai_compatible".into(),
            model: Some("gpt-test".into()),
            base_url: Some("https://example.invalid".into()),
            supports_tools: true,
            ..Default::default()
        };
        let memory_deps = ConversationDeps {
            fs: Arc::new(MemFs::default()),
            pool: Arc::new(
                MemoryTaskPool::new(PoolConfig::default(), StdThread::default())
                    .expect("memory pool"),
            ),
            compactor: Arc::new(NoopCompactor),
        };
        FsAgentFactory::new(
            Arc::new(EmptyResolver),
            llm_config,
            Arc::new(ScriptedHttp::new(bodies)),
            "/mem/agents",
            memory_deps,
        )
    }

    #[test]
    fn builds_a_runnable_agent_seeded_with_its_goal() {
        let factory = factory(vec![body_plain_text("hello there")]);
        let mut agent = factory
            .create_agent(
                AgentId(1),
                &AgentKind::new("conversation"),
                "say hi".into(),
                Arc::new(NoopHost),
                true,
            )
            .expect("agent builds");

        // The goal was seeded, so ticking drives straight to the scripted reply.
        let outcome = loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                other => break other,
            }
        };
        match outcome {
            TickOutcome::Yielded { text } => assert_eq!(text, "hello there"),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let factory = factory(vec![]);
        let result = factory.create_agent(
            AgentId(1),
            &AgentKind::new("nope"),
            "x".into(),
            Arc::new(NoopHost),
            false,
        );
        assert!(result.is_err());
    }
}
