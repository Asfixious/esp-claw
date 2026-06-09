//! Public CRUD operations (`claw_memory.c`) plus init and the long-term memory
//! provider. The single global state lock is acquired at each public entry
//! point; the `*_locked` helpers operate on `&mut MemoryState` so re-entrant
//! flows (update calling store + forget) never re-lock.

use serde_json::Value;

use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_SIZE, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND,
    ESP_FAIL, ESP_OK,
};

use crate::consts::{
    BackendFormat, MessageIntent, DEFAULT_MAX_MESSAGE_CHARS, DEFAULT_MAX_TOOL_ITERATIONS, MAX_PATH,
    MAX_SUMMARIES, RECALL_DEFAULT_LIMIT,
};
use crate::item::{self, MemItem};
use crate::state::{self, MemoryState};
use crate::storage;
use crate::util;

const DEFAULT_INDEX: &str =
    "{\"version\":3,\"next_summary_id\":1,\"last_compact_digest_size\":0,\"summaries\":[],\"keyword_index\":{}}\n";
const DEFAULT_MARKDOWN: &str =
    "# Long-term Memory\n\n(empty - ESP-Claw will write memories here as it learns)\n";

// --- config (mirror claw_memory_config_t / claw_memory_llm_config_t) ------

// Most LLM fields are only read by the espidf async-extract runtime builder.
#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub struct LlmConfig {
    pub api_key: Option<String>,
    pub backend_type: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub auth_type: Option<String>,
    pub max_tokens_field: Option<String>,
    pub timeout_ms: u32,
    pub max_tokens: u32,
    pub image_max_bytes: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub image_remote_url_only: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryConfig {
    pub session_root_dir: Option<String>,
    pub memory_root_dir: Option<String>,
    pub max_message_chars: usize,
    pub max_tool_iterations: u32,
    pub llm: LlmConfig,
    pub enable_async_extract_stage_note: bool,
}

// --- init -----------------------------------------------------------------

/// `claw_memory_init`.
pub fn init(config: &MemoryConfig) -> EspErr {
    let session_root = match &config.session_root_dir {
        Some(s) => s.clone(),
        None => return ESP_ERR_INVALID_ARG,
    };
    let memory_root = match &config.memory_root_dir {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return ESP_ERR_INVALID_ARG,
    };

    {
        let mut st = state::lock();
        *st = MemoryState::default();
        st.session_root_dir = util::safe_copy(&session_root, MAX_PATH);
        st.memory_root_dir = util::safe_copy(&memory_root, MAX_PATH);
        st.max_message_chars = if config.max_message_chars != 0 {
            config.max_message_chars
        } else {
            DEFAULT_MAX_MESSAGE_CHARS
        };
        st.max_tool_iterations = if config.max_tool_iterations != 0 {
            config.max_tool_iterations
        } else {
            DEFAULT_MAX_TOOL_ITERATIONS
        };
        st.backend_format = BackendFormat::from_type(config.llm.backend_type.as_deref());
        st.next_memory_seq = util::now_sec() % 10000;

        let md = st.memory_root_dir.clone();
        st.markdown_path = match util::join_path(&md, crate::consts::MARKDOWN_FILE) {
            Ok(p) => p,
            Err(_) => return ESP_ERR_INVALID_SIZE,
        };
        st.records_path = match util::join_path(&md, crate::consts::RECORDS_FILE) {
            Ok(p) => p,
            Err(_) => return ESP_ERR_INVALID_SIZE,
        };
        st.index_path = match util::join_path(&md, crate::consts::INDEX_FILE) {
            Ok(p) => p,
            Err(_) => return ESP_ERR_INVALID_SIZE,
        };
        st.digest_path = match util::join_path(&md, crate::consts::DIGEST_FILE) {
            Ok(p) => p,
            Err(_) => return ESP_ERR_INVALID_SIZE,
        };

        if util::ensure_dir_recursive(&st.session_root_dir) != ESP_OK
            || util::ensure_dir_recursive(&st.memory_root_dir) != ESP_OK
        {
            return ESP_FAIL;
        }

        if crate::profile::profile_init_defaults(&mut st) != ESP_OK {
            return ESP_FAIL;
        }

        if util::ensure_file_with_default(&st.records_path, "") != ESP_OK
            || util::ensure_file_with_default(&st.index_path, DEFAULT_INDEX) != ESP_OK
            || util::ensure_file_with_default(&st.digest_path, "") != ESP_OK
            || util::ensure_file_with_default(&st.markdown_path, DEFAULT_MARKDOWN) != ESP_OK
        {
            return ESP_FAIL;
        }

        st.initialized = true;
    }

    let async_err = crate::extract::async_extract_init(config);
    if async_err != ESP_OK {
        return async_err;
    }

    {
        let mut st = state::lock();
        let _ = storage::compact_internal(&mut st, false);
        log::info!("Initialized memory root={}", st.memory_root_dir);
    }
    ESP_OK
}

// --- store ----------------------------------------------------------------

fn fill_defaults(item: &mut MemItem) {
    if item.source.is_empty() {
        item.set_source("manual");
    }
}

/// `claw_memory_store_prepared_item_internal`.
fn store_prepared_item_internal(
    st: &mut MemoryState,
    item: &mut MemItem,
    digest_action: &str,
    digest_extra: Option<&str>,
    ignore_existing_id: Option<&str>,
) -> (EspErr, bool) {
    if item.content.is_empty() {
        return (ESP_ERR_INVALID_ARG, false);
    }

    item.content = util::trim_whitespace(&item.content).to_string();
    item.tags = util::trim_whitespace(&item.tags).to_string();
    item.keywords = util::trim_whitespace(&item.keywords).to_string();
    item::normalize_item_metadata(item);
    if item.content.is_empty() {
        return (ESP_ERR_INVALID_ARG, false);
    }

    fill_defaults(item);

    if item.id.is_empty() {
        item.id = item::make_id(&mut st.next_memory_seq);
    }
    if item.created_at == 0 {
        item.created_at = util::now_sec();
    }
    item.updated_at = util::now_sec();
    let item_key = item::build_item_key(item);

    let mut items = match storage::load_current_items(st) {
        Ok(v) => v,
        Err(e) => return (e, false),
    };

    for existing in items.iter() {
        if let Some(ignore) = ignore_existing_id {
            if !ignore.is_empty() && existing.id == ignore {
                continue;
            }
        }
        if !existing.deleted {
            let existing_key = item::build_item_key(existing);
            if !existing_key.is_empty() && existing_key == item_key {
                *item = existing.clone();
                return (ESP_OK, false);
            }
        }
        if item::items_semantically_match(existing, item) {
            *item = existing.clone();
            return (ESP_OK, false);
        }
    }

    let mut index_root = match storage::load_index(st) {
        Ok(v) => v,
        Err(e) => return (e, false),
    };

    let labels = item::collect_summary_labels(item);
    item.summary_ids.clear();
    for label in labels.iter().take(MAX_SUMMARIES) {
        match storage::ensure_summary_label(&mut index_root, label, 0) {
            Ok(id) => item.summary_ids.push(id as u16),
            Err(e) => return (e, false),
        }
    }

    let mut err = storage::append_record(st, item);
    let mut changed = false;
    if err == ESP_OK {
        storage::adjust_summary_stats(&mut index_root, item, 1);
        items.push(item.clone());
        storage::rebuild_keyword_index(&mut index_root, &items);
        err = storage::save_index(st, &index_root);
    }
    if err == ESP_OK {
        err = storage::sync_markdown(st, &items, &index_root);
    }
    if err == ESP_OK {
        storage::append_digest_line(st, digest_action, Some(item), digest_extra);
        st.write_changes_since_compact += 1;
        changed = true;
    }

    if err == ESP_OK {
        err = storage::maybe_compact(st);
    }
    (err, changed)
}

/// `claw_memory_store_with_result`.
pub fn store_with_result(item: &mut MemItem) -> (EspErr, bool) {
    let mut st = state::lock();
    if !st.initialized {
        return (ESP_ERR_INVALID_STATE, false);
    }
    store_prepared_item_internal(&mut st, item, "store", None, None)
}

// --- update ---------------------------------------------------------------

fn apply_update_patch(dst: &mut MemItem, patch: &MemItem) {
    if !patch.source.is_empty() {
        dst.set_source(&patch.source);
    }
    if !patch.content.is_empty() {
        dst.set_content(&patch.content);
    }
    if !patch.tags.is_empty() {
        dst.set_tags(&patch.tags);
    }
    if !patch.keywords.is_empty() {
        dst.set_keywords(&patch.keywords);
    }
}

/// `claw_memory_prepare_updated_replacement`.
fn prepare_updated_replacement(existing: &MemItem, patch: &MemItem) -> Result<MemItem, EspErr> {
    let mut replacement = existing.clone();
    replacement.id.clear();
    replacement.summary_ids.clear();
    replacement.deleted = false;
    replacement.access_count = 0;
    replacement.created_at = 0;
    replacement.updated_at = 0;

    apply_update_patch(&mut replacement, patch);
    replacement.content = util::trim_whitespace(&replacement.content).to_string();
    replacement.tags = util::trim_whitespace(&replacement.tags).to_string();
    replacement.keywords = util::trim_whitespace(&replacement.keywords).to_string();
    item::normalize_item_metadata(&mut replacement);
    fill_defaults(&mut replacement);
    if replacement.content.is_empty() {
        return Err(ESP_ERR_INVALID_ARG);
    }
    Ok(replacement)
}

fn update_with_result_locked(st: &mut MemoryState, item: &mut MemItem) -> (EspErr, bool) {
    if item.id.is_empty() {
        return (ESP_ERR_INVALID_ARG, false);
    }
    let orig_id = item.id.clone();

    let items = match storage::load_current_items(st) {
        Ok(v) => v,
        Err(e) => return (e, false),
    };
    let idx = match items.iter().position(|i| i.id == orig_id) {
        Some(i) if !items[i].deleted => i,
        _ => return (ESP_ERR_NOT_FOUND, false),
    };

    let existing = items[idx].clone();
    let prep = prepare_updated_replacement(&existing, item);
    if let Ok(ref replacement) = prep {
        if item::items_equivalent_for_update(&existing, replacement) {
            *item = existing;
            return (ESP_OK, false);
        }
    }
    let mut replacement = match prep {
        Ok(r) => r,
        Err(e) => return (e, false),
    };

    let (err, stored_changed) =
        store_prepared_item_internal(st, &mut replacement, "update_store", Some(&orig_id), Some(&orig_id));
    if err != ESP_OK {
        return (err, false);
    }

    let (err, forgotten, forgotten_changed) = forget_with_result_locked(st, &orig_id, true);
    if err != ESP_OK {
        return (err, false);
    }

    let forgotten_id = forgotten.map(|f| f.id).unwrap_or_default();
    storage::append_digest_line(st, "update", Some(&replacement), Some(&forgotten_id));
    *item = replacement;
    let out_changed = stored_changed || forgotten_changed;

    let err = storage::maybe_compact(st);
    (err, out_changed)
}

/// `claw_memory_update_with_result`.
pub fn update_with_result(item: &mut MemItem) -> (EspErr, bool) {
    let mut st = state::lock();
    if item.id.is_empty() {
        return (ESP_ERR_INVALID_ARG, false);
    }
    if !st.initialized {
        return (ESP_ERR_INVALID_STATE, false);
    }
    update_with_result_locked(&mut st, item)
}

// --- forget ---------------------------------------------------------------

fn forget_with_result_locked(
    st: &mut MemoryState,
    memory_id: &str,
    want_item: bool,
) -> (EspErr, Option<MemItem>, bool) {
    if memory_id.is_empty() {
        return (ESP_ERR_INVALID_ARG, None, false);
    }

    let mut items = match storage::load_current_items(st) {
        Ok(v) => v,
        Err(e) => return (e, None, false),
    };
    let idx = match items.iter().position(|i| i.id == memory_id) {
        Some(i) if !items[i].deleted => i,
        _ => return (ESP_ERR_NOT_FOUND, None, false),
    };

    let out_item = if want_item { Some(items[idx].clone()) } else { None };

    let mut forgotten = items[idx].clone();
    forgotten.deleted = true;
    forgotten.updated_at = util::now_sec();

    let err = storage::append_record(st, &forgotten);
    if err != ESP_OK {
        return (err, None, false);
    }

    let mut index_root = match storage::load_index(st) {
        Ok(v) => v,
        Err(e) => return (e, None, false),
    };

    items[idx] = forgotten.clone();
    storage::adjust_summary_stats(&mut index_root, &forgotten, -1);
    storage::remove_unused_summaries(&mut index_root);
    storage::rebuild_keyword_index(&mut index_root, &items);
    let mut err = storage::save_index(st, &index_root);
    if err == ESP_OK {
        let active: Vec<MemItem> = items.iter().filter(|i| !i.deleted).cloned().collect();
        err = storage::sync_markdown(st, &active, &index_root);
    }
    let mut out_changed = false;
    if err == ESP_OK {
        storage::append_digest_line(st, "forget", Some(&forgotten), None);
        st.write_changes_since_compact += 1;
        out_changed = true;
    }

    if err == ESP_OK {
        err = storage::maybe_compact(st);
    }
    (err, out_item, out_changed)
}

/// `claw_memory_forget_with_result`.
pub fn forget_with_result(memory_id: &str, want_item: bool) -> (EspErr, Option<MemItem>, bool) {
    if memory_id.is_empty() {
        return (ESP_ERR_INVALID_ARG, None, false);
    }
    let mut st = state::lock();
    if !st.initialized {
        return (ESP_ERR_INVALID_STATE, None, false);
    }
    forget_with_result_locked(&mut st, memory_id, want_item)
}

// --- replace conflicting (used by auto-extract) ---------------------------

/// `claw_memory_replace_conflicting_items`.
pub fn replace_conflicting_items(incoming: &MemItem, intent: MessageIntent) -> EspErr {
    if incoming.id.is_empty() || intent != MessageIntent::Replace {
        return ESP_OK;
    }
    let mut st = state::lock();
    if !st.initialized {
        return ESP_ERR_INVALID_STATE;
    }
    let items = match storage::load_current_items(&st) {
        Ok(v) => v,
        Err(e) => return e,
    };
    for existing in items.iter() {
        if !item::items_conflict_for_replacement(existing, incoming) {
            continue;
        }
        let (err, _, _) = forget_with_result_locked(&mut st, &existing.id, false);
        if err != ESP_OK && err != ESP_ERR_NOT_FOUND {
            return err;
        }
    }
    ESP_OK
}

// --- recall / list --------------------------------------------------------

fn item_matches_summary_ids(item: &MemItem, summary_ids: &[i64]) -> bool {
    if summary_ids.is_empty() {
        return true;
    }
    for &sid in summary_ids {
        for &have in item.summary_ids.iter().take(MAX_SUMMARIES) {
            if have as i64 == sid {
                return true;
            }
        }
    }
    false
}

fn render_item_list_json(items: &[MemItem], index_root: &Value) -> Result<String, EspErr> {
    let arr: Vec<Value> = items
        .iter()
        .map(|i| storage::item_to_json(i, Some(index_root)))
        .collect();
    serde_json::to_string(&Value::Array(arr)).map_err(|_| claw_interfaces::error::ESP_ERR_NO_MEM)
}

/// `claw_memory_recall`.
pub fn recall(labels: &[String], limit: usize) -> (EspErr, Option<String>) {
    let st = state::lock();
    if !st.initialized {
        return (ESP_ERR_INVALID_STATE, None);
    }

    let index_root = match storage::load_index(&st) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };

    let mut summary_ids: Vec<i64> = Vec::new();
    for label in labels.iter().take(MAX_SUMMARIES) {
        if let Some(id) = storage::find_summary_id_by_label(&index_root, label) {
            summary_ids.push(id);
        }
    }
    if !labels.is_empty() && summary_ids.is_empty() {
        return match render_item_list_json(&[], &index_root) {
            Ok(s) => (ESP_OK, Some(s)),
            Err(e) => (e, None),
        };
    }

    let items = match storage::load_current_items(&st) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };

    let limit = if limit > 0 { limit } else { RECALL_DEFAULT_LIMIT };
    let mut matches: Vec<MemItem> = Vec::new();
    for item in items.iter() {
        if item.deleted {
            continue;
        }
        if !item_matches_summary_ids(item, &summary_ids) {
            continue;
        }
        matches.push(item.clone());
    }

    if matches.len() > 1 {
        storage::sort_by_priority_desc(&mut matches);
    }
    if matches.len() > limit {
        matches.truncate(limit);
    }

    match render_item_list_json(&matches, &index_root) {
        Ok(s) => {
            for m in matches.iter() {
                storage::append_digest_line(&st, "recall", Some(m), Some("access_count+1"));
            }
            (ESP_OK, Some(s))
        }
        Err(e) => (e, None),
    }
}

/// `claw_memory_list`.
pub fn list() -> (EspErr, Option<String>) {
    let st = state::lock();
    if !st.initialized {
        return (ESP_ERR_INVALID_STATE, None);
    }
    let items = match storage::load_current_items(&st) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };
    let mut filtered: Vec<MemItem> = items.into_iter().filter(|i| !i.deleted).collect();
    if filtered.len() > 1 {
        storage::sort_by_priority_desc(&mut filtered);
    }
    let index_root = match storage::load_index(&st) {
        Ok(v) => v,
        Err(e) => return (e, None),
    };
    match render_item_list_json(&filtered, &index_root) {
        Ok(s) => (ESP_OK, Some(s)),
        Err(e) => (e, None),
    }
}

// --- primary summary label ------------------------------------------------

/// `claw_memory_item_primary_summary_label`.
pub fn item_primary_summary_label(item: &MemItem) -> Option<String> {
    item::primary_summary_label(item)
}

// --- long-term provider ---------------------------------------------------

const LONG_TERM_PREAMBLE: &str = "The auto-injected long-term memory context only contains summary labels, not full memory bodies.\nUse exact summary labels with memory_recall when you need detailed long-term memory.\nSummary labels must be copied verbatim from the catalog below. Do not invent new labels.\nIf the user asks what you remember about them, what they like or prefer, or asks you to verify a remembered fact, call memory_recall before answering when any relevant summary label is present.\nIf the user asks you to forget a remembered item, first inspect memory_id with memory_recall or memory_list, then call memory_forget with that exact memory_id.\nIf the user asks you to update a remembered item and any relevant summary label exists, first choose the most relevant summary labels from this catalog, call memory_recall to inspect the original memory bodies and memory_id values, then call memory_update with the selected memory_id.\nDo not rely on session history alone for those recall questions, because long-term memory may contain additional facts not visible in the recent chat.\nDo not treat /memory/MEMORY.md or raw memory files as the retrieval source of truth.\nDo not mention internal memory policy, storage behavior, auto-extraction, or whether you will or will not remember something unless the user explicitly asks about memory behavior.\nSummary label catalog:\n";

/// `claw_memory_long_term_collect` content. Returns `ESP_ERR_*` on failure.
pub fn long_term_collect_content() -> Result<String, EspErr> {
    let st = state::lock();
    let index_root = storage::load_index(&st)?;

    let mut content = String::from(LONG_TERM_PREAMBLE);
    let mut count = 0usize;
    if let Some(summaries) = index_root.get("summaries").and_then(|v| v.as_array()) {
        count = summaries.len();
        for item in summaries {
            if let Some(label) = item.get("label").and_then(|v| v.as_str()) {
                content.push_str(&format!("- {}\n", label));
            }
        }
    }
    if count == 0 {
        content.push_str("- (empty)\n");
    }
    Ok(content)
}
