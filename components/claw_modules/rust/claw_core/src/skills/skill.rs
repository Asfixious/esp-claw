//! Skill identity, catalog metadata, and `SKILL.md` front-matter parsing.
//!
//! A skill lives in `<root>/<id>/SKILL.md`, whose head is a JSON front-matter
//! block fenced by `---` lines:
//!
//! ```text
//! ---
//! { "name": "...", "description": "...",
//!   "metadata": { "cap_groups": [...], "manage_mode": "readonly" } }
//! ---
//! # markdown body …
//! ```
//!
//! The catalog (id + description + cap groups + manage mode) lives entirely in
//! that head, so it is read without touching the potentially large body.

use serde::Deserialize;
use thiserror::Error;

/// A skill's identity — its directory name under the skills root.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SkillId(String);

impl SkillId {
    /// Wrap a directory name as a skill id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a skill may be managed at runtime, declared in its `SKILL.md` metadata.
///
/// Serialized form matches the `SKILL.md` values: `"readonly"` / `"runtime"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManageMode {
    /// Fixed; cannot be registered/unregistered at runtime.
    #[default]
    ReadOnly,
    /// May be registered/unregistered at runtime.
    Runtime,
}

/// Cheap catalog row for one skill — everything except the document body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMetadata {
    id: SkillId,
    description: String,
    capability_groups: Vec<String>,
    manage_mode: ManageMode,
}

impl SkillMetadata {
    /// The skill's identity.
    pub fn id(&self) -> &SkillId {
        &self.id
    }

    /// One-line summary shown in the skills catalog.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Capability groups unlocked while the skill is active.
    pub fn capability_groups(&self) -> &[String] {
        &self.capability_groups
    }

    /// How the skill may be managed at runtime.
    pub fn manage_mode(&self) -> ManageMode {
        self.manage_mode
    }
}

/// Failure reading or parsing a skill.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SkillError {
    /// Underlying filesystem failure (carries the `FsError` display).
    #[error("skill filesystem error: {0}")]
    Fs(String),
    /// `SKILL.md` front-matter is absent or not valid JSON.
    #[error("skill '{0}' has malformed front-matter: {1}")]
    Malformed(String, String),
    /// No skill with the given id is registered.
    #[error("skill not found: {0}")]
    NotFound(String),
}

/// The JSON shape of a `SKILL.md` front-matter block.
#[derive(Deserialize)]
struct FrontMatter {
    #[serde(default)]
    description: String,
    #[serde(default)]
    metadata: FrontMatterMetadata,
}

#[derive(Default, Deserialize)]
struct FrontMatterMetadata {
    #[serde(default, rename = "cap_groups")]
    capability_groups: Vec<String>,
    #[serde(default)]
    manage_mode: ManageMode,
}

/// Parse the front-matter block at the head of a `SKILL.md` into a [`SkillMetadata`].
///
/// `head` must begin with the file's first bytes (a bounded prefix is enough —
/// the metadata lives between the first two `---` fences). The `id` comes from
/// the skill directory name, not the file.
pub(crate) fn parse_front_matter(id: SkillId, head: &str) -> Result<SkillMetadata, SkillError> {
    let json = front_matter_json(id.as_str(), head)?;
    let front_matter: FrontMatter = serde_json::from_str(json.trim())
        .map_err(|error| SkillError::Malformed(id.0.clone(), error.to_string()))?;
    Ok(SkillMetadata {
        id,
        description: front_matter.description,
        capability_groups: front_matter.metadata.capability_groups,
        manage_mode: front_matter.metadata.manage_mode,
    })
}

/// The JSON text between the opening and closing `---` fences.
fn front_matter_json<'a>(id: &str, text: &'a str) -> Result<&'a str, SkillError> {
    let after_open = text
        .trim_start()
        .strip_prefix("---")
        .ok_or_else(|| SkillError::Malformed(id.to_string(), "missing opening '---' fence".into()))?;
    let close = after_open.find("\n---").ok_or_else(|| {
        SkillError::Malformed(id.to_string(), "missing closing '---' fence".into())
    })?;
    Ok(after_open.get(..close).unwrap_or(""))
}

/// Return the markdown body of a `SKILL.md` — everything after the closing fence.
pub(crate) fn strip_front_matter<'a>(id: &str, text: &'a str) -> Result<&'a str, SkillError> {
    let after_open = text
        .trim_start()
        .strip_prefix("---")
        .ok_or_else(|| SkillError::Malformed(id.to_string(), "missing opening '---' fence".into()))?;
    let close = after_open.find("\n---").ok_or_else(|| {
        SkillError::Malformed(id.to_string(), "missing closing '---' fence".into())
    })?;
    // From the closing fence, skip the rest of that fence line, then the body.
    let from_fence = after_open.get(close..).unwrap_or("");
    let body = from_fence
        .get(1..) // drop the leading '\n' so the fence line starts the slice
        .and_then(|line| line.find('\n').and_then(|newline_index| line.get(newline_index.saturating_add(1)..)))
        .unwrap_or("");
    Ok(body)
}

// Parser error/edge paths only — degenerate inputs, no skill fixtures. Real
// `SKILL.md` parsing is covered data-driven in `tests/skills.rs` over the
// fixtures in `tests/data/skills/`.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn missing_opening_fence_is_malformed() {
        let error = parse_front_matter(SkillId::new("x"), "no front matter").unwrap_err();
        assert!(matches!(error, SkillError::Malformed(_, _)));
    }

    #[test]
    fn missing_close_fence_is_malformed() {
        let error = parse_front_matter(SkillId::new("x"), "---\n{}\n").unwrap_err();
        assert!(matches!(error, SkillError::Malformed(_, _)));
    }

    #[test]
    fn defaults_when_metadata_absent() {
        let metadata =
            parse_front_matter(SkillId::new("x"), "---\n{\"description\":\"d\"}\n---\nbody").unwrap();
        assert!(metadata.capability_groups().is_empty());
        assert_eq!(metadata.manage_mode(), ManageMode::ReadOnly);
    }
}
