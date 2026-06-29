//! [`LongTermMemoryAdapter`] — the [`Memory`] that fronts the dual-tier long-term
//! store, owning extraction scheduling, context contribution, and the memory
//! tools.
//!
//! It holds two [`LongTermMemory`] stores — a **global** one shared across every
//! agent and a **agent** one private to this agent — and presents them to the
//! model as a *single* memory: the tools take no tier parameter, recall/list
//! merge both stores, and stores route through the [`TierClassifier`]. The model
//! therefore never has to reason about where a fact lives; it just remembers and
//! recalls.
//!
//! The id prefix a store mints (`g-` global, `a-` agent) is the routing key for
//! id-addressed operations (`update`/`forget`): the prefix is opaque to the
//! model but lets the adapter send an edit back to the store that owns the item.

use std::sync::{Arc, Mutex};

use claw_context::{Block, BlockKind};
use claw_interface::ClawFs;
use claw_memory::{
    LongTermConfig, LongTermError, LongTermMemory, MemoryDraft, MemoryId, MemoryItem, MemoryPatch,
    StoreOutcome,
};
use claw_memory::MemoryTaskPool;
use claw_tool::ToolGroup;

use crate::memory::bus::{AgentEvent, Memory, MemoryError};
use crate::memory::extraction::Extractor;
use crate::memory::tier::{MemoryTier, TierClassifier};
use crate::memory::tools::memory_tool_group;

/// Id prefix for the shared global store.
pub const GLOBAL_ID_PREFIX: &str = "g-";
/// Id prefix for the per-agent store.
pub const AGENT_ID_PREFIX: &str = "a-";

/// Default memory id used in logs.
const DEFAULT_MEMORY_ID: &str = "long_term";
/// Max recent transcript lines retained for extraction.
const RECENT_BUFFER_MAX: usize = 24;

/// Build a global long-term store under `dir` (minting `g-` ids).
pub fn global_store<F: ClawFs + 'static>(dir: impl Into<String>, fs: F) -> LongTermMemory<F> {
    LongTermMemory::new(LongTermConfig::new(dir, GLOBAL_ID_PREFIX), fs)
}

/// Build a per-agent long-term store under `dir` (minting `a-` ids).
pub fn agent_store<F: ClawFs + 'static>(dir: impl Into<String>, fs: F) -> LongTermMemory<F> {
    LongTermMemory::new(LongTermConfig::new(dir, AGENT_ID_PREFIX), fs)
}

/// The two stores plus the routing policy, shared (by cheap clone) between the
/// adapter, the extraction job, and every memory tool handler.
///
/// Every clone refers to the same underlying stores (each [`LongTermMemory`] is
/// `Arc`-backed), so a tool call on the tick thread and an extraction job on a
/// pool worker write the same data.
pub(crate) struct MemoryStores<F: ClawFs + 'static> {
    global: LongTermMemory<F>,
    agent: LongTermMemory<F>,
    classifier: Arc<dyn TierClassifier>,
}

impl<F: ClawFs + 'static> Clone for MemoryStores<F> {
    fn clone(&self) -> Self {
        Self {
            global: self.global.clone(),
            agent: self.agent.clone(),
            classifier: Arc::clone(&self.classifier),
        }
    }
}

impl<F: ClawFs + 'static> MemoryStores<F> {
    /// Store a draft, routing it to a tier via the classifier (`hint` from an
    /// extractor, or `None` for a manual store).
    pub(crate) fn store(&self, draft: MemoryDraft, hint: Option<MemoryTier>) -> StoreOutcome {
        match self.classifier.classify(&draft, hint) {
            MemoryTier::Global => self.global.store(draft),
            MemoryTier::Agent => self.agent.store(draft),
        }
    }

    /// Recall across both stores (global first), capped at `limit` total.
    pub(crate) fn recall(
        &self,
        labels: &[String],
        query: Option<&str>,
        limit: usize,
    ) -> Vec<MemoryItem> {
        let mut hits = self.global.recall(labels, query, limit);
        hits.extend(self.agent.recall(labels, query, limit));
        hits.truncate(limit);
        hits
    }

    /// All facts across both stores (global first).
    pub(crate) fn list(&self) -> Vec<MemoryItem> {
        let mut items = self.global.list();
        items.extend(self.agent.list());
        items
    }

    /// Apply a patch to the item with `id`, routing by its prefix.
    pub(crate) fn update(
        &self,
        id: &MemoryId,
        patch: MemoryPatch,
    ) -> Result<MemoryItem, LongTermError> {
        self.store_for(id).update(id, patch)
    }

    /// Forget the item with `id`, routing by its prefix.
    pub(crate) fn forget(&self, id: &MemoryId) -> Result<(), LongTermError> {
        self.store_for(id).forget(id)
    }

    /// The store that owns `id` (by its prefix; defaults to the agent store for
    /// an unrecognized prefix, which then reports `NotFound`).
    fn store_for(&self, id: &MemoryId) -> &LongTermMemory<F> {
        if id.as_str().starts_with(GLOBAL_ID_PREFIX) {
            &self.global
        } else {
            &self.agent
        }
    }
}

/// A [`Memory`] over a dual-tier long-term store. See the module docs.
pub struct LongTermMemoryAdapter<F: ClawFs + 'static> {
    id: String,
    stores: MemoryStores<F>,
    extractor: Arc<dyn Extractor>,
    pool: Arc<MemoryTaskPool>,
    /// Rolling recent-conversation lines fed to extraction (capped).
    recent: Mutex<Vec<String>>,
}

impl<F: ClawFs + 'static> LongTermMemoryAdapter<F> {
    /// Build an adapter over the two stores, a tier `classifier`, an `extractor`,
    /// and the shared memory `pool` that runs extraction off the tick path.
    pub fn new(
        agent: LongTermMemory<F>,
        global: LongTermMemory<F>,
        classifier: Arc<dyn TierClassifier>,
        extractor: Arc<dyn Extractor>,
        pool: Arc<MemoryTaskPool>,
    ) -> Self {
        Self {
            id: DEFAULT_MEMORY_ID.to_string(),
            stores: MemoryStores {
                global,
                agent,
                classifier,
            },
            extractor,
            pool,
            recent: Mutex::new(Vec::new()),
        }
    }

    /// Append a line to the rolling recent buffer, capping its length.
    fn push_recent(&self, line: String) {
        let mut recent = self.lock_recent();
        recent.push(line);
        let overflow = recent.len().saturating_sub(RECENT_BUFFER_MAX);
        if overflow > 0 {
            recent.drain(0..overflow);
        }
    }

    /// Schedule a background extraction over the current transcript snapshot.
    ///
    /// Runs on a pool worker: it calls the (possibly LLM-backed) extractor, then
    /// routes and stores each fact. Dedup in the store absorbs facts re-extracted
    /// on later turns, so repeated scheduling is safe.
    fn schedule_extraction(&self) {
        let transcript = self.lock_recent().join("\n");
        if transcript.trim().is_empty() {
            return;
        }
        let extractor = Arc::clone(&self.extractor);
        let stores = self.stores.clone();
        let memory_id = self.id.clone();
        self.pool.submit(Box::new(move || {
            let items = match extractor.extract(&transcript) {
                Ok(items) => items,
                Err(error) => {
                    tracing::warn!(%error, memory = %memory_id, "memory extraction failed");
                    return;
                }
            };
            for item in items {
                let draft = MemoryDraft::new(item.content)
                    .with_tags(item.tags)
                    .with_keywords(item.keywords)
                    .with_source("extracted");
                stores.store(draft, item.tier);
            }
        }));
    }

    fn lock_recent(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.recent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<F: ClawFs + 'static> Memory for LongTermMemoryAdapter<F> {
    fn id(&self) -> &str {
        &self.id
    }

    fn on_event(&self, event: &AgentEvent<'_>) -> Result<(), MemoryError> {
        match event {
            AgentEvent::UserMessage { text } => self.push_recent(format!("User: {text}")),
            AgentEvent::AssistantMessage { text } => {
                self.push_recent(format!("Assistant: {text}"));
                // A completed plain-text answer is a natural turn boundary;
                // extract over the accumulated transcript.
                self.schedule_extraction();
            }
            // Tool activity and lifecycle boundaries do not feed extraction.
            _ => {}
        }
        Ok(())
    }

    fn render_context(&self, emit: &mut dyn FnMut(Block<'_>)) {
        emit(Block::new(
            BlockKind::GlobalMemory,
            render_catalog("Shared long-term memory topics", &self.stores.global.catalog()),
        ));
        emit(Block::new(
            BlockKind::AgentMemory,
            render_catalog("Your long-term memory topics", &self.stores.agent.catalog()),
        ));
    }

    fn tools(&self) -> Option<ToolGroup> {
        Some(memory_tool_group(self.stores.clone()))
    }
}

/// Render a label catalog as a single durable-context line, or empty when there
/// are no labels (the context then drops the block).
fn render_catalog(header: &str, labels: &[String]) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{header}: {}", labels.join(", "))
    }
}
