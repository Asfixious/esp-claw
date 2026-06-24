//! The **agent-manifest** generator.
//!
//! Self-contained codegen step: reads `resources/agents/<kind>/`, parses +
//! validates each kind, and writes the typed `MANIFESTS: &[AgentManifest]` array
//! to `<out_dir>/manifests.rs`.
//!
//! The whole step is sealed behind the single entry point [`generate`]; `main`
//! only calls it. Other generators (if added) live in their own sibling modules
//! with the same shape, so each stays isolated and `main` stays a thin wiring
//! layer.

use std::error::Error;
use std::fs;
use std::path::Path;

use crate::codegen;
use crate::parse::{
    parse_common, parse_kind, CommonBase, ParsedManifest, COMMON_FILES, MANIFEST_FILES,
};

/// The generated file's name within `OUT_DIR`.
const OUTPUT_FILE: &str = "manifests.rs";

/// Reserved directory under `resources/agents/` for data shared across kinds
/// (e.g. a shared instruction preamble). It is not an agent kind, so the
/// generator skips it rather than parsing it as a manifest.
const SHARED_DIR: &str = "common";

/// Generate the agent-manifest statics.
///
/// Reads `<manifest_dir>/resources/agents`, parses and validates every kind
/// directory, and writes `<out_dir>/manifests.rs`. Registers the resources it
/// reads for `rerun-if-changed` so edits re-trigger codegen.
///
/// # Errors
///
/// Returns an error if the resources directory cannot be read, a manifest is
/// malformed/invalid (via [`parse_kind`]), no kinds are found, or the output
/// file cannot be written — any of which fails the build.
pub fn generate(manifest_dir: &Path, out_dir: &Path) -> Result<(), Box<dyn Error>> {
    let agents_dir = manifest_dir.join("resources/agents");
    // Re-run when a kind is added or removed.
    println!("cargo:rerun-if-changed={}", agents_dir.display());

    // The shared base every kind inherits. Tracked for rerun (including
    // agent.json, so adding one re-triggers the build and fails it).
    let common_dir = agents_dir.join(SHARED_DIR);
    for file in COMMON_FILES {
        println!("cargo:rerun-if-changed={}", common_dir.join(file).display());
    }
    let common = parse_common(&common_dir)?;

    let mut kinds = collect_kinds(&agents_dir)?;
    // Every kind inherits the common base: its own entries extend the base.
    for kind in &mut kinds {
        inherit_base(kind, &common);
    }
    // Deterministic output regardless of directory iteration order.
    kinds.sort_by(|left, right| left.kind.cmp(&right.kind));

    if kinds.is_empty() {
        return Err(format!("no agent kinds found under {}", agents_dir.display()).into());
    }

    let generated = codegen::render(&kinds);
    let out_path = out_dir.join(OUTPUT_FILE);
    fs::write(&out_path, generated)
        .map_err(|error| format!("writing {}: {error}", out_path.display()))?;

    Ok(())
}

/// Parse every kind subdirectory under `agents_dir`, registering each manifest
/// file for `rerun-if-changed`. Hidden directories and the reserved shared-data
/// folder are skipped; only proper kind directories carry a manifest.
fn collect_kinds(agents_dir: &Path) -> Result<Vec<ParsedManifest>, Box<dyn Error>> {
    let mut kinds = Vec::new();
    for entry in fs::read_dir(agents_dir)
        .map_err(|error| format!("reading {}: {error}", agents_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skip = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') || name == SHARED_DIR);
        if skip {
            continue;
        }

        for file in MANIFEST_FILES {
            println!("cargo:rerun-if-changed={}", path.join(file).display());
        }
        kinds.push(parse_kind(&path)?);
    }
    Ok(kinds)
}

/// Fold the shared `common` base into one kind: the base entries come first,
/// then the kind's own, with duplicates dropped so a kind can list a capability
/// or skill already in the base without it appearing twice.
fn inherit_base(kind: &mut ParsedManifest, common: &CommonBase) {
    kind.capabilities = merge_unique(&common.capabilities, &kind.capabilities);
    kind.skills = merge_unique(&common.skills, &kind.skills);
}

/// Concatenate `base` then `own`, preserving first-seen order and dropping later
/// duplicates.
fn merge_unique(base: &[String], own: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(base.len() + own.len());
    for name in base.iter().chain(own) {
        if !merged.iter().any(|existing| existing == name) {
            merged.push(name.clone());
        }
    }
    merged
}
