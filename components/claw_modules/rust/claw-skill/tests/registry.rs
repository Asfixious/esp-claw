//! Registry + skill-set tests over a hermetic in-memory `ClawFs` (`MemFs`).
//!
//! No on-disk fixtures: each test builds the exact skill tree it needs, so the
//! decided behaviours are pinned in one place — unique ids across roots, no
//! `{CUR_SKILL_DIR}` expansion, and the two borrowed prompt fragments.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use claw_interface::{ClawFs, MemFs};
use claw_skill::{FsSkillRegistry, SkillError, SkillId, SkillRegistry, SkillSet};

/// A `SKILL.md` with the given description and body.
fn skill_md(description: &str, body: &str) -> Vec<u8> {
    format!("---\n{{\"description\":\"{description}\"}}\n---\n{body}").into_bytes()
}

/// A `MemFs` with two skills (`alpha`, `beta`) under the `skills` root.
fn two_skill_fs() -> Arc<MemFs> {
    let fs = MemFs::new();
    fs.write_atomic(
        "skills/alpha/SKILL.md",
        &skill_md(
            "Alpha skill",
            "# Alpha\nRun {CUR_SKILL_DIR}/scripts/a.lua\n",
        ),
    )
    .unwrap();
    fs.write_atomic(
        "skills/beta/SKILL.md",
        &skill_md("Beta skill", "# Beta\nBeta body\n"),
    )
    .unwrap();
    Arc::new(fs)
}

#[test]
fn scan_builds_catalog_sorted_by_id() {
    let registry = FsSkillRegistry::scan(two_skill_fs(), "skills").unwrap();
    let ids: Vec<&str> = registry
        .catalog()
        .iter()
        .map(|metadata| metadata.id().as_str())
        .collect();
    assert_eq!(ids, ["alpha", "beta"]);
    assert_eq!(
        registry
            .metadata(&SkillId::new("alpha"))
            .unwrap()
            .description(),
        "Alpha skill"
    );
}

#[test]
fn document_strips_front_matter_and_keeps_placeholder_literal() {
    let registry = FsSkillRegistry::scan(two_skill_fs(), "skills").unwrap();
    let document = registry.document(&SkillId::new("alpha")).unwrap();
    assert!(!document.starts_with("---"), "front-matter not stripped");
    assert!(document.contains("# Alpha"), "body missing");
    // {CUR_SKILL_DIR} expansion was dropped: the placeholder stays verbatim.
    assert!(
        document.contains("{CUR_SKILL_DIR}/scripts/a.lua"),
        "placeholder must be left untouched"
    );
}

#[test]
fn duplicate_id_across_roots_is_an_error() {
    let fs = MemFs::new();
    fs.write_atomic("system/shared/SKILL.md", &skill_md("from system", "# S"))
        .unwrap();
    fs.write_atomic("data/shared/SKILL.md", &skill_md("from data", "# S"))
        .unwrap();
    let result = FsSkillRegistry::scan_roots(Arc::new(fs), ["system", "data"]);
    assert!(matches!(result, Err(SkillError::DuplicateId(id)) if id.as_str() == "shared"));
}

#[test]
fn document_of_unknown_skill_is_not_found() {
    let registry = FsSkillRegistry::scan(two_skill_fs(), "skills").unwrap();
    let error = registry.document(&SkillId::new("missing")).unwrap_err();
    assert!(matches!(error, SkillError::NotFound(_)));
}

#[test]
fn skill_set_catalog_lists_every_available_skill() {
    let registry: Arc<dyn SkillRegistry> =
        Arc::new(FsSkillRegistry::scan(two_skill_fs(), "skills").unwrap());
    let mut set = SkillSet::new(registry);
    let catalog = set.catalog().to_string();
    assert!(catalog.starts_with("Available skills:\n"));
    assert!(catalog.contains("- alpha: Alpha skill"));
    assert!(catalog.contains("- beta: Beta skill"));
}

#[test]
fn skill_set_context_loads_unloads_and_caches() {
    let registry: Arc<dyn SkillRegistry> =
        Arc::new(FsSkillRegistry::scan(two_skill_fs(), "skills").unwrap());
    let mut set = SkillSet::new(registry);
    assert!(set.context().unwrap().is_empty());

    set.load("test", SkillId::new("alpha")).unwrap();
    let loaded = set.context().unwrap().to_string();
    assert!(loaded.contains("## Skill: alpha (test)"));
    assert!(loaded.contains("# Alpha"));
    // Re-read with nothing changed returns the same cached content.
    assert_eq!(set.context().unwrap(), loaded);

    set.unload(&SkillId::new("alpha"));
    assert!(set.context().unwrap().is_empty());
}

#[test]
fn loading_unknown_skill_is_not_found() {
    let registry: Arc<dyn SkillRegistry> =
        Arc::new(FsSkillRegistry::scan(two_skill_fs(), "skills").unwrap());
    let mut set = SkillSet::new(registry);
    assert!(set.load("test", SkillId::new("does_not_exist")).is_err());
}
