//! Public CRUD operations (`claw_memory.c`) plus init and the long-term memory
//! provider. The single global state lock is acquired at each public entry
//! point; the `*_locked` helpers operate on `&mut MemoryState` so re-entrant
//! flows (update calling store + forget) never re-lock.

use serde_json::Value;

use crate::consts::{
    BackendFormat, MessageIntent, DEFAULT_MAX_MESSAGE_CHARS, DEFAULT_MAX_TOOL_ITERATIONS, MAX_PATH,
    MAX_SUMMARIES, RECALL_DEFAULT_LIMIT,
};
use crate::error::{MemoryError, MemoryResult};
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
pub fn init(config: &MemoryConfig) -> MemoryResult<()> {
    let session_root = match &config.session_root_dir {
        Some(s) => s.clone(),
        None => return Err(MemoryError::InvalidArg),
    };
    let memory_root = match &config.memory_root_dir {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return Err(MemoryError::InvalidArg),
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
        // `join_path` only fails with `InvalidSize`, matching the original
        // `ESP_ERR_INVALID_SIZE` return for an over-long path.
        st.markdown_path = util::join_path(&md, crate::consts::MARKDOWN_FILE)?;
        st.records_path = util::join_path(&md, crate::consts::RECORDS_FILE)?;
        st.index_path = util::join_path(&md, crate::consts::INDEX_FILE)?;
        st.digest_path = util::join_path(&md, crate::consts::DIGEST_FILE)?;

        if util::ensure_dir_recursive(&st.session_root_dir).is_err()
            || util::ensure_dir_recursive(&st.memory_root_dir).is_err()
        {
            return Err(MemoryError::Fail);
        }

        if crate::profile::profile_init_defaults(&mut st).is_err() {
            return Err(MemoryError::Fail);
        }

        if util::ensure_file_with_default(&st.records_path, "").is_err()
            || util::ensure_file_with_default(&st.index_path, DEFAULT_INDEX).is_err()
            || util::ensure_file_with_default(&st.digest_path, "").is_err()
            || util::ensure_file_with_default(&st.markdown_path, DEFAULT_MARKDOWN).is_err()
        {
            return Err(MemoryError::Fail);
        }

        st.initialized = true;
    }

    crate::extract::async_extract_init(config)?;

    {
        let mut st = state::lock();
        let _ = storage::compact_internal(&mut st, false);
        log::info!("Initialized memory root={}", st.memory_root_dir);
    }
    Ok(())
}

// --- store ----------------------------------------------------------------

fn fill_defaults(item: &mut MemItem) {
    if item.source.is_empty() {
        item.set_source("manual");
    }
}

/// `claw_memory_store_prepared_item_internal`. Returns whether the item was
/// actually written (`changed`).
fn store_prepared_item_internal(
    st: &mut MemoryState,
    item: &mut MemItem,
    digest_action: &str,
    digest_extra: Option<&str>,
    ignore_existing_id: Option<&str>,
) -> MemoryResult<bool> {
    if item.content.is_empty() {
        return Err(MemoryError::InvalidArg);
    }

    item.content = util::trim_whitespace(&item.content).to_string();
    item.tags = util::trim_whitespace(&item.tags).to_string();
    item.keywords = util::trim_whitespace(&item.keywords).to_string();
    item::normalize_item_metadata(item);
    if item.content.is_empty() {
        return Err(MemoryError::InvalidArg);
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

    let mut items = storage::load_current_items(st)?;

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
                return Ok(false);
            }
        }
        if item::items_semantically_match(existing, item) {
            *item = existing.clone();
            return Ok(false);
        }
    }

    let mut index_root = storage::load_index(st)?;

    let labels = item::collect_summary_labels(item);
    item.summary_ids.clear();
    for label in labels.iter().take(MAX_SUMMARIES) {
        let id = storage::ensure_summary_label(&mut index_root, label, 0)?;
        item.summary_ids.push(id as u16);
    }

    storage::append_record(st, item)?;
    storage::adjust_summary_stats(&mut index_root, item, 1);
    items.push(item.clone());
    storage::rebuild_keyword_index(&mut index_root, &items);
    storage::save_index(st, &index_root)?;
    storage::sync_markdown(st, &items, &index_root)?;
    storage::append_digest_line(st, digest_action, Some(item), digest_extra);
    st.write_changes_since_compact += 1;

    storage::maybe_compact(st)?;
    Ok(true)
}

/// `claw_memory_store_with_result`.
pub fn store_with_result(item: &mut MemItem) -> MemoryResult<bool> {
    let mut st = state::lock();
    if !st.initialized {
        return Err(MemoryError::InvalidState);
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
fn prepare_updated_replacement(existing: &MemItem, patch: &MemItem) -> MemoryResult<MemItem> {
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
        return Err(MemoryError::InvalidArg);
    }
    Ok(replacement)
}

fn update_with_result_locked(st: &mut MemoryState, item: &mut MemItem) -> MemoryResult<bool> {
    if item.id.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    let orig_id = item.id.clone();

    let items = storage::load_current_items(st)?;
    let idx = match items.iter().position(|i| i.id == orig_id) {
        Some(i) if !items[i].deleted => i,
        _ => return Err(MemoryError::NotFound),
    };

    let existing = items[idx].clone();
    let prep = prepare_updated_replacement(&existing, item);
    if let Ok(ref replacement) = prep {
        if item::items_equivalent_for_update(&existing, replacement) {
            *item = existing;
            return Ok(false);
        }
    }
    let mut replacement = prep?;

    let stored_changed = store_prepared_item_internal(
        st,
        &mut replacement,
        "update_store",
        Some(&orig_id),
        Some(&orig_id),
    )?;

    let (forgotten, forgotten_changed) = forget_with_result_locked(st, &orig_id, true)?;

    let forgotten_id = forgotten.map(|f| f.id).unwrap_or_default();
    storage::append_digest_line(st, "update", Some(&replacement), Some(&forgotten_id));
    *item = replacement;
    let out_changed = stored_changed || forgotten_changed;

    storage::maybe_compact(st)?;
    Ok(out_changed)
}

/// `claw_memory_update_with_result`.
pub fn update_with_result(item: &mut MemItem) -> MemoryResult<bool> {
    let mut st = state::lock();
    if item.id.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    if !st.initialized {
        return Err(MemoryError::InvalidState);
    }
    update_with_result_locked(&mut st, item)
}

// --- forget ---------------------------------------------------------------

fn forget_with_result_locked(
    st: &mut MemoryState,
    memory_id: &str,
    want_item: bool,
) -> MemoryResult<(Option<MemItem>, bool)> {
    if memory_id.is_empty() {
        return Err(MemoryError::InvalidArg);
    }

    let mut items = storage::load_current_items(st)?;
    let idx = match items.iter().position(|i| i.id == memory_id) {
        Some(i) if !items[i].deleted => i,
        _ => return Err(MemoryError::NotFound),
    };

    let out_item = if want_item { Some(items[idx].clone()) } else { None };

    let mut forgotten = items[idx].clone();
    forgotten.deleted = true;
    forgotten.updated_at = util::now_sec();

    storage::append_record(st, &forgotten)?;

    let mut index_root = storage::load_index(st)?;

    items[idx] = forgotten.clone();
    storage::adjust_summary_stats(&mut index_root, &forgotten, -1);
    storage::remove_unused_summaries(&mut index_root);
    storage::rebuild_keyword_index(&mut index_root, &items);
    storage::save_index(st, &index_root)?;

    let active: Vec<MemItem> = items.iter().filter(|i| !i.deleted).cloned().collect();
    storage::sync_markdown(st, &active, &index_root)?;

    storage::append_digest_line(st, "forget", Some(&forgotten), None);
    st.write_changes_since_compact += 1;

    storage::maybe_compact(st)?;
    Ok((out_item, true))
}

/// `claw_memory_forget_with_result`.
pub fn forget_with_result(
    memory_id: &str,
    want_item: bool,
) -> MemoryResult<(Option<MemItem>, bool)> {
    if memory_id.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    let mut st = state::lock();
    if !st.initialized {
        return Err(MemoryError::InvalidState);
    }
    forget_with_result_locked(&mut st, memory_id, want_item)
}

// --- replace conflicting (used by auto-extract) ---------------------------

/// `claw_memory_replace_conflicting_items`.
pub fn replace_conflicting_items(incoming: &MemItem, intent: MessageIntent) -> MemoryResult<()> {
    if incoming.id.is_empty() || intent != MessageIntent::Replace {
        return Ok(());
    }
    let mut st = state::lock();
    if !st.initialized {
        return Err(MemoryError::InvalidState);
    }
    let items = storage::load_current_items(&st)?;
    for existing in items.iter() {
        if !item::items_conflict_for_replacement(existing, incoming) {
            continue;
        }
        // A missing item is benign (already gone); any other failure propagates.
        match forget_with_result_locked(&mut st, &existing.id, false) {
            Ok(_) | Err(MemoryError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
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

fn render_item_list_json(items: &[MemItem], index_root: &Value) -> MemoryResult<String> {
    let arr: Vec<Value> = items
        .iter()
        .map(|i| storage::item_to_json(i, Some(index_root)))
        .collect();
    serde_json::to_string(&Value::Array(arr)).map_err(|_| MemoryError::NoMem)
}

/// `claw_memory_recall`.
pub fn recall(labels: &[String], limit: usize) -> MemoryResult<String> {
    let st = state::lock();
    if !st.initialized {
        return Err(MemoryError::InvalidState);
    }

    let index_root = storage::load_index(&st)?;

    let mut summary_ids: Vec<i64> = Vec::new();
    for label in labels.iter().take(MAX_SUMMARIES) {
        if let Some(id) = storage::find_summary_id_by_label(&index_root, label) {
            summary_ids.push(id);
        }
    }
    if !labels.is_empty() && summary_ids.is_empty() {
        return render_item_list_json(&[], &index_root);
    }

    let items = storage::load_current_items(&st)?;

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

    let rendered = render_item_list_json(&matches, &index_root)?;
    for m in matches.iter() {
        storage::append_digest_line(&st, "recall", Some(m), Some("access_count+1"));
    }
    Ok(rendered)
}

/// `claw_memory_list`.
pub fn list() -> MemoryResult<String> {
    let st = state::lock();
    if !st.initialized {
        return Err(MemoryError::InvalidState);
    }
    let items = storage::load_current_items(&st)?;
    let mut filtered: Vec<MemItem> = items.into_iter().filter(|i| !i.deleted).collect();
    if filtered.len() > 1 {
        storage::sort_by_priority_desc(&mut filtered);
    }
    let index_root = storage::load_index(&st)?;
    render_item_list_json(&filtered, &index_root)
}

// --- primary summary label ------------------------------------------------

/// `claw_memory_item_primary_summary_label`.
pub fn item_primary_summary_label(item: &MemItem) -> Option<String> {
    item::primary_summary_label(item)
}

// --- long-term provider ---------------------------------------------------

const LONG_TERM_PREAMBLE: &str = "The auto-injected long-term memory context only contains summary labels, not full memory bodies.\nUse exact summary labels with memory_recall when you need detailed long-term memory.\nSummary labels must be copied verbatim from the catalog below. Do not invent new labels.\nIf the user asks what you remember about them, what they like or prefer, or asks you to verify a remembered fact, call memory_recall before answering when any relevant summary label is present.\nIf the user asks you to forget a remembered item, first inspect memory_id with memory_recall or memory_list, then call memory_forget with that exact memory_id.\nIf the user asks you to update a remembered item and any relevant summary label exists, first choose the most relevant summary labels from this catalog, call memory_recall to inspect the original memory bodies and memory_id values, then call memory_update with the selected memory_id.\nDo not rely on session history alone for those recall questions, because long-term memory may contain additional facts not visible in the recent chat.\nDo not treat /memory/MEMORY.md or raw memory files as the retrieval source of truth.\nDo not mention internal memory policy, storage behavior, auto-extraction, or whether you will or will not remember something unless the user explicitly asks about memory behavior.\nSummary label catalog:\n";

/// `claw_memory_long_term_collect` content.
pub fn long_term_collect_content() -> MemoryResult<String> {
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
