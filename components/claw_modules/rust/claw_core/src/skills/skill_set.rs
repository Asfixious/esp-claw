//! The per-agent [`SkillSet`]: which skills are loaded into context, and the
//! assembled prompt fragment they produce.
//!
//! Mirrors [`ToolSet`](crate::ToolSet), with one difference: a `SkillSet` is
//! **mutable** — skills load and unload at runtime without restarting the agent.
//! Because the assembled context is read every iteration but changes only when
//! the loaded set changes, it is **dirty-cached**: any mutation marks the cache
//! stale, and [`context`](SkillSet::context) rebuilds lazily on the next read.
//! Reads when nothing changed are O(1) and hand back a borrowed `&str`.

use std::sync::Arc;

use super::registry::SkillRegistry;
use super::skill::{SkillError, SkillId, SkillMetadata};

/// A named bundle of skills to load together (parallels `ToolGroup`).
pub struct SkillGroup {
    name: &'static str,
    skills: Vec<SkillId>,
}

impl SkillGroup {
    /// Bundle `skills` under the group identity `name`.
    pub fn new(name: &'static str, skills: impl IntoIterator<Item = SkillId>) -> Self {
        Self {
            name,
            skills: skills.into_iter().collect(),
        }
    }

    /// The group's identity.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// One loaded skill plus the group it was loaded under.
struct Loaded {
    group: &'static str,
    id: SkillId,
}

/// The agent's loaded skills and their cached, assembled context.
pub struct SkillSet {
    registry: Arc<dyn SkillRegistry>,
    loaded: Vec<Loaded>,
    /// Assembled context for the loaded skills; valid only when `!dirty`.
    cache: String,
    dirty: bool,
}

impl SkillSet {
    /// An empty set over `registry` — no skills loaded yet.
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            loaded: Vec::new(),
            cache: String::new(),
            dirty: false,
        }
    }

    /// The full catalog of loadable skills (borrowed from the registry).
    pub fn available(&self) -> &[SkillMetadata] {
        self.registry.catalog()
    }

    /// True when no skills are loaded.
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// Load one skill under `group`. No-op if already loaded.
    ///
    /// Returns [`SkillError::NotFound`] if the registry has no such skill, so a
    /// bad id surfaces immediately rather than at the next context rebuild.
    pub fn load(&mut self, group: &'static str, id: SkillId) -> Result<(), SkillError> {
        if self.registry.metadata(&id).is_none() {
            return Err(SkillError::NotFound(id.to_string()));
        }
        if self.loaded.iter().any(|loaded| loaded.id == id) {
            return Ok(());
        }
        self.loaded.push(Loaded { group, id });
        self.dirty = true;
        Ok(())
    }

    /// Load every skill in `group`.
    pub fn load_group(&mut self, group: SkillGroup) -> Result<(), SkillError> {
        let name = group.name;
        for id in group.skills {
            self.load(name, id)?;
        }
        Ok(())
    }

    /// Unload a skill by id. No-op if it was not loaded.
    pub fn unload(&mut self, id: &SkillId) {
        let before = self.loaded.len();
        self.loaded.retain(|loaded| &loaded.id != id);
        if self.loaded.len() != before {
            self.dirty = true;
        }
    }

    /// The assembled skill context for the prompt, rebuilt only if stale.
    ///
    /// Borrowed from the internal cache — no owned copy is handed out. `&mut self`
    /// because a stale cache is rebuilt in place; the agent calls this inside its
    /// `tick(&mut self)`. Returns the read on success; a document that fails to
    /// load surfaces as [`SkillError`] and leaves the cache stale for retry.
    pub fn context(&mut self) -> Result<&str, SkillError> {
        if self.dirty {
            self.cache = self.build()?;
            self.dirty = false;
        }
        Ok(&self.cache)
    }

    /// Assemble the loaded skills' documents into one fragment.
    fn build(&self) -> Result<String, SkillError> {
        let mut out = String::new();
        for loaded in &self.loaded {
            let document = self.registry.document(&loaded.id)?;
            out.push_str("## Skill: ");
            out.push_str(loaded.id.as_str());
            out.push_str(" (");
            out.push_str(loaded.group);
            out.push_str(")\n\n");
            out.push_str(document.trim());
            out.push_str("\n\n");
        }
        Ok(out)
    }
}
