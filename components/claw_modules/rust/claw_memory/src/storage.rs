//! JSONL records, the summary/keyword index, markdown export, and compaction.
//! Ports `claw_memory_storage.c`. The cJSON object/array manipulation is mirrored
//! with `serde_json::Value` (the crate enables `preserve_order` so field
//! insertion order matches the C output).

use serde_json::{Map, Value};

use crate::consts::{MAX_ACTIVE_ITEMS, MAX_SUMMARIES};
use crate::error::{MemoryError, MemoryResult};
use crate::item::{self, MemItem};
use crate::state::MemoryState;
use crate::util;

// --- index root -----------------------------------------------------------

pub fn new_index_root() -> Value {
    let mut root = Map::new();
    root.insert("version".into(), Value::from(3i64));
    root.insert("next_summary_id".into(), Value::from(1i64));
    root.insert("last_compact_digest_size".into(), Value::from(0i64));
    root.insert("summaries".into(), Value::Array(Vec::new()));
    root.insert("keyword_index".into(), Value::Object(Map::new()));
    Value::Object(root)
}

fn num_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}

/// `claw_memory_load_index` (with the legacy normalization/sanitization).
pub fn load_index(state: &MemoryState) -> MemoryResult<Value> {
    let raw = match util::read_file_dup(&state.index_path) {
        Ok(s) => s,
        Err(MemoryError::NotFound) => return Ok(new_index_root()),
        Err(e) => return Err(e),
    };

    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            log::error!("Failed to parse memory index: {}", state.index_path);
            return Err(MemoryError::Fail);
        }
    };

    {
        let obj = root.as_object_mut().unwrap();
        if !obj.get("summaries").is_some_and(|v| v.is_array()) {
            obj.insert("summaries".into(), Value::Array(Vec::new()));
        }
        if !obj.get("next_summary_id").is_some_and(|v| v.is_number()) {
            obj.insert("next_summary_id".into(), Value::from(1i64));
        }
        if !obj.get("last_compact_digest_size").is_some_and(|v| v.is_number()) {
            obj.insert("last_compact_digest_size".into(), Value::from(0i64));
        }
        if !obj.get("keyword_index").is_some_and(|v| v.is_object()) {
            obj.insert("keyword_index".into(), Value::Object(Map::new()));
        }
    }

    // Sanitize summary entries to {summary_id, label, ref_count}, dropping any
    // without a usable label (preserving order).
    let cleaned: Vec<Value> = {
        let summaries = root.get("summaries").and_then(|v| v.as_array()).unwrap();
        let mut out = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let label = summary.get("label").and_then(|v| v.as_str());
            let label = match label {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => continue,
            };
            let summary_id = num_i64(summary, "summary_id").unwrap_or(0);
            let ref_count = num_i64(summary, "ref_count").unwrap_or(0);
            let mut clean = Map::new();
            clean.insert("summary_id".into(), Value::from(if summary_id > 0 { summary_id } else { 0 }));
            clean.insert("label".into(), Value::from(label));
            clean.insert("ref_count".into(), Value::from(if ref_count > 0 { ref_count } else { 0 }));
            out.push(Value::Object(clean));
        }
        out
    };
    root.as_object_mut()
        .unwrap()
        .insert("summaries".into(), Value::Array(cleaned));

    Ok(root)
}

pub fn save_index(state: &MemoryState, root: &Value) -> MemoryResult<()> {
    match serde_json::to_string(root) {
        Ok(text) => util::write_file_text(&state.index_path, &text),
        Err(_) => Err(MemoryError::NoMem),
    }
}

fn summaries_ref(root: &Value) -> Option<&Vec<Value>> {
    root.get("summaries").and_then(|v| v.as_array())
}

fn summaries_mut(root: &mut Value) -> Option<&mut Vec<Value>> {
    root.get_mut("summaries").and_then(|v| v.as_array_mut())
}

pub fn find_summary_id_by_label(root: &Value, label: &str) -> Option<i64> {
    if label.is_empty() {
        return None;
    }
    let summaries = summaries_ref(root)?;
    for s in summaries {
        if s.get("label").and_then(|v| v.as_str()) == Some(label) {
            return s.get("summary_id").and_then(|v| v.as_i64());
        }
    }
    None
}

fn find_summary_index_by_id(summaries: &[Value], summary_id: i64) -> Option<usize> {
    if summary_id <= 0 {
        return None;
    }
    summaries
        .iter()
        .position(|s| s.get("summary_id").and_then(|v| v.as_i64()) == Some(summary_id))
}

fn label_for_summary_id(root: &Value, summary_id: i64) -> Option<String> {
    if summary_id <= 0 {
        return None;
    }
    let summaries = summaries_ref(root)?;
    let idx = find_summary_index_by_id(summaries, summary_id)?;
    summaries[idx]
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn next_summary_id(root: &mut Value) -> i64 {
    let valid = root
        .get("next_summary_id")
        .and_then(|v| v.as_i64())
        .filter(|&n| n > 0);
    match valid {
        Some(n) => n,
        None => {
            root.as_object_mut()
                .unwrap()
                .insert("next_summary_id".into(), Value::from(1i64));
            1
        }
    }
}

fn set_next_summary_id(root: &mut Value, next_id: i64) {
    root.as_object_mut()
        .unwrap()
        .insert("next_summary_id".into(), Value::from(next_id));
}

/// `claw_memory_ensure_summary_label`.
pub fn ensure_summary_label(root: &mut Value, label: &str, preferred_id: i64) -> MemoryResult<i64> {
    if label.is_empty() {
        return Err(MemoryError::InvalidArg);
    }
    if let Some(id) = find_summary_id_by_label(root, label) {
        return Ok(id);
    }
    if summaries_ref(root).is_none() {
        return Err(MemoryError::Fail);
    }

    let mut next_id = next_summary_id(root);
    if preferred_id > 0 {
        let exists = summaries_ref(root)
            .is_some_and(|s| find_summary_index_by_id(s, preferred_id).is_some());
        if !exists {
            next_id = preferred_id;
        }
    }

    let mut summary = Map::new();
    summary.insert("summary_id".into(), Value::from(next_id));
    summary.insert("label".into(), Value::from(label.to_string()));
    summary.insert("ref_count".into(), Value::from(0i64));
    summaries_mut(root).unwrap().push(Value::Object(summary));

    if next_id >= next_summary_id(root) {
        set_next_summary_id(root, next_id + 1);
    }
    Ok(next_id)
}

/// `claw_memory_adjust_summary_stats`.
pub fn adjust_summary_stats(root: &mut Value, item: &MemItem, ref_delta: i64) {
    let ids: Vec<i64> = item
        .summary_ids
        .iter()
        .take(MAX_SUMMARIES)
        .map(|&v| v as i64)
        .collect();
    let summaries = match summaries_mut(root) {
        Some(s) => s,
        None => return,
    };
    for id in ids {
        if let Some(idx) = find_summary_index_by_id(summaries, id) {
            let cur = summaries[idx].get("ref_count").and_then(|v| v.as_i64()).unwrap_or(0);
            let mut next = cur + ref_delta;
            if next < 0 {
                next = 0;
            }
            summaries[idx]
                .as_object_mut()
                .unwrap()
                .insert("ref_count".into(), Value::from(next));
        }
    }
}

/// `claw_memory_remove_unused_summaries`.
pub fn remove_unused_summaries(root: &mut Value) {
    if let Some(summaries) = summaries_mut(root) {
        summaries.retain(|s| s.get("ref_count").and_then(|v| v.as_i64()).is_some_and(|n| n > 0));
    }
}

// --- item <-> json --------------------------------------------------------

fn read_field(json: &Value, key: &str) -> Option<String> {
    json.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// `claw_memory_item_from_json`.
pub fn item_from_json(json: &Value) -> MemItem {
    let mut item = MemItem::default();
    if let Some(s) = read_field(json, "id") {
        item.set_id(&s);
    }
    if let Some(s) = read_field(json, "source") {
        item.set_source(&s);
    }
    if let Some(s) = read_field(json, "content") {
        item.set_content(&s);
    }
    if let Some(s) = read_field(json, "tags") {
        item.set_tags(&s);
    }
    if let Some(s) = read_field(json, "keywords") {
        item.set_keywords(&s);
    }
    item.created_at = json.get("created_at").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    item.updated_at = json.get("updated_at").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    item.access_count = json.get("access_count").and_then(|v| v.as_f64()).unwrap_or(0.0) as u16;
    item.deleted = json.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false);

    if let Some(arr) = json.get("summary_ids").and_then(|v| v.as_array()) {
        let count = arr.len().min(MAX_SUMMARIES);
        item.summary_ids = arr[..count]
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as u16)
            .collect();
    }
    item
}

/// `claw_memory_item_to_json` (includes resolved `summary_labels`).
pub fn item_to_json(item: &MemItem, index_root: Option<&Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), Value::from(item.id.clone()));
    obj.insert("source".into(), Value::from(item.source.clone()));
    obj.insert("content".into(), Value::from(item.content.clone()));
    obj.insert("tags".into(), Value::from(item.tags.clone()));
    obj.insert("keywords".into(), Value::from(item.keywords.clone()));
    obj.insert("created_at".into(), Value::from(item.created_at as u64));
    obj.insert("updated_at".into(), Value::from(item.updated_at as u64));
    obj.insert("access_count".into(), Value::from(item.access_count as u64));
    obj.insert("deleted".into(), Value::from(item.deleted));

    let mut ids = Vec::new();
    let mut labels = Vec::new();
    for &sid in item.summary_ids.iter().take(MAX_SUMMARIES) {
        ids.push(Value::from(sid as u64));
        if let Some(root) = index_root {
            if let Some(label) = label_for_summary_id(root, sid as i64) {
                labels.push(Value::from(label));
            }
        }
    }
    obj.insert("summary_ids".into(), Value::Array(ids));
    obj.insert("summary_labels".into(), Value::Array(labels));
    Value::Object(obj)
}

/// Build the JSONL record object (no `summary_labels`).
fn record_json(item: &MemItem) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), Value::from(item.id.clone()));
    obj.insert("source".into(), Value::from(item.source.clone()));
    obj.insert("content".into(), Value::from(item.content.clone()));
    obj.insert("tags".into(), Value::from(item.tags.clone()));
    obj.insert("keywords".into(), Value::from(item.keywords.clone()));
    obj.insert("created_at".into(), Value::from(item.created_at as u64));
    obj.insert("updated_at".into(), Value::from(item.updated_at as u64));
    obj.insert("access_count".into(), Value::from(item.access_count as u64));
    obj.insert("deleted".into(), Value::from(item.deleted));
    let ids: Vec<Value> = item
        .summary_ids
        .iter()
        .take(MAX_SUMMARIES)
        .map(|&v| Value::from(v as u64))
        .collect();
    obj.insert("summary_ids".into(), Value::Array(ids));
    Value::Object(obj)
}

/// `claw_memory_append_record`.
pub fn append_record(state: &MemoryState, item: &MemItem) -> MemoryResult<()> {
    let text = match serde_json::to_string(&record_json(item)) {
        Ok(t) => t,
        Err(_) => return Err(MemoryError::NoMem),
    };
    util::append_file_text(&state.records_path, &format!("{}\n", text))
}

/// `claw_memory_load_current_items` (deduplicated by id, later records win).
pub fn load_current_items(state: &MemoryState) -> MemoryResult<Vec<MemItem>> {
    let raw = match util::read_file_dup(&state.records_path) {
        Ok(s) => s,
        Err(MemoryError::NotFound) => return Ok(Vec::new()),
        Err(_) => {
            log::error!("Failed to open memory records: {}", state.records_path);
            return Err(MemoryError::Fail);
        }
    };

    let mut list: Vec<MemItem> = Vec::new();
    for line in raw.lines() {
        // C reads with fgets(line, 1024): only the first 1023 bytes per line.
        let line = util::truncate_to_bytes(line, 1023);
        let line = line.as_str();
        if line.is_empty() {
            continue;
        }
        let json: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let item = item_from_json(&json);
        if item.id.is_empty() {
            continue;
        }
        if let Some(existing) = list.iter_mut().find(|i| i.id == item.id) {
            *existing = item;
        } else {
            list.push(item);
        }
    }
    Ok(list)
}

/// Comparator for `claw_memory_sort_by_priority_desc`.
pub fn sort_by_priority_desc(items: &mut [MemItem]) {
    items.sort_by(|a, b| {
        b.access_count
            .cmp(&a.access_count)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

// --- digest ---------------------------------------------------------------

/// `claw_memory_append_digest_line`. `extra = Some(s)` emits a tab + `s`
/// (even when `s` is empty), matching the C `extra ? "\t" : ""` behaviour.
pub fn append_digest_line(state: &MemoryState, action: &str, item: Option<&MemItem>, extra: Option<&str>) {
    let ts = util::now_sec().to_string();
    let id = match item {
        Some(i) if !i.id.is_empty() => i.id.as_str(),
        _ => "-",
    };
    let action = if action.is_empty() { "unknown" } else { action };
    let line = match extra {
        Some(e) => format!("{}\t{}\t{}\t{}\n", ts, action, id, e),
        None => format!("{}\t{}\t{}\n", ts, action, id),
    };
    // The C `claw_memory_append_digest_line` is best-effort and ignores write
    // failures; preserve that behaviour.
    let _ = util::append_file_text(&state.digest_path, &line);
}

// --- keyword index --------------------------------------------------------

fn keyword_index_add_term(keyword_index: &mut Map<String, Value>, memory_id: &str, term: &str) {
    if memory_id.is_empty() || term.is_empty() {
        return;
    }
    let normalized = util::trim_whitespace(&util::safe_copy(term, 48)).to_string();
    if normalized.is_empty() {
        return;
    }
    let entry = keyword_index
        .entry(normalized)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let arr = entry.as_array_mut().unwrap();
    let exists = arr.iter().any(|v| v.as_str() == Some(memory_id));
    if !exists {
        arr.push(Value::from(memory_id.to_string()));
    }
}

fn keyword_index_add_csv(keyword_index: &mut Map<String, Value>, memory_id: &str, csv_text: &str) {
    if memory_id.is_empty() || csv_text.is_empty() {
        return;
    }
    let copy = util::safe_copy(csv_text, 160);
    for token in util::csv_split(&copy) {
        keyword_index_add_term(keyword_index, memory_id, token);
    }
}

/// `claw_memory_rebuild_keyword_index`.
pub fn rebuild_keyword_index(root: &mut Value, items: &[MemItem]) {
    let mut keyword_index: Map<String, Value> = Map::new();
    for item in items {
        if item.deleted || item.id.is_empty() {
            continue;
        }
        keyword_index_add_csv(&mut keyword_index, &item.id, &item.keywords);
        if item.keywords.is_empty() {
            keyword_index_add_csv(&mut keyword_index, &item.id, &item.tags);
        }
    }
    root.as_object_mut()
        .unwrap()
        .insert("keyword_index".into(), Value::Object(keyword_index));
}

// --- markdown -------------------------------------------------------------

/// `claw_memory_export_markdown_internal`.
pub fn export_markdown(items: &[MemItem], index_root: Option<&Value>) -> String {
    let mut buf = String::from("# Long-term Memory\n\n");

    let summaries = index_root.and_then(summaries_ref);
    if let Some(summaries) = summaries {
        if !summaries.is_empty() {
            buf.push_str("## Summary Labels\n\n");
            for summary in summaries {
                let label = match summary.get("label").and_then(|v| v.as_str()) {
                    Some(l) => l,
                    None => continue,
                };
                let ref_count = summary.get("ref_count").and_then(|v| v.as_i64()).unwrap_or(0);
                buf.push_str(&format!("- {} (refs={})\n", label, ref_count));
            }
            buf.push('\n');
        }
    }

    if items.is_empty() {
        buf.push_str("(empty - Claw will write memories here as it learns)\n");
        return buf;
    }

    let mut sorted = items.to_vec();
    sort_by_priority_desc(&mut sorted);

    buf.push_str("## Active Memories\n\n");
    for item in &sorted {
        if item.deleted {
            continue;
        }
        let item_json = item_to_json(item, index_root);
        let labels_json = item_json
            .get("summary_labels")
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| "[]".into());
        let updated = util::format_timestamp(item.updated_at);
        buf.push_str(&format!(
            "- `{}` {}\n  time={} access={} labels={}\n",
            item.id, item.content, updated, item.access_count, labels_json
        ));
    }
    buf
}

/// `claw_memory_sync_markdown`.
pub fn sync_markdown(state: &MemoryState, items: &[MemItem], index_root: &Value) -> MemoryResult<()> {
    let markdown = export_markdown(items, Some(index_root));
    util::write_file_text(&state.markdown_path, &markdown)
}

// --- compaction -----------------------------------------------------------

fn capacity_score(item: &MemItem) -> i64 {
    (item.access_count as i64) * 3 + (item.updated_at as i64 / 3600)
}

fn trim_to_capacity(items: &mut Vec<MemItem>) {
    while items.len() > MAX_ACTIVE_ITEMS {
        // `min_by_key` keeps the first of equal-minimum scores, matching the C
        // strict-less-than scan that retains the earliest lowest-scoring item.
        let remove_idx = items
            .iter()
            .enumerate()
            .min_by_key(|(_, item)| capacity_score(item))
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        items.remove(remove_idx);
    }
}

fn index_last_compact_digest_size(root: &Value) -> usize {
    match root.get("last_compact_digest_size").and_then(|v| v.as_f64()) {
        Some(v) if v >= 0.0 => v as usize,
        _ => 0,
    }
}

fn set_index_last_compact_digest_size(root: &mut Value, size: usize) {
    root.as_object_mut()
        .unwrap()
        .insert("last_compact_digest_size".into(), Value::from(size as u64));
}

fn compact_dedup_key(item: &MemItem) -> String {
    let key = item::build_item_key(item);
    if key.is_empty() {
        util::normalize_text_for_key(&item.content, 80)
    } else {
        key
    }
}

/// `claw_memory_apply_digest_recall_deltas`.
fn apply_digest_recall_deltas(state: &MemoryState, items: &mut [MemItem], old_index: &mut Value, digest_size: usize) {
    let start_offset = index_last_compact_digest_size(old_index);
    if start_offset >= digest_size {
        set_index_last_compact_digest_size(old_index, digest_size);
        return;
    }

    let bytes = match std::fs::read(&state.digest_path) {
        Ok(b) => b,
        Err(_) => {
            set_index_last_compact_digest_size(old_index, digest_size);
            return;
        }
    };
    let slice_start = start_offset.min(bytes.len());
    let text = String::from_utf8_lossy(&bytes[slice_start..]);

    for line in text.split('\n') {
        let fields: Vec<&str> = line
            .split(['\t', '\r'])
            .filter(|s| !s.is_empty())
            .take(5)
            .collect();
        if fields.len() < 4 {
            continue;
        }
        if fields[1] != "recall" || fields[3] != "access_count+1" {
            continue;
        }
        if let Some(item) = items.iter_mut().find(|item| item.id == fields[2]) {
            item.access_count = item.access_count.saturating_add(1);
            if !fields[0].is_empty() {
                if let Ok(ts) = fields[0].parse::<u64>() {
                    if ts > item.updated_at as u64 {
                        item.updated_at = ts as u32;
                    }
                }
            }
        }
    }

    set_index_last_compact_digest_size(old_index, digest_size);
}

/// `claw_memory_compact_internal`.
pub fn compact_internal(state: &mut MemoryState, append_digest: bool) -> MemoryResult<()> {
    let mut items = load_current_items(state)?;
    let mut old_index = load_index(state)?;
    let digest_size = util::file_size_bytes(&state.digest_path);
    apply_digest_recall_deltas(state, &mut items, &mut old_index, digest_size);

    let mut compacted: Vec<MemItem> = Vec::new();
    for src in items.iter() {
        if src.deleted {
            continue;
        }
        let key = compact_dedup_key(src);
        let existing = if key.is_empty() {
            None
        } else {
            compacted.iter().position(|other| compact_dedup_key(other) == key)
        };

        match existing {
            Some(idx) => {
                let dst = &mut compacted[idx];
                if src.access_count > dst.access_count {
                    dst.access_count = src.access_count;
                }
                if src.updated_at > dst.updated_at {
                    dst.updated_at = src.updated_at;
                    dst.set_content(&src.content);
                    dst.set_tags(&src.tags);
                    dst.set_source(&src.source);
                }
                if src.created_at != 0 && (dst.created_at == 0 || src.created_at < dst.created_at) {
                    dst.created_at = src.created_at;
                }
            }
            None => compacted.push(src.clone()),
        }
    }

    trim_to_capacity(&mut compacted);

    let mut new_index = new_index_root();
    for item in compacted.iter_mut() {
        item.summary_ids.clear();
        let labels = item::collect_summary_labels(item);
        for label in labels.iter().take(MAX_SUMMARIES) {
            let old_id = find_summary_id_by_label(&old_index, label).unwrap_or(0);
            match ensure_summary_label(&mut new_index, label, old_id) {
                Ok(new_id) => item.summary_ids.push(new_id as u16),
                Err(_) => continue,
            }
        }
        adjust_summary_stats(&mut new_index, item, 1);
    }
    rebuild_keyword_index(&mut new_index, &compacted);
    set_index_last_compact_digest_size(&mut new_index, digest_size);

    let mut records_text = String::new();
    for item in compacted.iter() {
        if let Ok(text) = serde_json::to_string(&item_to_json(item, None)) {
            records_text.push_str(&text);
            records_text.push('\n');
        }
    }

    let mut result = util::write_file_text(&state.records_path, &records_text);
    if result.is_ok() {
        result = save_index(state, &new_index);
    }
    if result.is_ok() {
        result = sync_markdown(state, &compacted, &new_index);
    }
    if result.is_ok() && append_digest {
        append_digest_line(state, "compact", None, Some("rebuild=index+markdown"));
    }

    state.write_changes_since_compact = 0;
    result
}

/// `claw_memory_maybe_compact`.
pub fn maybe_compact(state: &mut MemoryState) -> MemoryResult<()> {
    if state.write_changes_since_compact >= crate::consts::COMPACT_CHANGE_THRESHOLD
        || util::file_size_bytes(&state.records_path) >= crate::consts::COMPACT_SIZE_THRESHOLD
    {
        return compact_internal(state, true);
    }
    Ok(())
}

/// `claw_memory_summary_catalog_dup`. Only consumed by the espidf async-extract
/// prompt builder; dead on host.
#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
pub fn summary_catalog_dup(state: &MemoryState) -> String {
    let root = match load_index(state) {
        Ok(v) => v,
        Err(_) => return "- (empty)\n".to_string(),
    };
    let summaries = summaries_ref(&root);
    match summaries {
        Some(s) if !s.is_empty() => {
            let mut out = String::new();
            for item in s {
                if let Some(label) = item.get("label").and_then(|v| v.as_str()) {
                    out.push_str(&format!("- {}\n", label));
                }
            }
            if out.is_empty() {
                "- (empty)\n".to_string()
            } else {
                out
            }
        }
        _ => "- (empty)\n".to_string(),
    }
}
