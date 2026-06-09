//! Editable profile memory provider, port of `claw_memory_profile.c`.

use claw_interfaces::error::{EspErr, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND, ESP_FAIL, ESP_OK};

use crate::consts::{IDENTITY_FILE, MAX_PATH, SOUL_FILE, USER_FILE};
use crate::state::{self, MemoryState};
use crate::util;

const DEFAULT_SOUL: &str =
    "# Soul\n\nDescribe who the agent is, what it values, and how it prefers to help.\n";
const DEFAULT_IDENTITY: &str =
    "# Identity Card\n\n- Name:\n- Role:\n- Personality:\n- Strengths:\n";
const DEFAULT_USER: &str =
    "# User\n\nWrite down who the current user is, their preferences, and any standing guidance.\n";

/// `claw_memory_profile_init_defaults`.
pub fn profile_init_defaults(st: &mut MemoryState) -> EspErr {
    if st.memory_root_dir.is_empty() {
        return ESP_ERR_INVALID_STATE;
    }

    let root = st.memory_root_dir.clone();
    st.soul_path = match util::join_path(&root, SOUL_FILE) {
        Ok(p) if p.len() < MAX_PATH => p,
        Ok(_) => return claw_interfaces::error::ESP_ERR_INVALID_SIZE,
        Err(_) => return claw_interfaces::error::ESP_ERR_INVALID_SIZE,
    };
    st.identity_path = match util::join_path(&root, IDENTITY_FILE) {
        Ok(p) => p,
        Err(_) => return claw_interfaces::error::ESP_ERR_INVALID_SIZE,
    };
    st.user_path = match util::join_path(&root, USER_FILE) {
        Ok(p) => p,
        Err(_) => return claw_interfaces::error::ESP_ERR_INVALID_SIZE,
    };

    if util::ensure_file_with_default(&st.soul_path, DEFAULT_SOUL) != ESP_OK
        || util::ensure_file_with_default(&st.identity_path, DEFAULT_IDENTITY) != ESP_OK
        || util::ensure_file_with_default(&st.user_path, DEFAULT_USER) != ESP_OK
    {
        return ESP_FAIL;
    }

    ESP_OK
}

fn dup_nonempty(path: &str) -> Result<String, EspErr> {
    let text = util::read_file_dup(path)?;
    let trimmed = util::trim_whitespace(&text);
    if trimmed.is_empty() {
        return Err(ESP_ERR_NOT_FOUND);
    }
    Ok(trimmed.to_string())
}

/// `claw_memory_profile_collect`.
pub fn profile_collect_content() -> Result<String, EspErr> {
    let st = state::lock();
    if !st.initialized {
        return Err(claw_interfaces::error::ESP_ERR_INVALID_ARG);
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
        return Err(ESP_ERR_NOT_FOUND);
    }
    Ok(content)
}
