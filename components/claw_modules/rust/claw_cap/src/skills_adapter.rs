//! [`Skills`] port adapters.

use claw_core::{SkillPrompt, Skills};

/// No skill context (host tests without skill registry).
pub struct EmptySkills;

impl Skills for EmptySkills {
    fn prompt(&self) -> SkillPrompt {
        SkillPrompt::default()
    }
}

/// Fixed prompt for host tests.
pub struct StaticSkills {
    pub prompt: SkillPrompt,
}

impl Skills for StaticSkills {
    fn prompt(&self) -> SkillPrompt {
        self.prompt.clone()
    }
}
