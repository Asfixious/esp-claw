//! Skill registry: the catalog source the agent's [`SkillSet`] reads from.
//!
//! [`FsSkillRegistry`] scans one or more skills roots over an injected
//! [`ClawFs`]: one directory per skill, each holding a `SKILL.md`. The catalog
//! (metadata) is built once by reading only each file's front-matter head; full
//! documents are read lazily, on demand, when a skill is placed in context.
//!
//! Skill ids are unique across every scanned root — the same id in two roots is
//! a hard [`SkillError::DuplicateId`], not a silent override.
//!
//! [`SkillSet`]: crate::SkillSet

use std::collections::HashMap;
use std::sync::Arc;

use claw_interface::ClawFs;

use super::skill::{parse_front_matter, strip_front_matter, SkillError, SkillId, SkillMetadata};

/// Maximum bytes read from a `SKILL.md` head to parse its front-matter.
///
/// Real metadata blocks are a few hundred bytes; this bound keeps the catalog
/// scan from reading large document bodies. A header that does not close within
/// this window is treated as malformed.
const METADATA_PREFIX_BYTES: u64 = 2048;

/// The catalog + document source the agent's [`SkillSet`] reads from.
///
/// An inbound port: the catalog (cheap metadata) is held in memory, while full
/// documents are fetched on demand. [`FsSkillRegistry`] is the filesystem-backed
/// implementation; tests can substitute a fake.
///
/// [`SkillSet`]: crate::SkillSet
pub trait SkillRegistry: Send + Sync {
    /// The full catalog, borrowed. Cheap, in-memory metadata only.
    fn catalog(&self) -> &[SkillMetadata];

    /// Read one skill's full document body (front-matter stripped).
    ///
    /// # Errors
    ///
    /// - [`SkillError::NotFound`] when no skill has `id`.
    /// - [`SkillError::ReadFailed`] / [`SkillError::InvalidUtf8`] if the file
    ///   cannot be read or decoded.
    /// - [`SkillError::MissingOpeningFence`] / [`SkillError::MissingClosingFence`]
    ///   if the document's front-matter is malformed.
    fn document(&self, id: &SkillId) -> Result<String, SkillError>;

    /// Look up one catalog entry by id, or `None` if there is no such skill.
    fn metadata(&self, id: &SkillId) -> Option<&SkillMetadata> {
        self.catalog().iter().find(|entry| entry.id() == id)
    }
}

/// A [`SkillRegistry`] backed by one or more [`ClawFs`] skills directories.
pub struct FsSkillRegistry {
    fs: Arc<dyn ClawFs>,
    roots: Vec<String>,
    catalog: Vec<SkillMetadata>,
    /// The root each skill id was found under, so [`document`](Self::document)
    /// can rebuild its path.
    root_by_id: HashMap<SkillId, String>,
}

impl FsSkillRegistry {
    /// Scan a single `root` and build the catalog.
    ///
    /// Convenience for the common single-root case; see [`scan_roots`](Self::scan_roots).
    ///
    /// # Errors
    ///
    /// See [`scan_roots`](Self::scan_roots).
    pub fn scan(fs: Arc<dyn ClawFs>, root: impl Into<String>) -> Result<Self, SkillError> {
        Self::scan_roots(fs, [root.into()])
    }

    /// Scan every `root` in order and build the merged catalog.
    ///
    /// Each immediate subdirectory holding a `SKILL.md` becomes a catalog entry
    /// (id = directory name). The catalog is sorted by id for stable output. A
    /// directory whose `SKILL.md` is missing is skipped.
    ///
    /// # Errors
    ///
    /// - [`SkillError::ScanFailed`] if a root directory cannot be listed.
    /// - [`SkillError::DuplicateId`] if two skills (in any root) share an id.
    /// - [`SkillError::ReadFailed`] / [`SkillError::InvalidUtf8`] if a skill's
    ///   head cannot be read or decoded.
    /// - [`SkillError::MissingOpeningFence`] / [`SkillError::MissingClosingFence`]
    ///   / [`SkillError::InvalidJson`] if a skill's front-matter is malformed.
    pub fn scan_roots(
        fs: Arc<dyn ClawFs>,
        roots: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, SkillError> {
        let roots: Vec<String> = roots.into_iter().map(Into::into).collect();
        let (catalog, root_by_id) = Self::scan_catalog(fs.as_ref(), &roots)?;
        Ok(Self {
            fs,
            roots,
            catalog,
            root_by_id,
        })
    }

    /// Re-scan every root, replacing the catalog (e.g. after skills are added or
    /// removed on disk).
    ///
    /// # Errors
    ///
    /// Same as [`scan_roots`](Self::scan_roots).
    pub fn reload(&mut self) -> Result<(), SkillError> {
        let (catalog, root_by_id) = Self::scan_catalog(self.fs.as_ref(), &self.roots)?;
        self.catalog = catalog;
        self.root_by_id = root_by_id;
        Ok(())
    }

    /// Build the catalog and id→root map by scanning every root once.
    fn scan_catalog(
        fs: &dyn ClawFs,
        roots: &[String],
    ) -> Result<(Vec<SkillMetadata>, HashMap<SkillId, String>), SkillError> {
        let mut catalog = Vec::new();
        let mut root_by_id: HashMap<SkillId, String> = HashMap::new();
        for root in roots {
            let names = fs
                .list_dir(root)
                .map_err(|error| SkillError::ScanFailed(root.clone(), error))?;
            for name in names {
                let id = SkillId::new(name);
                let path = skill_document_path(root, id.as_str());
                if !fs.exists(&path) {
                    continue;
                }
                if root_by_id.contains_key(&id) {
                    return Err(SkillError::DuplicateId(id));
                }
                let head = read_head(fs, &id, &path)?;
                let metadata = parse_front_matter(id.clone(), &head)?;
                root_by_id.insert(id, root.clone());
                catalog.push(metadata);
            }
        }
        catalog.sort_by(|a, b| a.id().cmp(b.id()));
        Ok((catalog, root_by_id))
    }
}

impl SkillRegistry for FsSkillRegistry {
    fn catalog(&self) -> &[SkillMetadata] {
        &self.catalog
    }

    fn document(&self, id: &SkillId) -> Result<String, SkillError> {
        let root = self
            .root_by_id
            .get(id)
            .ok_or_else(|| SkillError::NotFound(id.clone()))?;
        let path = skill_document_path(root, id.as_str());
        let bytes = self
            .fs
            .read(&path)
            .map_err(|error| SkillError::ReadFailed(id.clone(), error))?;
        let text = String::from_utf8(bytes).map_err(|_| SkillError::InvalidUtf8(id.clone()))?;
        Ok(strip_front_matter(id, &text)?.to_string())
    }
}

fn skill_document_path(root: &str, id: &str) -> String {
    format!("{}/{}/SKILL.md", root.trim_end_matches('/'), id)
}

/// Read just the front-matter head of a `SKILL.md`.
///
/// Clamps the read to the file size so small files don't trip `read_at`'s
/// past-EOF error, and to [`METADATA_PREFIX_BYTES`] so large bodies aren't read.
fn read_head(fs: &dyn ClawFs, id: &SkillId, path: &str) -> Result<String, SkillError> {
    let read_failed = |error| SkillError::ReadFailed(id.clone(), error);
    let size = fs.len(path).map_err(read_failed)?;
    let take = usize::try_from(size.min(METADATA_PREFIX_BYTES)).unwrap_or(usize::MAX);
    let bytes = fs.read_at(path, 0, take).map_err(read_failed)?;
    String::from_utf8(bytes).map_err(|_| SkillError::InvalidUtf8(id.clone()))
}
