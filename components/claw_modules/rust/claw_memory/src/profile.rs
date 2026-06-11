//! Editable profile memory provider, port of `claw_memory_profile.c`.

use crate::consts::{IDENTITY_FILE, MAX_PATH, SOUL_FILE, USER_FILE};
use crate::error::{MemoryError, MemoryResult};
use crate::state::{self, MemoryState};
use crate::util;

const DEFAULT_SOUL: &str =
    "# Soul\n\nDescribe who the agent is, what it values, and how it prefers to help.\n";
const DEFAULT_IDENTITY: &str =
    "# Identity Card\n\n- Name:\n- Role:\n- Personality:\n- Strengths:\n";
const DEFAULT_USER: &str =
    "# User\n\nWrite down who the current user is, their preferences, and any standing guidance.\n";

/// `claw_memory_profile_init_defaults`.
pub fn profile_init_defaults(st: &mut MemoryState) -> MemoryResult<()> {
    if st.memory_root_dir.is_empty() {
        return Err(MemoryError::InvalidState);
    }

    let root = st.memory_root_dir.clone();
    st.soul_path = match util::join_path(&root, SOUL_FILE) {
        Ok(p) if p.len() < MAX_PATH => p,
        _ => return Err(MemoryError::InvalidSize),
    };
    let Ok(identity_path) = util::join_path(&root, IDENTITY_FILE) else {
        return Err(MemoryError::InvalidSize);
    };
    st.identity_path = identity_path;
    let Ok(user_path) = util::join_path(&root, USER_FILE) else {
        return Err(MemoryError::InvalidSize);
    };
    st.user_path = user_path;

    if util::ensure_file_with_default(&st.soul_path, DEFAULT_SOUL).is_err()
        || util::ensure_file_with_default(&st.identity_path, DEFAULT_IDENTITY).is_err()
        || util::ensure_file_with_default(&st.user_path, DEFAULT_USER).is_err()
    {
        return Err(MemoryError::Fail);
    }

    Ok(())
}

fn dup_nonempty(path: &str) -> MemoryResult<String> {
    let text = util::read_file_dup(path)?;
    let trimmed = util::trim_whitespace(&text);
    if trimmed.is_empty() {
        return Err(MemoryError::NotFound);
    }
    Ok(trimmed.to_string())
}

/// `claw_memory_profile_collect`.
pub fn profile_collect_content() -> MemoryResult<String> {
    let st = state::lock();
    if !st.initialized {
        return Err(MemoryError::InvalidArg);
    }

    let entries: [(&str, &str); 3] = [
        ("User Profile", st.user_path.as_str()),
        ("Agent Soul", st.soul_path.as_str()),
        ("Agent Identity Card", st.identity_path.as_str()),
    ];

    let mut content = String::new();
    let mut any = false;
    for (title, path) in entries.iter() {
        if let Ok(part) = dup_nonempty(path) {
            any = true;
            content.push_str(&format!("{} ({}):\n{}\n\n", title, path, part));
        }
    }

    if !any {
        return Err(MemoryError::NotFound);
    }
    Ok(content)
}
