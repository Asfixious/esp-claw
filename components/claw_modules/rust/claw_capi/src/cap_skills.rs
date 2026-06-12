//! C `claw_skill_*` APIs → [`Skills`] port (`CapSkills`).

use claw_core::{SkillPrompt, Skills, TurnContext};

#[cfg(target_os = "espidf")]
use claw_core::ActiveSkillDoc;

/// Skill prompt assembly from the C skill registry.
pub struct CLegacySkills;

impl Skills for CLegacySkills {
    fn prompt(&self, ctx: &TurnContext) -> SkillPrompt {
        #[cfg(not(target_os = "espidf"))]
        {
            let _ = ctx;
            return SkillPrompt::default();
        }

        #[cfg(target_os = "espidf")]
        {
            const SKILLS_LIST_BUF: usize = 8 * 1024;
            const DOC_BUF: usize = 32 * 1024;

            extern "C" {
                fn claw_skill_read_skills_list(buf: *mut libc::c_char, size: usize) -> i32;
                fn claw_skill_load_active_skill_ids(
                    session_id: *const libc::c_char,
                    out_skill_ids: *mut *mut *mut libc::c_char,
                    out_skill_count: *mut usize,
                ) -> i32;
                fn claw_skill_read_document(
                    skill_id: *const libc::c_char,
                    buf: *mut libc::c_char,
                    size: usize,
                ) -> i32;
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

            let mut active = Vec::new();
            if let Some(session_id) = ctx.session_id.as_deref().filter(|s| !s.is_empty()) {
                let session_c = std::ffi::CString::new(session_id).unwrap_or_default();
                let mut ids: *mut *mut libc::c_char = std::ptr::null_mut();
                let mut count: usize = 0;
                let load_err = unsafe {
                    claw_skill_load_active_skill_ids(session_c.as_ptr(), &mut ids, &mut count)
                };
                if load_err == 0 && !ids.is_null() && count > 0 {
                    for i in 0..count {
                        let id_ptr = unsafe { *ids.add(i) };
                        if id_ptr.is_null() {
                            continue;
                        }
                        let id = unsafe {
                            std::ffi::CStr::from_ptr(id_ptr)
                                .to_string_lossy()
                                .into_owned()
                        };
                        let mut doc_buf = vec![0u8; DOC_BUF];
                        let id_c = std::ffi::CString::new(id.as_str()).unwrap_or_default();
                        let doc_err = unsafe {
                            claw_skill_read_document(
                                id_c.as_ptr(),
                                doc_buf.as_mut_ptr() as *mut libc::c_char,
                                doc_buf.len(),
                            )
                        };
                        let markdown = if doc_err == 0 {
                            let nul =
                                doc_buf.iter().position(|&b| b == 0).unwrap_or(doc_buf.len());
                            String::from_utf8_lossy(&doc_buf[..nul]).into_owned()
                        } else {
                            String::new()
                        };
                        active.push(ActiveSkillDoc { id, markdown });
                    }
                    unsafe {
                        for i in 0..count {
                            let id_ptr = *ids.add(i);
                            if !id_ptr.is_null() {
                                libc::free(id_ptr as *mut libc::c_void);
                            }
                        }
                        libc::free(ids as *mut libc::c_void);
                    }
                }
            }

            SkillPrompt {
                catalog_text,
                catalog: Vec::<SkillCatalogEntry>::new(),
                active,
            }
        }
    }
}
