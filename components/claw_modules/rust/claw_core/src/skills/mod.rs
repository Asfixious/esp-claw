//! Skills: per-agent, dynamically loadable prompt context.
//!
//! - [`SkillRegistry`] / [`FsSkillRegistry`] — the catalog source: scans a skills
//!   directory, reads each `SKILL.md`'s front-matter for the catalog, and reads
//!   full documents on demand.
//! - [`Skill`] domain types — [`SkillId`], [`SkillMetadata`], [`ManageMode`].
//! - [`SkillSet`] / [`SkillGroup`] — the agent's loaded skills and their
//!   dirty-cached, assembled context. Mutable at runtime (load/unload without
//!   restarting the agent), parallel to [`ToolSet`](crate::ToolSet).

mod registry;
mod skill;
mod skill_set;

pub use registry::{FsSkillRegistry, SkillRegistry};
pub use skill::{ManageMode, SkillError, SkillId, SkillMetadata};
pub use skill_set::{SkillGroup, SkillSet};
