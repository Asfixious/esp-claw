//! `claw_memory_item_t` (the by-value C ABI struct) plus the ergonomic
//! internal `MemItem` model and the item-centric logic ported from
//! `claw_memory_utils.c` / `claw_memory_extract.c`.

use core::ffi::c_char;

use crate::consts::{
    CONTENT_CAP, ID_CAP, KEYWORDS_CAP, MAX_LABEL_CHARS, MAX_LABEL_TEXT, MAX_SUMMARIES, SOURCE_CAP,
    TAGS_CAP,
};
use crate::util;

/// The exact `claw_memory_item_t` C ABI layout (`include/claw_memory.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct claw_memory_item_t {
    pub id: [c_char; 40],
    pub source: [c_char; 16],
    pub content: [c_char; 256],
    pub summary_ids: [u16; 3],
    pub summary_id_count: u8,
    pub tags: [c_char; 96],
    pub keywords: [c_char; 128],
    pub created_at: u32,
    pub updated_at: u32,
    pub access_count: u16,
    pub deleted: u8,
}

impl Default for claw_memory_item_t {
    fn default() -> Self {
        claw_memory_item_t {
            id: [0; 40],
            source: [0; 16],
            content: [0; 256],
            summary_ids: [0; 3],
            summary_id_count: 0,
            tags: [0; 96],
            keywords: [0; 128],
            created_at: 0,
            updated_at: 0,
            access_count: 0,
            deleted: 0,
        }
    }
}

fn read_carr(arr: &[c_char]) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(arr.len());
    for &c in arr {
        let b = c as u8;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn write_carr(arr: &mut [c_char], s: &str) {
    for slot in arr.iter_mut() {
        *slot = 0;
    }
    let cap = arr.len();
    if cap == 0 {
        return;
    }
    let truncated = util::safe_copy(s, cap);
    let bytes = truncated.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        arr[i] = b as c_char;
    }
}

/// Internal item model with owned `String` fields. Each string is kept within
/// the matching C buffer capacity by the setters.
#[derive(Clone, Debug, Default)]
pub struct MemItem {
    pub id: String,
    pub source: String,
    pub content: String,
    pub summary_ids: Vec<u16>,
    pub tags: String,
    pub keywords: String,
    pub created_at: u32,
    pub updated_at: u32,
    pub access_count: u16,
    pub deleted: bool,
}

impl MemItem {
    pub fn from_c(c: &claw_memory_item_t) -> MemItem {
        let count = (c.summary_id_count as usize).min(MAX_SUMMARIES);
        let summary_ids = c.summary_ids[..count].to_vec();
        MemItem {
            id: read_carr(&c.id),
            source: read_carr(&c.source),
            content: read_carr(&c.content),
            summary_ids,
            tags: read_carr(&c.tags),
            keywords: read_carr(&c.keywords),
            created_at: c.created_at,
            updated_at: c.updated_at,
            access_count: c.access_count,
            deleted: c.deleted != 0,
        }
    }

    pub fn to_c(&self) -> claw_memory_item_t {
        let mut c = claw_memory_item_t::default();
        write_carr(&mut c.id, &self.id);
        write_carr(&mut c.source, &self.source);
        write_carr(&mut c.content, &self.content);
        write_carr(&mut c.tags, &self.tags);
        write_carr(&mut c.keywords, &self.keywords);
        let count = self.summary_ids.len().min(MAX_SUMMARIES);
        for i in 0..count {
            c.summary_ids[i] = self.summary_ids[i];
        }
        c.summary_id_count = count as u8;
        c.created_at = self.created_at;
        c.updated_at = self.updated_at;
        c.access_count = self.access_count;
        c.deleted = if self.deleted { 1 } else { 0 };
        c
    }

    pub fn set_id(&mut self, s: &str) {
        self.id = util::safe_copy(s, ID_CAP);
    }
    pub fn set_source(&mut self, s: &str) {
        self.source = util::safe_copy(s, SOURCE_CAP);
    }
    pub fn set_content(&mut self, s: &str) {
        self.content = util::safe_copy(s, CONTENT_CAP);
    }
    pub fn set_tags(&mut self, s: &str) {
        self.tags = util::safe_copy(s, TAGS_CAP);
    }
    pub fn set_keywords(&mut self, s: &str) {
        self.keywords = util::safe_copy(s, KEYWORDS_CAP);
    }
}

// --- item-centric logic ---------------------------------------------------

/// `claw_memory_make_id`: increments the sequence counter and formats the id.
pub fn make_id(next_seq: &mut u32) -> String {
    let now = util::now_sec();
    *next_seq = next_seq.wrapping_add(1);
    let id = format!("mem-{}-{:04}", now, *next_seq % 10000);
    util::safe_copy(&id, ID_CAP)
}

/// `claw_memory_build_item_key`.
pub fn build_item_key(item: &MemItem) -> String {
    let normalized = util::normalize_text_for_key(&item.content, 160);
    if normalized.is_empty() {
        return format!("memory-{}", util::now_sec());
    }
    util::utf8_copy_chars(&normalized, 37, 36)
}

/// `claw_memory_build_semantic_key`.
pub fn build_semantic_key(item: &MemItem) -> String {
    util::normalize_text_for_key(&item.content, 160)
}

/// `claw_memory_items_semantically_match`.
pub fn items_semantically_match(existing: &MemItem, incoming: &MemItem) -> bool {
    if existing.deleted {
        return false;
    }
    let existing_key = build_semantic_key(existing);
    let incoming_key = build_semantic_key(incoming);
    if existing_key.is_empty() || incoming_key.is_empty() {
        return false;
    }
    if existing_key == incoming_key {
        return true;
    }
    if existing_key.len() >= 12 && incoming_key.len() >= 12 {
        return existing_key.contains(&incoming_key) || incoming_key.contains(&existing_key);
    }
    false
}

/// `claw_memory_items_equivalent_for_update`.
pub fn items_equivalent_for_update(existing: &MemItem, replacement: &MemItem) -> bool {
    existing.source == replacement.source
        && existing.content == replacement.content
        && existing.tags == replacement.tags
        && existing.keywords == replacement.keywords
}

fn csv_contains_term(csv_text: &str, term: &str) -> bool {
    if csv_text.is_empty() || term.is_empty() {
        return false;
    }
    let copy = util::safe_copy(csv_text, 160);
    for token in util::csv_split(&copy) {
        let trimmed = util::trim_whitespace(token);
        if !trimmed.is_empty() && trimmed == term {
            return true;
        }
    }
    false
}

fn csv_has_overlap(lhs: &str, rhs: &str) -> bool {
    if lhs.is_empty() || rhs.is_empty() {
        return false;
    }
    let copy = util::safe_copy(lhs, 160);
    for token in util::csv_split(&copy) {
        let trimmed = util::trim_whitespace(token);
        if !trimmed.is_empty() && csv_contains_term(rhs, trimmed) {
            return true;
        }
    }
    false
}

/// `claw_memory_items_conflict_for_replacement`.
pub fn items_conflict_for_replacement(existing: &MemItem, incoming: &MemItem) -> bool {
    if existing.deleted {
        return false;
    }
    if !existing.id.is_empty() && !incoming.id.is_empty() && existing.id == incoming.id {
        return false;
    }
    if items_semantically_match(existing, incoming) {
        return false;
    }
    if !csv_has_overlap(&existing.tags, &incoming.tags)
        && !csv_has_overlap(&existing.keywords, &incoming.keywords)
        && !csv_has_overlap(&existing.tags, &incoming.keywords)
        && !csv_has_overlap(&existing.keywords, &incoming.tags)
    {
        return false;
    }
    true
}

fn term_is_grounded(term: &str, content: &str, tags: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    util::text_contains_token(content, term) || util::text_contains_token(tags, term)
}

/// `claw_memory_append_csv_terms`: append unique CSV tokens from `src` into
/// `dst` (comma-joined), bounded by `dst_size` bytes and `max_terms`. Returns
/// the new term count.
#[allow(clippy::too_many_arguments)]
fn append_csv_terms(
    dst: &mut String,
    dst_size: usize,
    mut current_count: usize,
    max_terms: usize,
    src: &str,
    content: &str,
    tags: &str,
    require_grounding: bool,
) -> usize {
    if dst_size == 0 || src.is_empty() || current_count >= max_terms {
        return current_count;
    }
    let copy = util::safe_copy(src, 192);
    for token in util::csv_split(&copy) {
        if current_count >= max_terms {
            break;
        }
        let trimmed = util::trim_whitespace(token);
        if trimmed.is_empty() || csv_contains_term(dst, trimmed) {
            continue;
        }
        if require_grounding && !term_is_grounded(trimmed, content, tags) {
            continue;
        }
        let sep = if dst.is_empty() { "" } else { "," };
        let candidate = format!("{}{}", sep, trimmed);
        // snprintf bound check: written must fit within remaining dst_size.
        if dst.len() + candidate.len() >= dst_size {
            break;
        }
        dst.push_str(&candidate);
        current_count += 1;
    }
    current_count
}

/// `claw_memory_normalize_item_metadata`.
pub fn normalize_item_metadata(item: &mut MemItem) {
    let mut normalized_tags = String::new();
    append_csv_terms(
        &mut normalized_tags,
        TAGS_CAP,
        0,
        MAX_SUMMARIES,
        &item.tags,
        &item.content,
        &item.tags,
        false,
    );
    let tags_snapshot = util::safe_copy(&normalized_tags, TAGS_CAP);

    let mut normalized_keywords = String::new();
    let keyword_count = append_csv_terms(
        &mut normalized_keywords,
        KEYWORDS_CAP,
        0,
        MAX_SUMMARIES,
        &item.keywords,
        &item.content,
        &tags_snapshot,
        true,
    );
    if keyword_count < MAX_SUMMARIES {
        append_csv_terms(
            &mut normalized_keywords,
            KEYWORDS_CAP,
            keyword_count,
            MAX_SUMMARIES,
            &tags_snapshot,
            &item.content,
            &tags_snapshot,
            false,
        );
    }

    item.tags = tags_snapshot;
    item.keywords = util::safe_copy(&normalized_keywords, KEYWORDS_CAP);
}

/// `claw_memory_collect_summary_labels`.
pub fn collect_summary_labels(item: &MemItem) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let tag_copy = util::safe_copy(&item.tags, 128);
    for token in util::csv_split(&tag_copy) {
        if labels.len() >= MAX_SUMMARIES {
            break;
        }
        let trimmed = util::trim_whitespace(token);
        let limited = util::utf8_copy_chars(trimmed, MAX_LABEL_TEXT, MAX_LABEL_CHARS);
        if limited.is_empty() {
            continue;
        }
        if labels.iter().any(|l| l == &limited) {
            continue;
        }
        labels.push(limited);
    }
    labels
}

/// `claw_memory_item_primary_summary_label`.
pub fn primary_summary_label(item: &MemItem) -> Option<String> {
    let labels = collect_summary_labels(item);
    match labels.into_iter().next() {
        Some(l) if !l.is_empty() => Some(l),
        _ => None,
    }
}

/// `claw_memory_append_item_summary_labels`: merge this item's labels into a
/// newline list. Returns an `EspErr` only on the (unreachable) empty-label
/// case, mirroring `line_list_append_unique`.
pub fn append_item_summary_labels(
    item: &MemItem,
    out_summary_list: &mut Option<String>,
) -> claw_interfaces::error::EspErr {
    use claw_interfaces::error::ESP_OK;
    for label in collect_summary_labels(item) {
        let err = util::line_list_append_unique(out_summary_list, &label);
        if err != ESP_OK {
            return err;
        }
    }
    ESP_OK
}
