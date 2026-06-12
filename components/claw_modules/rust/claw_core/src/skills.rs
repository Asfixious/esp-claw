//! Skill port: prompt sections merged into orchestrator context.

use thiserror::Error;

use crate::tools::TurnContext;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCatalogEntry {
    pub id: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSkillDoc {
    pub id: String,
    pub markdown: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillPrompt {
    pub catalog_text: String,
    pub catalog: Vec<SkillCatalogEntry>,
    pub active: Vec<ActiveSkillDoc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSlot {
    SystemPrompt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSection {
    pub slot: PromptSlot,
    pub content: String,
}

impl SkillPrompt {
    pub fn into_sections(self) -> Vec<PromptSection> {
        let mut sections = Vec::new();
        if !self.catalog_text.is_empty() {
            sections.push(PromptSection {
                slot: PromptSlot::SystemPrompt,
                content: self.catalog_text,
            });
        }
        for doc in self.active {
            if doc.markdown.is_empty() {
                continue;
            }
            sections.push(PromptSection {
                slot: PromptSlot::SystemPrompt,
                content: format!("## Skill: {}\n\n{}", doc.id, doc.markdown),
            });
        }
        sections
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SkillError {
    #[error("skill error: {0}")]
    Other(String),
}

pub trait Skills: Send + Sync {
    fn prompt(&self, ctx: &TurnContext) -> SkillPrompt;
}

/// No skill context (default orchestrator wiring).
pub struct NoSkills;

impl Skills for NoSkills {
    fn prompt(&self, _ctx: &TurnContext) -> SkillPrompt {
        SkillPrompt::default()
    }
}
