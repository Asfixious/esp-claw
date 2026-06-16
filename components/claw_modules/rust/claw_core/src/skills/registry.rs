//! Skill registry: the catalog source the agent's [`SkillSet`] reads from.
//!
//! [`FsSkillRegistry`] scans a skills root over an injected [`ClawFs`]: one
//! directory per skill, each holding a `SKILL.md`. The catalog (metadata) is
//! built once by reading only each file's front-matter head; full documents are
//! read lazily, on demand, when a skill is actually placed in context.
//!
//! [`SkillSet`]: crate::skills::SkillSet

use std::sync::Arc;

use claw_interfaces::ClawFs;

use super::skill::{parse_front_matter, strip_front_matter, SkillError, SkillId, SkillMetadata};

/// Maximum bytes read from a `SKILL.md` head to parse its front-matter.
///
/// Real metadata blocks are a few hundred bytes; this bound keeps the catalog
/// scan from reading large document bodies. A header that does not close within
/// this window is treated as malformed.
const METADATA_PREFIX_BYTES: u64 = 2048;

/// The catalog + document source for skills.
pub trait SkillRegistry: Send + Sync {
    /// The full catalog, borrowed. Cheap, in-memory metadata only.
    fn catalog(&self) -> &[SkillMetadata];

    /// Read one skill's full document body, `{CUR_SKILL_DIR}` expanded.
    ///
    /// Returns [`SkillError::NotFound`] when no skill has `id`.
    fn document(&self, id: &SkillId) -> Result<String, SkillError>;

    /// Look up one catalog entry by id.
    fn metadata(&self, id: &SkillId) -> Option<&SkillMetadata> {
        self.catalog().iter().find(|entry| entry.id() == id)
    }
}

/// A [`SkillRegistry`] backed by a [`ClawFs`] skills directory.
pub struct FsSkillRegistry {
    fs: Arc<dyn ClawFs>,
    root: String,
    catalog: Vec<SkillMetadata>,
}

impl FsSkillRegistry {
    /// Scan `root` and build the catalog.
    ///
    /// Each immediate subdirectory holding a `SKILL.md` becomes a catalog entry
    /// (id = directory name). Entries are sorted by id for stable catalog output.
    /// A directory whose `SKILL.md` is missing is skipped; one whose header is
    /// malformed fails the scan.
    pub fn scan(fs: Arc<dyn ClawFs>, root: impl Into<String>) -> Result<Self, SkillError> {
        let root = root.into();
        let mut catalog = Vec::new();
        for name in fs.list_dir(&root).map_err(|error| SkillError::Fs(error.to_string()))? {
            let path = skill_document_path(&root, &name);
            if !fs.exists(&path) {
                continue;
            }
            let head = read_head(fs.as_ref(), &path)?;
            catalog.push(parse_front_matter(SkillId::new(name), &head)?);
        }
        catalog.sort_by(|a, b| a.id().cmp(b.id()));
        Ok(Self { fs, root, catalog })
    }

    /// Re-scan the root, replacing the catalog (e.g. after skills change on disk).
    pub fn reload(&mut self) -> Result<(), SkillError> {
        let rescanned = Self::scan(Arc::clone(&self.fs), self.root.clone())?;
        self.catalog = rescanned.catalog;
        Ok(())
    }
}

impl SkillRegistry for FsSkillRegistry {
    fn catalog(&self) -> &[SkillMetadata] {
        &self.catalog
    }

    fn document(&self, id: &SkillId) -> Result<String, SkillError> {
        if self.metadata(id).is_none() {
            return Err(SkillError::NotFound(id.to_string()));
        }
        let path = skill_document_path(&self.root, id.as_str());
        let bytes = self
            .fs
            .read(&path)
            .map_err(|error| SkillError::Fs(error.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|error| SkillError::Fs(error.to_string()))?;
        let body = strip_front_matter(id.as_str(), &text)?;
        let current_skill_dir = format!("{}/{}", self.root.trim_end_matches('/'), id.as_str());
        Ok(body.replace("{CUR_SKILL_DIR}", &current_skill_dir))
    }
}

fn skill_document_path(root: &str, id: &str) -> String {
    format!("{}/{}/SKILL.md", root.trim_end_matches('/'), id)
}

/// Read just the front-matter head of a `SKILL.md`.
///
/// Clamps the read to the file size so small files don't trip `read_at`'s
/// past-EOF error, and to [`METADATA_PREFIX_BYTES`] so large bodies aren't read.
fn read_head(fs: &dyn ClawFs, path: &str) -> Result<String, SkillError> {
    let size = fs.len(path).map_err(|error| SkillError::Fs(error.to_string()))?;
    let take = usize::try_from(size.min(METADATA_PREFIX_BYTES)).unwrap_or(usize::MAX);
    let bytes = fs
        .read_at(path, 0, take)
        .map_err(|error| SkillError::Fs(error.to_string()))?;
    String::from_utf8(bytes).map_err(|error| SkillError::Fs(error.to_string()))
}
