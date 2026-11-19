// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable forget records in `memory/forgotten.jsonl`.
//!
//! The file lives beside the graph, not inside it, so a rebuild that swaps
//! `graph/` does not drop the decision. The match key is the folded subject,
//! the folded relation or key, and the value compared the way a same-fact
//! write already compares values. Subject plus value is not a match.
//!
//! A slot record, `kind: "slot"`, names the subject and the relation or key
//! and blocks every value of that pair. It is loaded only when
//! `graph.slot_forget` is on together with `graph.lasting_forget`. A leftover
//! slot row is ignored when slot forget is off.
//!
//! Writes take `memory/.forgotten.lock`, sweep stale temps, and publish a
//! `0600` temp with `sync_data` before `rename`, so a crash cannot leave a
//! half-written file as the record. `sync_data` runs before the rename: a
//! crash after the rename must not publish bytes that never reached disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use super::aliases::fold;
use super::v2::known_match::value_spans;
use super::v2::values_equal;
use crate::error::RecallError;

const FILE_NAME: &str = "forgotten.jsonl";
const LOCK_NAME: &str = ".forgotten.lock";
const TEMP_PREFIX: &str = ".forgotten.jsonl.";
const RECORD_VERSION: u64 = 1;
const PRIVATE: u32 = 0o600;
const MASK: &str = "[forgotten]";
const PROVENANCE: &str = "user";

/// `memory/forgotten.jsonl`.
#[must_use]
pub(crate) fn forgotten_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join(FILE_NAME)
}

/// `memory/.forgotten.lock`.
#[must_use]
pub(crate) fn forgotten_lock_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join(LOCK_NAME)
}

/// Whether `[graph] lasting_forget` is on. Does not open the forget file.
#[must_use]
pub(crate) fn lasting_forget_on(memory_dir: &Path) -> bool {
    graph_flags(memory_dir).lasting_forget
}

/// Whether slot forget applies. True only when both flags are on.
///
/// Does not open the forget file, and does not turn lasting forget on.
#[must_use]
pub(crate) fn slot_forget_on(memory_dir: &Path) -> bool {
    let flags = graph_flags(memory_dir);
    flags.lasting_forget && flags.slot_forget
}

fn graph_flags(memory_dir: &Path) -> crate::config::GraphSection {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
}

/// One JSONL row. `kind` may be absent on input when `relation` and `value` are present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ForgetRecord {
    pub v: u32,
    pub status: RecordStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RecordKind>,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Deleted objects, kept so a free-text read can mask them. Slot rows only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    pub forgotten_at: String,
    pub provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unforgotten_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RecordStatus {
    Active,
    Inactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RecordKind {
    Fact,
    Entity,
    Slot,
}

/// Live records only, keyed for match and mask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ForgetSet {
    facts: Vec<LiveFact>,
    entities: Vec<String>,
    slots: Vec<LiveSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveFact {
    subject: String,
    relation: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveSlot {
    subject: String,
    relation: String,
    values: Vec<String>,
}

impl ForgetSet {
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.entities.is_empty() && self.slots.is_empty()
    }

    /// Live fact rows, live entity rows, and live slot rows.
    #[must_use]
    pub(crate) fn live_count(&self) -> usize {
        self.facts.len() + self.entities.len() + self.slots.len()
    }
}

/// An active fact row. The subject is stored as given; matching folds it.
#[must_use]
pub(crate) fn fact_record(
    subject: &str,
    relation: &str,
    value: &str,
    forgotten_at: &str,
) -> ForgetRecord {
    ForgetRecord {
        v: 1,
        status: RecordStatus::Active,
        kind: Some(RecordKind::Fact),
        subject: subject.to_string(),
        relation: Some(relation.to_string()),
        value: Some(value.to_string()),
        values: None,
        forgotten_at: forgotten_at.to_string(),
        provenance: PROVENANCE.to_string(),
        entity_id: None,
        edge_id: None,
        unforgotten_at: None,
    }
}

/// An active entity row. The subject is the folded name.
#[must_use]
pub(crate) fn entity_record(subject: &str, forgotten_at: &str) -> ForgetRecord {
    ForgetRecord {
        v: 1,
        status: RecordStatus::Active,
        kind: Some(RecordKind::Entity),
        subject: fold(subject),
        relation: None,
        value: None,
        values: None,
        forgotten_at: forgotten_at.to_string(),
        provenance: PROVENANCE.to_string(),
        entity_id: None,
        edge_id: None,
        unforgotten_at: None,
    }
}

/// An active slot row. The subject and relation are stored as given.
#[must_use]
pub(crate) fn slot_record(
    subject: &str,
    relation: &str,
    values: &[&str],
    forgotten_at: &str,
) -> ForgetRecord {
    ForgetRecord {
        v: 1,
        status: RecordStatus::Active,
        kind: Some(RecordKind::Slot),
        subject: subject.to_string(),
        relation: Some(relation.to_string()),
        value: None,
        values: snapshot_values(values),
        forgotten_at: forgotten_at.to_string(),
        provenance: PROVENANCE.to_string(),
        entity_id: None,
        edge_id: None,
        unforgotten_at: None,
    }
}

fn snapshot_values(values: &[&str]) -> Option<Vec<String>> {
    let kept: Vec<String> = values
        .iter()
        .filter(|value| !value.is_empty())
        .map(|value| (*value).to_string())
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept)
    }
}

/// `None` when `n` is zero, so a report can omit the counter.
///
/// The report field uses the same zero check. This function is the shared form.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn forgotten_skips_field(n: u32) -> Option<u32> {
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// Load live records. Flag off returns an empty set and does not touch the file.
pub(crate) fn load_forget_set(memory_dir: &Path, flag_on: bool) -> Result<ForgetSet, RecallError> {
    if !flag_on {
        return Ok(ForgetSet::default());
    }
    // `flag_on` is the lasting-forget decision. Slot rows join the set only
    // when both flags are on.
    Ok(live_set_with(
        &load_records(memory_dir)?,
        slot_forget_on(memory_dir),
    ))
}

/// What `--unforget` selects. History stays in the file.
pub(crate) enum UnforgetSelector {
    Fact {
        subject: String,
        relation: String,
        value: String,
    },
    Entity {
        subject: String,
    },
    Slot {
        subject: String,
        relation: String,
    },
}

/// Exclusive lock on `memory/.forgotten.lock`. Dropping it releases the lock.
pub(crate) struct ForgetLock {
    _file: File,
}

/// Open the sibling lock and wait up to `wait`.
pub(crate) fn lock_forgotten(memory_dir: &Path, wait: Duration) -> Result<ForgetLock, RecallError> {
    let path = forgotten_lock_path(memory_dir);
    let file = open_lock(&path)?;
    crate::file_lock::lock_exclusive(&file, &path, wait)?;
    Ok(ForgetLock { _file: file })
}

/// How long a forget waits for `memory/.forgotten.lock`.
///
/// Tests can shorten it. Production always waits [`crate::file_lock::LOCK_WAIT`].
#[must_use]
pub(crate) fn forget_lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = WAIT_OVERRIDE.with(std::cell::Cell::get) {
        return wait;
    }
    crate::file_lock::LOCK_WAIT
}

#[cfg(test)]
thread_local! {
    static WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// Restores the previous test wait when dropped. In-process only.
#[cfg(test)]
pub(crate) struct ForgetWaitOverride {
    previous: Option<Duration>,
}

#[cfg(test)]
impl ForgetWaitOverride {
    pub(crate) fn at(wait: Duration) -> Self {
        let previous = WAIT_OVERRIDE.with(|cell| cell.replace(Some(wait)));
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for ForgetWaitOverride {
    fn drop(&mut self) {
        let previous = self.previous.take();
        WAIT_OVERRIDE.with(|cell| cell.set(previous));
    }
}

/// Parse the forget file while `lock` is held. Missing is empty.
///
/// A malformed file is an error, so the caller can refuse before any delete.
pub(crate) fn ensure_readable(memory_dir: &Path, _lock: &ForgetLock) -> Result<(), RecallError> {
    load_records(memory_dir).map(|_| ())
}

/// Lock, then append. An empty slice does not create the file or the lock.
///
/// Confirmed forget holds the lock across the delete and calls [`append_locked`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn append_forget_records(
    memory_dir: &Path,
    records: &[ForgetRecord],
    wait: Duration,
) -> Result<(), RecallError> {
    if records.is_empty() {
        return Ok(());
    }
    let lock = lock_forgotten(memory_dir, wait)?;
    append_locked(memory_dir, &lock, records)
}

/// Append while `lock` is already held. Does not take the lock again.
pub(crate) fn append_locked(
    memory_dir: &Path,
    _lock: &ForgetLock,
    records: &[ForgetRecord],
) -> Result<(), RecallError> {
    if records.is_empty() {
        return Ok(());
    }
    let path = forgotten_path(memory_dir);
    let mut existing = load_records(memory_dir)?;
    existing.extend(records.iter().cloned());
    write_records(&path, &existing)
}

/// Live rows `--unforget` would deactivate. Does not create or lock the file.
pub(crate) fn matching_live(
    memory_dir: &Path,
    selector: &UnforgetSelector,
) -> Result<Vec<ForgetRecord>, RecallError> {
    let records = load_records(memory_dir)?;
    Ok(records
        .into_iter()
        .filter(|record| live_selected(record, selector))
        .collect())
}

fn live_selected(record: &ForgetRecord, selector: &UnforgetSelector) -> bool {
    record.status == RecordStatus::Active && selected(record, selector)
}

/// One entity row for the folded name, plus one fact row per removed edge.
#[must_use]
pub(crate) fn records_for_removal(removal: &super::correct::Removal) -> Vec<ForgetRecord> {
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut records = Vec::new();
    push_entity_record(&mut records, removal, &at);
    push_edge_records(&mut records, removal, &at);
    records
}

fn push_entity_record(
    records: &mut Vec<ForgetRecord>,
    removal: &super::correct::Removal,
    at: &str,
) {
    let Some(entity) = removal.entity.as_ref() else {
        return;
    };
    let mut record = entity_record(&entity.name, at);
    record.entity_id = Some(entity.id.clone());
    records.push(record);
}

fn push_edge_records(records: &mut Vec<ForgetRecord>, removal: &super::correct::Removal, at: &str) {
    for edge in &removal.edges {
        let mut record = fact_record(&edge.from, &edge.rel_type, &edge.to, at);
        record.edge_id = Some(edge.id.clone());
        records.push(record);
    }
}

/// Mark the selected live rows inactive. Returns how many changed.
pub(crate) fn unforget_records(
    memory_dir: &Path,
    selector: &UnforgetSelector,
    wait: Duration,
) -> Result<usize, RecallError> {
    let lock = lock_forgotten(memory_dir, wait)?;
    unforget_locked(memory_dir, &lock, selector)
}

fn unforget_locked(
    memory_dir: &Path,
    _lock: &ForgetLock,
    selector: &UnforgetSelector,
) -> Result<usize, RecallError> {
    let path = forgotten_path(memory_dir);
    let mut records = load_records(memory_dir)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let count = deactivate(&mut records, selector, &now);
    if count == 0 {
        return Ok(0);
    }
    write_records(&path, &records)?;
    Ok(count)
}

/// Exact live fact: folded subject, folded relation or key, same-fact value.
#[must_use]
pub(crate) fn is_forgotten_fact(
    set: &ForgetSet,
    subject: &str,
    relation: &str,
    value: &str,
) -> bool {
    let subject = fold(subject);
    let relation = fold(relation);
    set.facts
        .iter()
        .any(|fact| fact_key_hit(fact, &subject, &relation, value))
}

fn fact_key_hit(fact: &LiveFact, subject: &str, relation: &str, value: &str) -> bool {
    fact.subject == subject && fact.relation == relation && values_equal(&fact.value, value)
}

/// A live slot blocks any value of this subject and relation or key.
///
/// `setting:timeout` and `timeout` are the same relation. The `setting:` form
/// is the one a setting key already uses; nothing else is rewritten.
#[must_use]
pub(crate) fn is_forgotten_slot(set: &ForgetSet, subject: &str, relation: &str) -> bool {
    let subject = fold(subject);
    set.slots
        .iter()
        .any(|slot| slot.subject == subject && same_relation(&slot.relation, relation))
}

/// Folded equality, plus the `setting:<key>` form either side may use.
fn same_relation(stored: &str, query: &str) -> bool {
    let stored = fold(stored);
    let query = fold(query);
    if stored == query {
        return true;
    }
    setting_key(&stored).is_some_and(|key| key == query)
        || setting_key(&query).is_some_and(|key| key == stored)
}

fn setting_key(relation: &str) -> Option<&str> {
    let key = relation.strip_prefix("setting:")?;
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// Folded subject against a live entity row.
#[must_use]
pub(crate) fn is_forgotten_entity(set: &ForgetSet, subject: &str) -> bool {
    let subject = fold(subject);
    set.entities.iter().any(|name| name == &subject)
}

/// A manual `add-entity` or `relate` the forget file refuses.
pub(crate) enum ManualWrite<'a> {
    AddEntity(&'a str),
    Relate {
        from: &'a str,
        relation: &'a str,
        to: &'a str,
    },
}

/// `Err` names `--unforget` when the write would recreate a live forget.
pub(crate) fn refuse_manual_write(
    set: &ForgetSet,
    write: ManualWrite<'_>,
) -> Result<(), RecallError> {
    match blocked_name(set, write) {
        Some(name) => Err(manual_refusal(name)),
        None => Ok(()),
    }
}

fn blocked_name<'a>(set: &ForgetSet, write: ManualWrite<'a>) -> Option<&'a str> {
    match write {
        ManualWrite::AddEntity(name) => is_forgotten_entity(set, name).then_some(name),
        ManualWrite::Relate { from, relation, to } => relate_block(set, from, relation, to),
    }
}

fn relate_block<'a>(
    set: &ForgetSet,
    from: &'a str,
    relation: &'a str,
    to: &'a str,
) -> Option<&'a str> {
    if is_forgotten_entity(set, from) {
        return Some(from);
    }
    if is_forgotten_entity(set, to) {
        return Some(to);
    }
    if is_forgotten_slot(set, from, relation) {
        return Some(from);
    }
    if is_forgotten_fact(set, from, relation, to) {
        return Some(from);
    }
    None
}

fn manual_refusal(name: &str) -> RecallError {
    RecallError::Other(format!(
        "\"{name}\" is forgotten; graph correct --unforget restores it before it can be written again"
    ))
}

/// Mask text fields of a graph response. An empty set returns `value` unchanged.
#[must_use]
pub(crate) fn mask_json(set: &ForgetSet, value: serde_json::Value) -> serde_json::Value {
    if set.facts.is_empty() && set.slots.is_empty() {
        return value;
    }
    mask_json_value(set, value, false)
}

fn mask_json_value(
    set: &ForgetSet,
    value: serde_json::Value,
    mask_text: bool,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) if mask_text => {
            serde_json::Value::String(mask_forgotten(&text, set))
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(mask_object(set, map)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(mask_array(set, items, mask_text))
        }
        other => other,
    }
}

fn mask_object(
    set: &ForgetSet,
    map: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let slot_hit = object_slot_hit(set, &map);
    map.into_iter()
        .map(|(key, value)| {
            if slot_hit && value_field(&key) {
                return (key, mask_slot_value(set, value));
            }
            let mask_text = text_field(&key);
            (key, mask_json_value(set, value, mask_text))
        })
        .collect()
}

fn object_slot_hit(set: &ForgetSet, map: &serde_json::Map<String, serde_json::Value>) -> bool {
    if set.slots.is_empty() {
        return false;
    }
    let Some(subject) = first_text(map, &["from", "subject", "subject_name"]) else {
        return false;
    };
    let relation = first_text(map, &["rel_type", "relation", "predicate"]).unwrap_or("");
    let key = first_text(map, &["key"]).unwrap_or("");
    if !relation.is_empty() && is_forgotten_slot(set, subject, relation) {
        return true;
    }
    if !key.is_empty() && is_forgotten_slot(set, subject, key) {
        return true;
    }
    !relation.is_empty()
        && !key.is_empty()
        && is_forgotten_slot(set, subject, &format!("{relation}:{key}"))
}

fn first_text<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
}

fn value_field(key: &str) -> bool {
    matches!(key, "to" | "value" | "object" | "object_name")
}

fn mask_slot_value(set: &ForgetSet, value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(_) => serde_json::Value::String(MASK.to_string()),
        other => mask_json_value(set, other, false),
    }
}

fn mask_array(
    set: &ForgetSet,
    items: Vec<serde_json::Value>,
    mask_text: bool,
) -> Vec<serde_json::Value> {
    items
        .into_iter()
        .map(|item| mask_json_value(set, item, mask_text))
        .collect()
}

fn text_field(key: &str) -> bool {
    matches!(
        key,
        "abstract_text"
            | "content"
            | "overview"
            | "description"
            | "quote"
            | "text"
            | "summary"
            | "line"
            | "value"
            | "name"
            | "from"
            | "to"
    )
}

/// The rendered text, plus a values-free count when the flag is on and live records exist.
pub(crate) fn finish_read_text(memory_dir: &Path, text: String) -> Result<String, RecallError> {
    if !lasting_forget_on(memory_dir) {
        return Ok(text);
    }
    let set = load_forget_set(memory_dir, true)?;
    Ok(finish_with_set(&set, text))
}

fn finish_with_set(set: &ForgetSet, text: String) -> String {
    let mut masked = mask_forgotten(&text, set);
    let count = set.live_count();
    if count > 0 {
        masked.push_str(&format!("\n  Forgotten: {count}\n"));
    }
    masked
}

/// Loaded mask, or nothing when the flag is off (no read of the forget file).
pub(crate) fn read_mask(memory_dir: &Path) -> Result<Option<ForgetSet>, RecallError> {
    if !lasting_forget_on(memory_dir) {
        return Ok(None);
    }
    let set = load_forget_set(memory_dir, true)?;
    Ok(Some(set))
}

#[must_use]
pub(crate) fn apply_mask(set: Option<&ForgetSet>, text: &str) -> String {
    match set {
        Some(set) => mask_forgotten(text, set),
        None => text.to_string(),
    }
}

/// Replace each live fact value with `[forgotten]`. Entity names are not masked.
#[must_use]
pub(crate) fn mask_forgotten(text: &str, set: &ForgetSet) -> String {
    if text.is_empty() || (set.facts.is_empty() && !slot_values_exist(set)) {
        return text.to_string();
    }
    let spans = forgotten_spans(text, set);
    if spans.is_empty() {
        return text.to_string();
    }
    paint_spans(text, &spans)
}

fn slot_values_exist(set: &ForgetSet) -> bool {
    set.slots.iter().any(|slot| !slot.values.is_empty())
}

fn forgotten_spans(text: &str, set: &ForgetSet) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for fact in &set.facts {
        spans.extend(value_spans(text, &fact.value));
    }
    for slot in &set.slots {
        for value in &slot.values {
            spans.extend(value_spans(text, value));
        }
    }
    merge_spans(spans)
}

fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_by_key(|span| (span.0, span.1));
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(current) if span.0 <= current.1 => current.1 = current.1.max(span.1),
            _ => merged.push(span),
        }
    }
    merged
}

fn paint_spans(text: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for &(start, end) in spans {
        if !span_in_range(text, cursor, start, end) {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(MASK);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn span_in_range(text: &str, cursor: usize, start: usize, end: usize) -> bool {
    start >= cursor && end <= text.len() && start < end
}

#[cfg(test)]
fn live_set(records: &[ForgetRecord]) -> ForgetSet {
    live_set_with(records, true)
}

fn live_set_with(records: &[ForgetRecord], include_slots: bool) -> ForgetSet {
    let mut set = ForgetSet::default();
    for record in records {
        if record.status == RecordStatus::Active {
            push_live(record, &mut set, include_slots);
        }
    }
    set
}

fn push_live(record: &ForgetRecord, set: &mut ForgetSet, include_slots: bool) {
    match record.kind {
        Some(RecordKind::Entity) => set.entities.push(fold(&record.subject)),
        Some(RecordKind::Slot) if include_slots => push_live_slot(record, set),
        Some(RecordKind::Slot) => {}
        Some(RecordKind::Fact) | None => push_live_fact(record, set),
    }
}

fn push_live_slot(record: &ForgetRecord, set: &mut ForgetSet) {
    let Some(relation) = record.relation.as_deref() else {
        return;
    };
    set.slots.push(LiveSlot {
        subject: fold(&record.subject),
        relation: fold(relation),
        values: record.values.clone().unwrap_or_default(),
    });
}

fn push_live_fact(record: &ForgetRecord, set: &mut ForgetSet) {
    let (Some(relation), Some(value)) = (record.relation.as_deref(), record.value.as_deref())
    else {
        return;
    };
    set.facts.push(LiveFact {
        subject: fold(&record.subject),
        relation: fold(relation),
        value: value.to_string(),
    });
}

fn deactivate(records: &mut [ForgetRecord], selector: &UnforgetSelector, now: &str) -> usize {
    let mut count = 0;
    for record in records.iter_mut() {
        if record.status == RecordStatus::Active && selected(record, selector) {
            record.status = RecordStatus::Inactive;
            record.unforgotten_at = Some(now.to_string());
            count += 1;
        }
    }
    count
}

fn selected(record: &ForgetRecord, selector: &UnforgetSelector) -> bool {
    match selector {
        UnforgetSelector::Fact {
            subject,
            relation,
            value,
        } => fact_selected(record, subject, relation, value),
        UnforgetSelector::Entity { subject } => {
            record.kind != Some(RecordKind::Slot) && fold(&record.subject) == fold(subject)
        }
        UnforgetSelector::Slot { subject, relation } => slot_selected(record, subject, relation),
    }
}

fn slot_selected(record: &ForgetRecord, subject: &str, relation: &str) -> bool {
    record.kind == Some(RecordKind::Slot)
        && fold(&record.subject) == fold(subject)
        && record
            .relation
            .as_deref()
            .is_some_and(|stored| same_relation(stored, relation))
}

fn fact_selected(record: &ForgetRecord, subject: &str, relation: &str, value: &str) -> bool {
    if matches!(
        record.kind,
        Some(RecordKind::Entity) | Some(RecordKind::Slot)
    ) {
        return false;
    }
    fold(&record.subject) == fold(subject)
        && relation_matches(record.relation.as_deref(), relation)
        && value_matches(record.value.as_deref(), value)
}

fn relation_matches(stored: Option<&str>, query: &str) -> bool {
    stored.is_some_and(|relation| fold(relation) == fold(query))
}

fn value_matches(stored: Option<&str>, query: &str) -> bool {
    stored.is_some_and(|value| values_equal(value, query))
}

fn load_records(memory_dir: &Path) -> Result<Vec<ForgetRecord>, RecallError> {
    let path = forgotten_path(memory_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(unreadable(&path, &err)),
    };
    parse_records(&path, &text)
}

fn parse_records(path: &Path, text: &str) -> Result<Vec<ForgetRecord>, RecallError> {
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = parse_line(line).map_err(|fail| malformed(path, index + 1, fail))?;
        records.push(record);
    }
    Ok(records)
}

enum LineFail {
    InvalidJson,
    Missing,
    Version,
}

fn parse_line(line: &str) -> Result<ForgetRecord, LineFail> {
    let value: Value = serde_json::from_str(line).map_err(|_| LineFail::InvalidJson)?;
    let obj = value.as_object().ok_or(LineFail::InvalidJson)?;
    record_from_object(obj)
}

struct RecordHead {
    status: RecordStatus,
    kind: RecordKind,
    subject: String,
    forgotten_at: String,
    provenance: String,
}

struct RecordBody {
    relation: Option<String>,
    value: Option<String>,
    values: Option<Vec<String>>,
    entity_id: Option<String>,
    edge_id: Option<String>,
    unforgotten_at: Option<String>,
}

fn record_from_object(obj: &Map<String, Value>) -> Result<ForgetRecord, LineFail> {
    let head = record_head(obj)?;
    let body = record_body(obj, head.kind)?;
    Ok(ForgetRecord {
        v: 1,
        status: head.status,
        kind: Some(head.kind),
        subject: head.subject,
        relation: body.relation,
        value: body.value,
        values: body.values,
        forgotten_at: head.forgotten_at,
        provenance: head.provenance,
        entity_id: body.entity_id,
        edge_id: body.edge_id,
        unforgotten_at: body.unforgotten_at,
    })
}

fn record_head(obj: &Map<String, Value>) -> Result<RecordHead, LineFail> {
    require_version(obj)?;
    Ok(RecordHead {
        status: require_status(obj)?,
        kind: require_kind(obj)?,
        subject: require_string(obj, "subject")?.to_string(),
        forgotten_at: require_time(obj, "forgotten_at")?,
        provenance: require_provenance(obj)?,
    })
}

fn record_body(obj: &Map<String, Value>, kind: RecordKind) -> Result<RecordBody, LineFail> {
    let parsed = relation_value(obj, kind)?;
    Ok(RecordBody {
        relation: parsed.relation,
        value: parsed.value,
        values: parsed.values,
        entity_id: optional_string(obj, "entity_id")?,
        edge_id: optional_string(obj, "edge_id")?,
        unforgotten_at: optional_time(obj, "unforgotten_at")?,
    })
}

fn require_version(obj: &Map<String, Value>) -> Result<(), LineFail> {
    match obj.get("v") {
        None => Err(LineFail::Missing),
        Some(value) if value.as_u64() == Some(RECORD_VERSION) => Ok(()),
        Some(_) => Err(LineFail::Version),
    }
}

fn require_status(obj: &Map<String, Value>) -> Result<RecordStatus, LineFail> {
    match require_string(obj, "status")? {
        "active" => Ok(RecordStatus::Active),
        "inactive" => Ok(RecordStatus::Inactive),
        _ => Err(LineFail::InvalidJson),
    }
}

fn require_kind(obj: &Map<String, Value>) -> Result<RecordKind, LineFail> {
    match obj.get("kind") {
        None => infer_kind(obj),
        Some(value) => named_kind(value),
    }
}

fn named_kind(value: &Value) -> Result<RecordKind, LineFail> {
    match value.as_str() {
        Some("fact") => Ok(RecordKind::Fact),
        Some("entity") => Ok(RecordKind::Entity),
        Some("slot") => Ok(RecordKind::Slot),
        _ => Err(LineFail::InvalidJson),
    }
}

fn infer_kind(obj: &Map<String, Value>) -> Result<RecordKind, LineFail> {
    if obj.get("relation").and_then(Value::as_str).is_some()
        && obj.get("value").and_then(Value::as_str).is_some()
    {
        Ok(RecordKind::Fact)
    } else {
        Err(LineFail::Missing)
    }
}

struct RelationValue {
    relation: Option<String>,
    value: Option<String>,
    values: Option<Vec<String>>,
}

fn relation_value(obj: &Map<String, Value>, kind: RecordKind) -> Result<RelationValue, LineFail> {
    match kind {
        RecordKind::Fact => Ok(RelationValue {
            relation: Some(require_string(obj, "relation")?.to_string()),
            value: Some(require_string(obj, "value")?.to_string()),
            values: None,
        }),
        RecordKind::Entity => entity_without_triple(obj),
        RecordKind::Slot => slot_relation(obj),
    }
}

fn entity_without_triple(obj: &Map<String, Value>) -> Result<RelationValue, LineFail> {
    if obj.contains_key("relation") || obj.contains_key("value") {
        return Err(LineFail::InvalidJson);
    }
    Ok(RelationValue {
        relation: None,
        value: None,
        values: None,
    })
}

fn slot_relation(obj: &Map<String, Value>) -> Result<RelationValue, LineFail> {
    if obj.contains_key("value") {
        return Err(LineFail::InvalidJson);
    }
    Ok(RelationValue {
        relation: Some(require_string(obj, "relation")?.to_string()),
        value: None,
        values: optional_values(obj)?,
    })
}

fn optional_values(obj: &Map<String, Value>) -> Result<Option<Vec<String>>, LineFail> {
    match obj.get("values") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(text) if !text.is_empty() => values.push(text.to_string()),
                    _ => return Err(LineFail::InvalidJson),
                }
            }
            if values.is_empty() {
                Ok(None)
            } else {
                Ok(Some(values))
            }
        }
        Some(_) => Err(LineFail::InvalidJson),
    }
}

fn require_provenance(obj: &Map<String, Value>) -> Result<String, LineFail> {
    match require_string(obj, "provenance")? {
        PROVENANCE => Ok(PROVENANCE.to_string()),
        _ => Err(LineFail::InvalidJson),
    }
}

fn require_time(obj: &Map<String, Value>, key: &str) -> Result<String, LineFail> {
    let text = require_string(obj, key)?;
    if chrono::DateTime::parse_from_rfc3339(text).is_err() {
        return Err(LineFail::InvalidJson);
    }
    Ok(text.to_string())
}

fn optional_time(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, LineFail> {
    match optional_string(obj, key)? {
        None => Ok(None),
        Some(text) if chrono::DateTime::parse_from_rfc3339(&text).is_ok() => Ok(Some(text)),
        Some(_) => Err(LineFail::InvalidJson),
    }
}

fn require_string<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a str, LineFail> {
    match obj.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text),
        Some(_) => Err(LineFail::InvalidJson),
        None => Err(LineFail::Missing),
    }
}

fn optional_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, LineFail> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.clone())),
        Some(_) => Err(LineFail::InvalidJson),
    }
}

fn malformed(path: &Path, line_no: usize, fail: LineFail) -> RecallError {
    let reason = match fail {
        LineFail::InvalidJson => "invalid JSON",
        LineFail::Missing => "missing required fields",
        LineFail::Version => "unsupported v",
    };
    RecallError::Other(format!("{}: {reason} at line {line_no}", path.display()))
}

fn unreadable(path: &Path, err: &io::Error) -> RecallError {
    RecallError::Other(format!("{}: unreadable: {err}", path.display()))
}

fn write_records(target: &Path, records: &[ForgetRecord]) -> Result<(), RecallError> {
    sweep_stale_temps(&parent_dir(target))?;
    replace_file(target, &render_records(records)?)
}

fn render_records(records: &[ForgetRecord]) -> Result<String, RecallError> {
    let mut out = String::new();
    for record in records {
        out.push_str(&serde_json::to_string(record)?);
        out.push('\n');
    }
    Ok(out)
}

fn replace_file(target: &Path, contents: &str) -> Result<(), RecallError> {
    let (path, mut file) = create_temp(target)?;
    let mut guard = TempGuard::arm(path);
    file.write_all(contents.as_bytes())?;
    // Publish only bytes that have reached disk. See the module note.
    file.sync_data()?;
    drop(file);
    fs::rename(&guard.path, target)?;
    guard.disarm();
    Ok(())
}

fn create_temp(target: &Path) -> Result<(PathBuf, File), RecallError> {
    let mut collision = None;
    for _ in 0..8 {
        let path = temp_path(target);
        match open_temp(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if is_collision(&err) => collision = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(collision.unwrap_or_else(|| {
        RecallError::Other(format!(
            "{}: could not create a private temp file",
            target.display()
        ))
    }))
}

fn is_collision(err: &RecallError) -> bool {
    matches!(err, RecallError::Io(io) if io.kind() == io::ErrorKind::AlreadyExists)
}

fn open_temp(path: &Path) -> Result<File, RecallError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE)
        .open(path)
        .map_err(RecallError::from)
}

fn temp_path(target: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let name = format!("{TEMP_PREFIX}{}.{n}.tmp", std::process::id());
    parent_dir(target).join(name)
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn sweep_stale_temps(dir: &Path) -> Result<(), RecallError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        remove_stale_temp(&entry?)?;
    }
    Ok(())
}

fn remove_stale_temp(entry: &fs::DirEntry) -> Result<(), RecallError> {
    let name = entry.file_name();
    if stale_temp_name(&name.to_string_lossy()) {
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn stale_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(TEMP_PREFIX) else {
        return false;
    };
    rest.ends_with(".tmp")
}

fn open_lock(path: &Path) -> Result<File, RecallError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE)
        .open(path)
        .map_err(|err| RecallError::Other(format!("{}: {err}", path.display())))?;
    ensure_private(&file)?;
    Ok(file)
}

fn ensure_private(file: &File) -> Result<(), RecallError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode == PRIVATE {
        return Ok(());
    }
    file.set_permissions(fs::Permissions::from_mode(PRIVATE))?;
    Ok(())
}

struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn arm(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const AT: &str = "2026-11-17T12:00:00Z";

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn sample_fact() -> ForgetRecord {
        fact_record("Ada", "uses", "Rust", AT)
    }

    fn assert_names_path(err: &RecallError, path: &Path) {
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn missing_and_empty_files_are_no_records() {
        let dir = tempfile::tempdir().expect("temp");
        let missing = load_forget_set(dir.path(), true).expect("missing");
        assert!(missing.is_empty());
        fs::write(forgotten_path(dir.path()), "\n\n").expect("empty");
        let empty = load_forget_set(dir.path(), true).expect("empty");
        assert!(empty.is_empty());
        assert!(!forgotten_lock_path(dir.path()).exists());
    }

    #[test]
    fn an_empty_append_creates_nothing() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(dir.path(), &[], Duration::from_secs(1)).expect("append");
        assert!(!forgotten_path(dir.path()).exists());
        assert!(!forgotten_lock_path(dir.path()).exists());
    }

    #[test]
    fn a_record_round_trips_and_the_files_are_private() {
        let dir = tempfile::tempdir().expect("temp");
        let stale = dir.path().join(".forgotten.jsonl.4.4.tmp");
        fs::write(&stale, "not a record").expect("stale");
        let entity = entity_record("  Vim ", AT);
        assert_eq!(entity.subject, "vim");
        append_forget_records(dir.path(), &[sample_fact(), entity], Duration::from_secs(1))
            .expect("append");
        assert!(!stale.exists(), "a stale temp is not the file");
        assert_eq!(mode_of(&forgotten_path(dir.path())), PRIVATE);
        assert_eq!(mode_of(&forgotten_lock_path(dir.path())), PRIVATE);
        assert!(dir
            .path()
            .read_dir()
            .expect("dir")
            .flatten()
            .all(|entry| !stale_temp_name(&entry.file_name().to_string_lossy())));

        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert!(is_forgotten_entity(&set, "VIM"));
        assert_eq!(set.live_count(), 2);
        let text = fs::read_to_string(forgotten_path(dir.path())).expect("file");
        assert!(text.contains("\"provenance\":\"user\""));
        assert!(text.contains("\"v\":1"));
        assert!(text.contains("\"subject\":\"Ada\""));
    }

    #[test]
    fn subject_and_value_without_the_relation_does_not_match() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(dir.path(), &[sample_fact()], Duration::from_secs(1))
            .expect("append");
        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(is_forgotten_fact(&set, "  ADA ", "USES", "rust"));
        assert!(
            !is_forgotten_fact(&set, "Ada", "likes", "Rust"),
            "the same subject and value under another relation is a different fact"
        );
        assert!(!is_forgotten_fact(&set, "Ada", "uses", "Go"));
        assert!(!is_forgotten_fact(&set, "Bea", "uses", "Rust"));
        assert!(!is_forgotten_entity(&set, "Ada"));
    }

    #[test]
    fn a_folded_subject_matches_and_a_different_subject_does_not() {
        let set = live_set(&[fact_record("Ada Lovelace", "works_on", "Rust", AT)]);
        assert!(is_forgotten_fact(
            &set,
            "ada   lovelace",
            "works_on",
            "Rust"
        ));
        assert!(!is_forgotten_fact(&set, "Ada", "works_on", "Rust"));
    }

    #[test]
    fn a_different_value_for_the_same_subject_and_relation_is_kept() {
        let set = live_set(&[sample_fact()]);
        assert!(!is_forgotten_fact(&set, "Ada", "uses", "Go"));
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert!(!values_equal("10s", "10ms"));
        assert!(values_equal("10s", "10S"));
    }

    #[test]
    fn blank_lines_are_ignored_and_kind_can_be_omitted() {
        let dir = tempfile::tempdir().expect("temp");
        let line = concat!(
            "\n{\"v\":1,\"status\":\"active\",\"subject\":\"Ada\",\"relation\":\"uses\",",
            "\"value\":\"Rust\",\"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n\n"
        );
        fs::write(forgotten_path(dir.path()), line).expect("write");
        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert_eq!(set.live_count(), 1);
    }

    #[test]
    fn a_malformed_file_names_the_path() {
        let dir = tempfile::tempdir().expect("temp");
        let path = forgotten_path(dir.path());
        let cases = [
            ("not-json\n", "invalid JSON"),
            ("{\"v\":1}\n", "missing required fields"),
            (
                "{\"v\":2,\"status\":\"active\",\"kind\":\"entity\",\"subject\":\"Ada\",\"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n",
                "unsupported v",
            ),
        ];
        for (body, reason) in cases {
            fs::write(&path, body).expect("write");
            let err = load_forget_set(dir.path(), true).expect_err("malformed");
            let message = err.to_string();
            assert_names_path(&err, &path);
            assert!(message.contains(reason), "{message}");
        }
        fs::remove_file(&path).expect("remove");
        fs::create_dir(&path).expect("dir");
        let err = load_forget_set(dir.path(), true).expect_err("unreadable");
        let message = err.to_string();
        assert_names_path(&err, &path);
        assert!(message.contains("unreadable"), "{message}");
    }

    #[test]
    fn flag_off_load_ignores_a_malformed_file() {
        let dir = tempfile::tempdir().expect("temp");
        let path = forgotten_path(dir.path());
        fs::write(&path, b"{not json\n").expect("write");
        let before = fs::read(&path).expect("bytes");
        let set = load_forget_set(dir.path(), false).expect("flag off");
        assert!(set.is_empty());
        assert_eq!(fs::read(&path).expect("bytes"), before);
        let missing = dir.path().join("absent");
        let set = load_forget_set(&missing, false).expect("no dir");
        assert!(set.is_empty());
        assert!(!missing.exists());
    }

    #[test]
    fn lasting_forget_on_reads_the_flag_and_not_the_file() {
        let dir = tempfile::tempdir().expect("temp");
        fs::write(forgotten_path(dir.path()), b"{not json\n").expect("bad");
        assert!(!lasting_forget_on(dir.path()));
        fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nlasting_forget = true\n",
        )
        .expect("config");
        assert!(lasting_forget_on(dir.path()));
        let set = load_forget_set(dir.path(), false).expect("still off when told");
        assert!(set.is_empty());
    }

    #[test]
    fn a_held_lock_times_out_and_leaves_the_file() {
        let dir = tempfile::tempdir().expect("temp");
        let lock_path = forgotten_lock_path(dir.path());
        fs::write(&lock_path, b"").expect("lock");
        let file = forgotten_path(dir.path());
        fs::write(&file, b"keep\n").expect("file");
        let held = crate::file_lock::HeldLock::on(&lock_path);
        let err = append_forget_records(dir.path(), &[sample_fact()], Duration::ZERO)
            .expect_err("timeout");
        let message = err.to_string();
        assert!(
            message.contains(&lock_path.display().to_string()),
            "{message}"
        );
        assert!(message.contains("locked"), "{message}");
        assert_eq!(fs::read(&file).expect("bytes"), b"keep\n");
        drop(held);
    }

    #[test]
    fn a_temp_file_is_created_at_mode_0600() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join(".forgotten.jsonl.temp-check.tmp");
        let file = open_temp(&path).expect("temp");
        drop(file);
        assert_eq!(mode_of(&path), PRIVATE);
    }

    #[test]
    fn an_unforget_keeps_history_and_drops_only_the_selected_key() {
        let dir = tempfile::tempdir().expect("temp");
        let likes = fact_record("Ada", "likes", "Rust", AT);
        append_forget_records(dir.path(), &[sample_fact(), likes], Duration::from_secs(1))
            .expect("append");
        let changed = unforget_records(
            dir.path(),
            &UnforgetSelector::Fact {
                subject: "ada".into(),
                relation: "USES".into(),
                value: "rust".into(),
            },
            Duration::from_secs(1),
        )
        .expect("unforget");
        assert_eq!(changed, 1);
        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(!is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert!(is_forgotten_fact(&set, "Ada", "likes", "Rust"));
        let text = fs::read_to_string(forgotten_path(dir.path())).expect("file");
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"status\":\"inactive\""));
        assert!(text.contains("\"unforgotten_at\""));
        assert_eq!(mode_of(&forgotten_path(dir.path())), PRIVATE);
        append_forget_records(dir.path(), &[sample_fact()], Duration::from_secs(1)).expect("again");
        let set = load_forget_set(dir.path(), true).expect("reload");
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
    }

    #[test]
    fn an_entity_unforget_clears_that_subject_and_keeps_other_facts() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(
            dir.path(),
            &[
                entity_record("Ada", AT),
                fact_record("Ada", "uses", "Rust", AT),
                fact_record("Bea", "uses", "Rust", AT),
            ],
            Duration::from_secs(1),
        )
        .expect("append");
        let changed = unforget_records(
            dir.path(),
            &UnforgetSelector::Entity {
                subject: "ADA".into(),
            },
            Duration::from_secs(1),
        )
        .expect("unforget");
        assert_eq!(changed, 2);
        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(!is_forgotten_entity(&set, "Ada"));
        assert!(!is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert!(is_forgotten_fact(&set, "Bea", "uses", "Rust"));
        let text = fs::read_to_string(forgotten_path(dir.path())).expect("file");
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn a_manual_write_of_a_forgotten_name_names_unforget() {
        let set = live_set(&[
            entity_record("Ada", AT),
            fact_record("Bea", "uses", "Rust", AT),
        ]);
        let err = refuse_manual_write(&set, ManualWrite::AddEntity("ada")).expect_err("entity");
        assert!(err.to_string().contains("--unforget"), "{err}");
        assert!(refuse_manual_write(&set, ManualWrite::AddEntity("Bea")).is_ok());
        let from = refuse_manual_write(
            &set,
            ManualWrite::Relate {
                from: "Ada",
                relation: "likes",
                to: "Go",
            },
        )
        .expect_err("from");
        assert!(from.to_string().contains("--unforget"));
        let to = refuse_manual_write(
            &set,
            ManualWrite::Relate {
                from: "Cleo",
                relation: "likes",
                to: "Ada",
            },
        )
        .expect_err("to");
        assert!(to.to_string().contains("--unforget"));
        let fact = refuse_manual_write(
            &set,
            ManualWrite::Relate {
                from: "Bea",
                relation: "uses",
                to: "Rust",
            },
        )
        .expect_err("fact");
        assert!(fact.to_string().contains("--unforget"));
        assert!(refuse_manual_write(
            &set,
            ManualWrite::Relate {
                from: "Bea",
                relation: "likes",
                to: "Rust",
            },
        )
        .is_ok());
    }

    #[test]
    fn mask_replaces_a_live_value_and_not_an_entity_name() {
        let facts = live_set(&[fact_record("Ada", "uses", "Rust", AT)]);
        assert_eq!(
            mask_forgotten("Ada uses Rust.", &facts),
            "Ada uses [forgotten]."
        );
        assert_eq!(mask_forgotten("see rust!", &facts), "see [forgotten]!");
        assert_eq!(mask_forgotten("Go stays", &facts), "Go stays");
        let entity = live_set(&[entity_record("Vim", AT)]);
        assert_eq!(mask_forgotten("Vim is here", &entity), "Vim is here");
        assert!(entity.facts.is_empty());
    }

    #[test]
    fn records_for_a_removal_keep_the_entity_and_each_edge() {
        use super::super::correct::{EntityMatch, Removal};
        use super::super::edge_view::EdgeView;
        let removal = Removal {
            entity: Some(EntityMatch {
                id: "entity:vim".into(),
                name: " Vim ".into(),
                entity_type: "tool".into(),
            }),
            edges: vec![EdgeView {
                id: "edge:1".into(),
                from: "D".into(),
                to: "Vim".into(),
                rel_type: "USES".into(),
                description: None,
                confidence: 1.0,
                evidence: 1.0,
                self_reinforcements: 0,
                superseded: false,
            }],
        };
        let records = records_for_removal(&removal);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].subject, "vim");
        assert_eq!(records[0].kind, Some(RecordKind::Entity));
        assert_eq!(records[0].entity_id.as_deref(), Some("entity:vim"));
        assert_eq!(records[1].relation.as_deref(), Some("USES"));
        assert_eq!(records[1].value.as_deref(), Some("Vim"));
        assert_eq!(records[1].edge_id.as_deref(), Some("edge:1"));
        assert_eq!(records[1].provenance, "user");
    }

    #[test]
    fn forget_lock_wait_uses_the_test_seam() {
        let _guard = ForgetWaitOverride::at(Duration::ZERO);
        assert_eq!(forget_lock_wait(), Duration::ZERO);
    }

    #[test]
    fn forget_lock_wait_ignores_the_environment() {
        let previous = std::env::var("RECALL_ECHO_FORGET_LOCK_MS").ok();
        std::env::set_var("RECALL_ECHO_FORGET_LOCK_MS", "0");
        assert_eq!(forget_lock_wait(), crate::file_lock::LOCK_WAIT);
        match previous {
            Some(value) => std::env::set_var("RECALL_ECHO_FORGET_LOCK_MS", value),
            None => std::env::remove_var("RECALL_ECHO_FORGET_LOCK_MS"),
        }
    }

    #[test]
    fn a_malformed_file_is_unreadable_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("temp");
        let lock = lock_forgotten(dir.path(), Duration::from_secs(1)).expect("lock");
        ensure_readable(dir.path(), &lock).expect("missing file");
        std::fs::write(dir.path().join("forgotten.jsonl"), b"{not json\n").expect("file");
        let err = ensure_readable(dir.path(), &lock).expect_err("malformed");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    #[test]
    fn forgotten_skips_field_is_omitted_when_zero() {
        assert_eq!(forgotten_skips_field(0), None);
        assert_eq!(forgotten_skips_field(1), Some(1));
    }

    #[test]
    fn mask_json_replaces_text_fields_and_leaves_other_strings() {
        let set = live_set(&[fact_record("Ada", "uses", "Rust", AT)]);
        let value = serde_json::json!({
            "name": "Rust",
            "id": "entity:rust",
            "abstract_text": "Ada uses Rust.",
            "score": 1
        });
        let masked = mask_json(&set, value);
        assert_eq!(masked["name"], "[forgotten]");
        assert_eq!(masked["abstract_text"], "Ada uses [forgotten].");
        assert_eq!(masked["id"], "entity:rust");
        assert_eq!(masked["score"], 1);
        let entity = live_set(&[entity_record("Vim", AT)]);
        let named = serde_json::json!({"name": "Vim"});
        assert_eq!(mask_json(&entity, named)["name"], "Vim");
    }

    #[test]
    fn a_zero_count_adds_nothing_and_flag_off_reads_nothing() {
        let dir = tempfile::tempdir().expect("temp");
        let path = forgotten_path(dir.path());
        fs::create_dir(&path).expect("unreadable");
        let text = finish_read_text(dir.path(), "What I know\n".into()).expect("off");
        assert_eq!(text, "What I know\n");
        fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nlasting_forget = true\n",
        )
        .expect("config");
        let err = finish_read_text(dir.path(), "What I know\n".into()).expect_err("bad file");
        assert!(err.to_string().contains("unreadable"), "{err}");
    }

    #[test]
    fn a_live_record_adds_a_count_without_the_value() {
        let set = live_set(&[fact_record("Ada", "uses", "Rust", AT)]);
        let text = finish_with_set(&set, "Ada uses Rust.\n".into());
        assert!(text.contains("[forgotten]"));
        assert!(text.contains("Forgotten: 1"));
        assert!(!text.contains("Rust"));
    }

    #[test]
    fn a_second_append_keeps_the_first_record() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(dir.path(), &[sample_fact()], Duration::from_secs(1)).expect("first");
        append_forget_records(
            dir.path(),
            &[fact_record("Bea", "uses", "Go", AT)],
            Duration::from_secs(1),
        )
        .expect("second");
        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert!(is_forgotten_fact(&set, "Bea", "uses", "Go"));
    }

    fn slot_line(values: &str) -> String {
        format!(
            "{{\"v\":1,\"status\":\"active\",\"kind\":\"slot\",\"subject\":\"Ada\",\
             \"relation\":\"works_at\",\"values\":{values},\
             \"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}}\n"
        )
    }

    #[test]
    fn slot_forget_on_is_true_only_when_both_flags_are_on() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join(".recall-echo.toml");
        assert!(!slot_forget_on(dir.path()));
        assert!(!lasting_forget_on(dir.path()));
        fs::write(&path, "[graph]\nslot_forget = true\n").expect("slot only");
        assert!(!lasting_forget_on(dir.path()));
        assert!(!slot_forget_on(dir.path()));
        fs::write(&path, "[graph]\nlasting_forget = true\n").expect("lasting only");
        assert!(lasting_forget_on(dir.path()));
        assert!(!slot_forget_on(dir.path()));
        fs::write(
            &path,
            "[graph]\nlasting_forget = true\nslot_forget = true\n",
        )
        .expect("both");
        assert!(slot_forget_on(dir.path()));
    }

    #[test]
    fn a_slot_row_matches_the_relation_and_not_another() {
        let set = live_set(&[slot_record("Ada", "works_at", &["Acme", "Birch"], AT)]);
        assert!(is_forgotten_slot(&set, "  ADA ", "WORKS_AT"));
        assert!(!is_forgotten_slot(&set, "Ada", "lives_in"));
        assert!(!is_forgotten_slot(&set, "Bea", "works_at"));
        assert!(!is_forgotten_fact(&set, "Ada", "works_at", "Acme"));
        assert!(!is_forgotten_entity(&set, "Ada"));
        assert_eq!(set.live_count(), 1);
    }

    #[test]
    fn a_leftover_slot_row_does_not_break_a_lasting_forget_load() {
        let dir = tempfile::tempdir().expect("temp");
        let mut body = slot_line("[\"Acme\"]");
        body.push_str(
            "{\"v\":1,\"status\":\"active\",\"kind\":\"fact\",\"subject\":\"Ada\",\"relation\":\"uses\",\
             \"value\":\"Rust\",\"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n",
        );
        fs::write(forgotten_path(dir.path()), &body).expect("file");
        let set = load_forget_set(dir.path(), true).expect("lasting only");
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert!(!is_forgotten_slot(&set, "Ada", "works_at"));
        assert_eq!(set.live_count(), 1);
        assert_eq!(
            fs::read_to_string(forgotten_path(dir.path())).expect("unchanged"),
            body
        );

        fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nlasting_forget = true\nslot_forget = true\n",
        )
        .expect("both");
        let set = load_forget_set(dir.path(), true).expect("both");
        assert!(is_forgotten_slot(&set, "Ada", "works_at"));
        assert!(is_forgotten_fact(&set, "Ada", "uses", "Rust"));
        assert_eq!(set.live_count(), 2);
    }

    #[test]
    fn a_slot_row_round_trips_and_an_inactive_one_is_not_live() {
        let dir = tempfile::tempdir().expect("temp");
        fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nlasting_forget = true\nslot_forget = true\n",
        )
        .expect("both flags");
        let mut inactive = slot_record("Bea", "likes", &["Go"], AT);
        inactive.status = RecordStatus::Inactive;
        append_forget_records(
            dir.path(),
            &[slot_record("Ada", "works_at", &["Acme"], AT), inactive],
            Duration::from_secs(1),
        )
        .expect("append");
        let text = fs::read_to_string(forgotten_path(dir.path())).expect("file");
        assert!(text.contains("\"kind\":\"slot\""));
        assert!(text.contains("\"values\":[\"Acme\"]"));
        assert!(!text.contains("\"value\""));
        let set = load_forget_set(dir.path(), true).expect("load");
        assert!(is_forgotten_slot(&set, "Ada", "works_at"));
        assert!(!is_forgotten_slot(&set, "Bea", "likes"));
        assert_eq!(set.live_count(), 1);
    }

    #[test]
    fn an_unknown_kind_is_still_malformed_and_a_slot_needs_a_relation() {
        let dir = tempfile::tempdir().expect("temp");
        let path = forgotten_path(dir.path());
        fs::write(
            &path,
            "{\"v\":1,\"status\":\"active\",\"kind\":\"other\",\"subject\":\"Ada\",\
             \"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n",
        )
        .expect("unknown");
        let err = load_forget_set(dir.path(), true).expect_err("unknown kind");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
        fs::write(
            &path,
            "{\"v\":1,\"status\":\"active\",\"kind\":\"slot\",\"subject\":\"Ada\",\
             \"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n",
        )
        .expect("missing relation");
        let err = load_forget_set(dir.path(), true).expect_err("missing relation");
        assert!(err.to_string().contains("missing required fields"), "{err}");
    }

    #[test]
    fn an_entity_unforget_leaves_a_leftover_slot_row() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(
            dir.path(),
            &[
                entity_record("Ada", AT),
                slot_record("Ada", "works_at", &["Acme"], AT),
                fact_record("Ada", "uses", "Rust", AT),
            ],
            Duration::from_secs(1),
        )
        .expect("append");
        let changed = unforget_records(
            dir.path(),
            &UnforgetSelector::Entity {
                subject: "Ada".into(),
            },
            Duration::from_secs(1),
        )
        .expect("unforget");
        assert_eq!(changed, 2);
        let text = fs::read_to_string(forgotten_path(dir.path())).expect("file");
        assert!(text.contains("\"kind\":\"slot\""));
        assert!(text.contains("\"status\":\"active\""));
        let set = live_set(&load_records(dir.path()).expect("records"));
        assert!(is_forgotten_slot(&set, "Ada", "works_at"));
        assert!(!is_forgotten_entity(&set, "Ada"));
        assert!(!is_forgotten_fact(&set, "Ada", "uses", "Rust"));
    }

    #[test]
    fn a_setting_key_and_its_prefixed_form_are_the_same_slot() {
        let prefixed = live_set(&[slot_record("Harbor", "setting:timeout", &["30s"], AT)]);
        assert!(is_forgotten_slot(&prefixed, "Harbor", "timeout"));
        assert!(is_forgotten_slot(&prefixed, "harbor", "SETTING:TIMEOUT"));
        assert!(!is_forgotten_slot(&prefixed, "Harbor", "setting"));
        assert!(!is_forgotten_slot(&prefixed, "Harbor", "port"));
        let bare = live_set(&[slot_record("Harbor", "timeout", &["30s"], AT)]);
        assert!(is_forgotten_slot(&bare, "Harbor", "setting:timeout"));
        assert!(!is_forgotten_slot(&bare, "Harbor", "setting:port"));
    }

    #[test]
    fn an_exact_unforget_does_not_clear_a_slot_and_a_slot_unforget_does() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(
            dir.path(),
            &[
                slot_record("Ada", "works_at", &["Acme", "Birch"], AT),
                fact_record("Ada", "works_at", "Acme", AT),
                fact_record("Ada", "lives_in", "Madrid", AT),
            ],
            Duration::from_secs(1),
        )
        .expect("append");
        let exact = unforget_records(
            dir.path(),
            &UnforgetSelector::Fact {
                subject: "Ada".into(),
                relation: "works_at".into(),
                value: "Acme".into(),
            },
            Duration::from_secs(1),
        )
        .expect("exact");
        assert_eq!(exact, 1);
        let set = live_set(&load_records(dir.path()).expect("records"));
        assert!(is_forgotten_slot(&set, "Ada", "works_at"));
        assert!(!is_forgotten_fact(&set, "Ada", "works_at", "Acme"));
        assert!(is_forgotten_fact(&set, "Ada", "lives_in", "Madrid"));

        let slot = unforget_records(
            dir.path(),
            &UnforgetSelector::Slot {
                subject: "ADA".into(),
                relation: "works_at".into(),
            },
            Duration::from_secs(1),
        )
        .expect("slot");
        assert_eq!(slot, 1);
        let set = live_set(&load_records(dir.path()).expect("records"));
        assert!(!is_forgotten_slot(&set, "Ada", "works_at"));
        assert!(is_forgotten_fact(&set, "Ada", "lives_in", "Madrid"));
    }

    #[test]
    fn a_setting_prefix_unforget_clears_the_bare_key() {
        let dir = tempfile::tempdir().expect("temp");
        append_forget_records(
            dir.path(),
            &[slot_record("Harbor", "timeout", &["30s"], AT)],
            Duration::from_secs(1),
        )
        .expect("append");
        let changed = unforget_records(
            dir.path(),
            &UnforgetSelector::Slot {
                subject: "Harbor".into(),
                relation: "setting:timeout".into(),
            },
            Duration::from_secs(1),
        )
        .expect("unforget");
        assert_eq!(changed, 1);
        let set = live_set(&load_records(dir.path()).expect("records"));
        assert!(!is_forgotten_slot(&set, "Harbor", "timeout"));
    }

    #[test]
    fn a_relate_on_a_forgotten_slot_names_unforget_for_any_value() {
        let set = live_set(&[slot_record("Ada", "works_at", &["Acme"], AT)]);
        let blocked = refuse_manual_write(
            &set,
            ManualWrite::Relate {
                from: "Ada",
                relation: "works_at",
                to: "Birch",
            },
        )
        .expect_err("any value");
        assert!(blocked.to_string().contains("--unforget"), "{blocked}");
        assert!(refuse_manual_write(
            &set,
            ManualWrite::Relate {
                from: "Ada",
                relation: "lives_in",
                to: "Madrid",
            },
        )
        .is_ok());
        assert!(refuse_manual_write(&set, ManualWrite::AddEntity("Acme")).is_ok());
    }

    #[test]
    fn free_text_masks_slot_snapshot_values_and_structured_masks_the_current_value() {
        let set = live_set(&[slot_record("Ada", "works_at", &["Acme"], AT)]);
        assert_eq!(
            mask_forgotten("Ada works at Acme.", &set),
            "Ada works at [forgotten]."
        );
        assert_eq!(
            mask_forgotten("Ada works at Cedar.", &set),
            "Ada works at Cedar."
        );
        let current = serde_json::json!({
            "from": "Ada",
            "rel_type": "works_at",
            "to": "Cedar",
            "id": "edge:1"
        });
        let masked = mask_json(&set, current);
        assert_eq!(masked["to"], "[forgotten]");
        assert_eq!(masked["from"], "Ada");
        assert_eq!(masked["id"], "edge:1");
        let other = serde_json::json!({
            "from": "Ada",
            "rel_type": "lives_in",
            "to": "Madrid"
        });
        assert_eq!(mask_json(&set, other)["to"], "Madrid");
        let setting = serde_json::json!({
            "subject": "Harbor",
            "predicate": "setting",
            "key": "timeout",
            "value": "99s"
        });
        let timeout = live_set(&[slot_record("Harbor", "setting:timeout", &["30s"], AT)]);
        assert_eq!(mask_json(&timeout, setting)["value"], "[forgotten]");
        assert_eq!(mask_forgotten("still 30s", &timeout), "still [forgotten]");
        assert_eq!(mask_forgotten("now 99s", &timeout), "now 99s");
    }

    #[test]
    fn a_leftover_slot_value_is_not_masked_when_slot_forget_is_off() {
        let dir = tempfile::tempdir().expect("temp");
        fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nlasting_forget = true\n",
        )
        .expect("lasting");
        fs::write(
            forgotten_path(dir.path()),
            concat!(
                "{\"v\":1,\"status\":\"active\",\"kind\":\"slot\",\"subject\":\"Ada\",",
                "\"relation\":\"works_at\",\"values\":[\"Acme\"],",
                "\"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n",
                "{\"v\":1,\"status\":\"active\",\"kind\":\"fact\",\"subject\":\"Ada\",",
                "\"relation\":\"uses\",\"value\":\"Rust\",",
                "\"forgotten_at\":\"2026-11-17T12:00:00Z\",\"provenance\":\"user\"}\n"
            ),
        )
        .expect("file");
        let set = load_forget_set(dir.path(), true).expect("load");
        let text = mask_forgotten("Ada at Acme uses Rust.", &set);
        assert!(text.contains("Acme"), "{text}");
        assert!(text.contains("[forgotten]"), "{text}");
        assert!(!text.contains("Rust"));
    }
}
