//! C `claw_skill_*` APIs → [`Skills`] port (`CapSkills`).

use claw_core::{SkillPrompt, Skills};

#[cfg(target_os = "espidf")]
use claw_core::SkillCatalogEntry;

/// Skill prompt assembly from the C skill registry.
///
/// Loads the skill catalog text from the firmware registry. Per-session active
/// skill documents are no longer assembled here — that depended on the removed
/// turn/session routing context and will be re-supplied by the context builder.
pub struct CLegacySkills;

impl Skills for CLegacySkills {
    fn prompt(&self) -> SkillPrompt {
        #[cfg(not(target_os = "espidf"))]
        {
            SkillPrompt::default()
        }

        #[cfg(target_os = "espidf")]
        {
            const SKILLS_LIST_BUF: usize = 8 * 1024;

            extern "C" {
                fn claw_skill_read_skills_list(buf: *mut libc::c_char, size: usize) -> i32;
            }

            let mut catalog_text = String::new();
            let mut catalog_buf = vec![0u8; SKILLS_LIST_BUF];
            let err = unsafe {
                claw_skill_read_skills_list(
                    catalog_buf.as_mut_ptr() as *mut libc::c_char,
                    catalog_buf.len(),
                )
            };
            if err == 0 {
                let nul = catalog_buf.iter().position(|&b| b == 0).unwrap_or(catalog_buf.len());
                catalog_text = String::from_utf8_lossy(&catalog_buf[..nul]).into_owned();
            }

            SkillPrompt {
                catalog_text,
                catalog: Vec::<SkillCatalogEntry>::new(),
                active: Vec::new(),
            }
        }
    }
}
