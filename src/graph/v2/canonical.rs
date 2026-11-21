// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Copy a closed sibling's detail onto the decided row.
//!
//! Runs only when `graph.canonical_details` is on, after the forgotten skip
//! and before write. The stored value stays bare. A replacement starts empty
//! and merges only donors that belong to it.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

use super::consolidate::{names_new_value, names_old_value};
use super::known_match::MatchTarget;
use super::schema::{normalise, restates_setting, Fact, Predicate};
use super::write::{
    alphanumeric_runs, is_pending, keys_match, session_midnight, stamp_slot, values_equal,
};
use crate::graph::edge_view::record_id;
use crate::graph::error::GraphError;
use crate::graph::store::define_edge_details;
use crate::graph::types::{IngestionReport, Retirement};
use crate::graph::GraphMemory;

const CLOSE_REASON: &str = "canonical_detail";

/// Facts left to write, with donors removed, plus retirements for the closes.
pub(crate) struct Applied {
    pub facts: Vec<Fact>,
    pub targets: Vec<Option<MatchTarget>>,
    pub retirements: Vec<Retirement>,
}

/// Pair decided rows with closed siblings and apply the copies and closes.
pub(crate) async fn apply(
    gm: &GraphMemory,
    session_id: &str,
    date: &str,
    _project: &str,
    facts: Vec<Fact>,
    targets: Vec<Option<MatchTarget>>,
    report: &mut IngestionReport,
) -> Result<Applied, GraphError> {
    let loaded = load(gm).await?;
    let mut built = build(&facts, loaded, session_id);
    let plan = plan(&built.cands, &built.olds, &built.parts);
    let until = session_midnight(date);
    let ended = format!("{session_id}:{CLOSE_REASON}");
    let mut state = State::new(&built.cands);
    let mut lines = Vec::new();
    for ambiguous in &plan.ambiguous {
        add_count(&mut report.canonical_detail_ambiguous);
        lines.push(ambiguous_line(ambiguous));
    }
    for pair in &plan.pairs {
        commit_pair(
            &mut built,
            pair,
            until.as_deref(),
            &ended,
            &mut state,
            &mut lines,
            report,
        );
    }
    persist(gm, &built, &state).await?;
    lines.sort();
    for line in &lines {
        eprintln!("{line}");
    }
    let (facts, targets) = paint_facts(facts, targets, &built.cands, &state);
    Ok(Applied {
        facts,
        targets,
        retirements: state.retirements,
    })
}

struct Loaded {
    entities: Vec<EntitySnap>,
    edges: Vec<EdgeSnap>,
}

struct EntitySnap {
    id: String,
    name: String,
    attributes: Value,
}

struct EdgeSnap {
    id: String,
    from_id: String,
    to_id: String,
    rel_type: String,
    status: String,
    live: bool,
    pending: bool,
    details: String,
    first_session: String,
}

struct Built {
    cands: Vec<Cand>,
    olds: Vec<OldVal>,
    parts: Vec<Part>,
    entities: Vec<EntitySnap>,
}

#[derive(Clone, Debug)]
struct Cand {
    subject: String,
    subject_raw: String,
    predicate: String,
    key: String,
    value: String,
    status: String,
    turn: Option<usize>,
    live: bool,
    earlier: bool,
    facts: Vec<usize>,
    slot: Option<SlotAt>,
    edge: Option<String>,
    details: String,
}

#[derive(Clone, Debug)]
struct SlotAt {
    entity: usize,
    index: usize,
}

#[derive(Clone, Debug)]
struct OldVal {
    subject: String,
    key: String,
    value: String,
}

#[derive(Clone, Debug)]
struct Part {
    component: String,
    project: String,
}

#[derive(Clone, Debug)]
struct Pair {
    canonical: usize,
    donor: usize,
    text: String,
    close: bool,
}

struct Planned {
    pairs: Vec<Pair>,
    ambiguous: Vec<Cand>,
}

struct State {
    details: HashMap<usize, String>,
    drop_facts: BTreeSet<usize>,
    attributes: HashMap<usize, Value>,
    dirty: BTreeSet<usize>,
    edge_details: HashMap<String, String>,
    retirements: Vec<Retirement>,
}

impl State {
    fn new(cands: &[Cand]) -> Self {
        Self {
            details: cands
                .iter()
                .enumerate()
                .map(|(index, cand)| (index, cand.details.clone()))
                .collect(),
            drop_facts: BTreeSet::new(),
            attributes: HashMap::new(),
            dirty: BTreeSet::new(),
            edge_details: HashMap::new(),
            retirements: Vec::new(),
        }
    }
}

async fn load(gm: &GraphMemory) -> Result<Loaded, GraphError> {
    let mut entities = gm
        .db()
        .query("SELECT id, name, attributes FROM entity")
        .await?;
    let entity_rows: Vec<Value> = crate::graph::deserialize_take(&mut entities, 0)?;
    let mut edges = gm.db().query("SELECT * FROM relates_to").await?;
    let edge_rows: Vec<Value> = crate::graph::deserialize_take(&mut edges, 0)?;
    Ok(Loaded {
        entities: entity_rows.into_iter().filter_map(entity_snap).collect(),
        edges: edge_rows.into_iter().filter_map(edge_snap).collect(),
    })
}

fn entity_snap(row: Value) -> Option<EntitySnap> {
    let id = record_id(row.get("id")?);
    if id.is_empty() {
        return None;
    }
    Some(EntitySnap {
        id,
        name: text_of(row.get("name")),
        attributes: row.get("attributes").cloned().unwrap_or(Value::Null),
    })
}

fn edge_snap(row: Value) -> Option<EdgeSnap> {
    let id = record_id(row.get("id")?);
    let from_id = record_id(row.get("in")?);
    let to_id = record_id(row.get("out")?);
    if id.is_empty() || from_id.is_empty() || to_id.is_empty() {
        return None;
    }
    Some(EdgeSnap {
        id,
        from_id,
        to_id,
        rel_type: text_of(row.get("rel_type")),
        status: text_of(row.get("status")),
        live: !crate::graph::crud::edge_already_closed(row.get("valid_until")),
        pending: is_pending(&row),
        details: text_of(row.get("details")),
        first_session: first_session(&row),
    })
}

fn build(facts: &[Fact], loaded: Loaded, session_id: &str) -> Built {
    let mut cands = Vec::new();
    let mut olds = Vec::new();
    let mut parts = Vec::new();
    let names: HashMap<String, (String, String)> = loaded
        .entities
        .iter()
        .map(|entity| {
            (
                entity.id.clone(),
                (entity.name.clone(), normalise(&entity.name)),
            )
        })
        .collect();
    for (index, fact) in facts.iter().enumerate() {
        if fact.predicate == Predicate::PartOf && fact.status == "decided" {
            parts.push(Part {
                component: normalise(&fact.subject_name),
                project: normalise(&fact.object_name),
            });
        }
        if fact.predicate == Predicate::Setting && fact.status == "rejected" {
            olds.push(OldVal {
                subject: normalise(&fact.subject_name),
                key: fact.key.clone(),
                value: fact.object_name.clone(),
            });
        }
        if let Some(cand) = fact_cand(fact, index) {
            cands.push(cand);
        }
    }
    for (entity_index, entity) in loaded.entities.iter().enumerate() {
        let Some(slots) = entity
            .attributes
            .pointer("/v2/slots")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for (index, slot) in slots.iter().enumerate() {
            note_slot(
                &mut cands,
                &mut olds,
                entity,
                entity_index,
                index,
                slot,
                session_id,
            );
        }
    }
    for edge in &loaded.edges {
        note_edge(&mut cands, &mut parts, edge, &names, session_id);
    }
    collapse(&mut cands);
    Built {
        cands,
        olds,
        parts,
        entities: loaded.entities,
    }
}

fn fact_cand(fact: &Fact, index: usize) -> Option<Cand> {
    let predicate = fact.predicate.as_str();
    if !matches!(predicate, "uses" | "rule" | "setting") {
        return None;
    }
    Some(Cand {
        subject: normalise(&fact.subject_name),
        subject_raw: fact.subject_name.clone(),
        predicate: predicate.to_string(),
        key: fact.key.clone(),
        value: fact.object_name.clone(),
        status: fact.status.clone(),
        turn: Some(fact.turn),
        live: false,
        earlier: false,
        facts: vec![index],
        slot: None,
        edge: None,
        details: fact.details.trim().to_string(),
    })
}

fn note_slot(
    cands: &mut Vec<Cand>,
    olds: &mut Vec<OldVal>,
    entity: &EntitySnap,
    entity_index: usize,
    index: usize,
    slot: &Value,
    session_id: &str,
) {
    if !slot.is_object() {
        return;
    }
    let predicate = text_of(slot.get("predicate"));
    let key = text_of(slot.get("key"));
    let value = text_of(slot.get("value"));
    let status = text_of(slot.get("status"));
    let live = !crate::graph::crud::edge_already_closed(slot.get("valid_until"));
    let subject = normalise(&entity.name);
    if predicate == "setting" && (status == "rejected" || !live) {
        olds.push(OldVal {
            subject: subject.clone(),
            key: key.clone(),
            value: value.clone(),
        });
    }
    if !live || is_pending(slot) || !matches!(predicate.as_str(), "rule" | "setting") {
        return;
    }
    cands.push(Cand {
        subject,
        subject_raw: entity.name.clone(),
        predicate,
        key,
        value,
        status,
        turn: None,
        live: true,
        earlier: first_session(slot) != session_id,
        facts: Vec::new(),
        slot: Some(SlotAt {
            entity: entity_index,
            index,
        }),
        edge: None,
        details: text_of(slot.get("details")),
    });
}

fn note_edge(
    cands: &mut Vec<Cand>,
    parts: &mut Vec<Part>,
    edge: &EdgeSnap,
    names: &HashMap<String, (String, String)>,
    session_id: &str,
) {
    if !edge.live || edge.pending {
        return;
    }
    let Some((from_raw, from)) = names.get(&edge.from_id) else {
        return;
    };
    let Some((to_raw, to)) = names.get(&edge.to_id) else {
        return;
    };
    if edge.rel_type == "part_of" && edge.status == "decided" {
        parts.push(Part {
            component: from.clone(),
            project: to.clone(),
        });
    }
    if edge.rel_type != "uses" {
        return;
    }
    cands.push(Cand {
        subject: from.clone(),
        subject_raw: from_raw.clone(),
        predicate: "uses".into(),
        key: String::new(),
        value: to_raw.clone(),
        status: edge.status.clone(),
        turn: None,
        live: true,
        earlier: edge.first_session != session_id,
        facts: Vec::new(),
        slot: None,
        edge: Some(edge.id.clone()),
        details: edge.details.clone(),
    });
}

fn collapse(cands: &mut Vec<Cand>) {
    let mut kept: Vec<Cand> = cands.iter().filter(|cand| cand.live).cloned().collect();
    for cand in cands.iter().filter(|cand| !cand.live) {
        if let Some(live) = kept.iter_mut().find(|other| same_row(other, cand)) {
            live.facts.extend(cand.facts.iter().copied());
            if live.details.trim().is_empty() {
                live.details.clone_from(&cand.details);
            }
            continue;
        }
        kept.push(cand.clone());
    }
    *cands = kept;
}

fn same_row(left: &Cand, right: &Cand) -> bool {
    left.subject == right.subject
        && left.predicate == right.predicate
        && left.status == right.status
        && values_equal(&left.value, &right.value)
        && keys_same(&left.predicate, &left.key, &right.key)
}

fn plan(cands: &[Cand], olds: &[OldVal], parts: &[Part]) -> Planned {
    let (mut d2, mut ambiguous) = unique(d2_pairs(cands, olds), true, cands);
    let (mut d3, mut more) = unique(d3_pairs(cands), true, cands);
    ambiguous.append(&mut more);
    let closing: BTreeSet<usize> = donor_ids(&d2).union(&donor_ids(&d3)).copied().collect();
    let (mut d1, mut more) = unique(d1_pairs(cands, parts, &closing), false, cands);
    ambiguous.append(&mut more);
    let clashes = role_clashes(&[&d1, &d2, &d3]);
    clear(&mut d1, &clashes, cands, &mut ambiguous);
    clear(&mut d2, &clashes, cands, &mut ambiguous);
    clear(&mut d3, &clashes, cands, &mut ambiguous);
    let mut pairs = d1;
    pairs.extend(d2);
    pairs.extend(d3);
    pairs.sort_by(|left, right| {
        left.canonical.cmp(&right.canonical).then(
            cands[left.donor]
                .value
                .cmp(&cands[right.donor].value)
                .then(left.donor.cmp(&right.donor)),
        )
    });
    ambiguous.sort_by(|left, right| {
        left.subject_raw
            .cmp(&right.subject_raw)
            .then(left.predicate.cmp(&right.predicate))
            .then(left.value.cmp(&right.value))
    });
    ambiguous.dedup_by(|left, right| {
        left.subject == right.subject
            && left.predicate == right.predicate
            && left.value == right.value
            && left.key == right.key
    });
    Planned { pairs, ambiguous }
}

fn d2_pairs(cands: &[Cand], olds: &[OldVal]) -> Vec<Pair> {
    let mut pairs = Vec::new();
    for (canonical, canon) in cands.iter().enumerate() {
        if !decided_setting(canon) {
            continue;
        }
        for (donor, rule) in cands.iter().enumerate() {
            if let Some(text) = d2_text(canon, rule, olds) {
                pairs.push(Pair {
                    canonical,
                    donor,
                    text,
                    close: true,
                });
            }
        }
    }
    pairs
}

fn d2_text(canon: &Cand, rule: &Cand, olds: &[OldVal]) -> Option<String> {
    if rule.predicate != "rule" || rule.status != "decided" || rule.subject != canon.subject {
        return None;
    }
    if !donor_time(canon, rule) {
        return None;
    }
    if restates_setting(&rule.value, &canon.key, &canon.value) {
        return None;
    }
    if names_retired(canon, &rule.value, olds) {
        return None;
    }
    let text = collapse_ws(&strip_olds(&rule.value, canon, olds));
    if text.is_empty() || !adds_tokens(&text, &canon.value) {
        return None;
    }
    Some(text)
}

/// A live donor from an earlier session, or a same-session fact on the same turn.
fn donor_time(canon: &Cand, donor: &Cand) -> bool {
    if donor.live {
        return donor.earlier;
    }
    !canon.live && donor.turn.is_some() && donor.turn == canon.turn
}

fn names_retired(canon: &Cand, text: &str, olds: &[OldVal]) -> bool {
    if names_new_value(&canon.value, text) {
        return false;
    }
    olds.iter().any(|old| {
        old.subject == canon.subject
            && keys_same("setting", &canon.key, &old.key)
            && !values_equal(&old.value, &canon.value)
            && names_old_value(&canon.key, &old.value, text)
    })
}

fn strip_olds(text: &str, canon: &Cand, olds: &[OldVal]) -> String {
    let mut out = text.to_string();
    for old in olds {
        if old.subject == canon.subject && keys_same("setting", &canon.key, &old.key) {
            out = strip_bounded(&out, &old.value);
        }
    }
    out
}

fn d3_pairs(cands: &[Cand]) -> Vec<Pair> {
    let mut pairs = Vec::new();
    for (canonical, canon) in cands.iter().enumerate() {
        if !decided_setting(canon) || env_name(&canon.value) {
            continue;
        }
        for (donor, other) in cands.iter().enumerate() {
            if donor != canonical && d3_donor(canon, other) {
                pairs.push(Pair {
                    canonical,
                    donor,
                    text: other.value.clone(),
                    close: true,
                });
            }
        }
    }
    pairs
}

fn d3_donor(canon: &Cand, donor: &Cand) -> bool {
    donor.predicate == "setting"
        && donor.status == "decided"
        && donor.subject == canon.subject
        && env_name(&donor.value)
        && !keys_same("setting", &canon.key, &donor.key)
        && shares_stem(&donor.value, &canon.key, &canon.subject_raw)
        && (!donor.live || donor.earlier)
}

fn d1_pairs(cands: &[Cand], parts: &[Part], closing: &BTreeSet<usize>) -> Vec<Pair> {
    let mut pairs = Vec::new();
    for (canonical, canon) in cands.iter().enumerate() {
        if closing.contains(&canonical) || !d1_canonical(canon, cands) {
            continue;
        }
        for (donor, other) in cands.iter().enumerate() {
            if d1_donor(canon, other, parts) {
                pairs.push(Pair {
                    canonical,
                    donor,
                    text: other.value.clone(),
                    close: false,
                });
            }
        }
    }
    pairs
}

fn d1_canonical(cand: &Cand, cands: &[Cand]) -> bool {
    cand.status == "decided"
        && (cand.predicate == "uses"
            || (cand.predicate == "rule" && cands.iter().any(|uses| names_chosen(cand, uses))))
}

fn names_chosen(rule: &Cand, uses: &Cand) -> bool {
    uses.predicate == "uses"
        && uses.status == "decided"
        && uses.subject == rule.subject
        && (values_equal(&uses.value, &rule.value) || names_new_value(&uses.value, &rule.value))
}

fn d1_donor(canon: &Cand, donor: &Cand, parts: &[Part]) -> bool {
    if donor.predicate != "uses" || !matches!(donor.status.as_str(), "considered" | "rejected") {
        return false;
    }
    if canon.predicate == "uses" && values_equal(&donor.value, &canon.value) {
        return false;
    }
    if canon.predicate == "rule"
        && (values_equal(&donor.value, &canon.value) || names_new_value(&donor.value, &canon.value))
    {
        return false;
    }
    donor.subject == canon.subject || component_of(&donor.subject, &canon.subject, parts)
}

fn component_of(component: &str, project: &str, parts: &[Part]) -> bool {
    parts
        .iter()
        .any(|part| part.component == component && part.project == project)
}

fn unique(pairs: Vec<Pair>, both_ways: bool, cands: &[Cand]) -> (Vec<Pair>, Vec<Cand>) {
    let mut by_donor: HashMap<usize, usize> = HashMap::new();
    let mut by_canon: HashMap<usize, usize> = HashMap::new();
    for pair in &pairs {
        *by_donor.entry(pair.donor).or_insert(0) += 1;
        *by_canon.entry(pair.canonical).or_insert(0) += 1;
    }
    let mut ambiguous = Vec::new();
    let mut seen = BTreeSet::new();
    let kept = pairs
        .into_iter()
        .filter(|pair| {
            let ok = by_donor.get(&pair.donor).copied() == Some(1)
                && (!both_ways || by_canon.get(&pair.canonical).copied() == Some(1));
            if !ok && seen.insert(pair.donor) {
                ambiguous.push(cands[pair.donor].clone());
            }
            ok
        })
        .collect();
    (kept, ambiguous)
}

fn donor_ids(pairs: &[Pair]) -> BTreeSet<usize> {
    pairs.iter().map(|pair| pair.donor).collect()
}

fn role_clashes(groups: &[&Vec<Pair>]) -> BTreeSet<usize> {
    let mut donors = BTreeSet::new();
    let mut canons = BTreeSet::new();
    for group in groups {
        for pair in *group {
            donors.insert(pair.donor);
            canons.insert(pair.canonical);
        }
    }
    donors.intersection(&canons).copied().collect()
}

fn clear(
    pairs: &mut Vec<Pair>,
    clashes: &BTreeSet<usize>,
    cands: &[Cand],
    ambiguous: &mut Vec<Cand>,
) {
    let mut seen = BTreeSet::new();
    pairs.retain(|pair| {
        let clear = !clashes.contains(&pair.canonical) && !clashes.contains(&pair.donor);
        if !clear && seen.insert(pair.donor) {
            ambiguous.push(cands[pair.donor].clone());
        }
        clear
    });
}

fn decided_setting(cand: &Cand) -> bool {
    cand.predicate == "setting"
        && cand.status == "decided"
        && !cand.key.trim().is_empty()
        && !cand.value.trim().is_empty()
}

fn keys_same(predicate: &str, left: &str, right: &str) -> bool {
    if predicate != "setting" {
        return left == right;
    }
    if left.trim().is_empty() || right.trim().is_empty() {
        return left.trim().eq_ignore_ascii_case(right.trim());
    }
    keys_match(left, right)
}

fn adds_tokens(extra: &str, value: &str) -> bool {
    let have: BTreeSet<_> = content_tokens(value).into_iter().collect();
    content_tokens(extra)
        .into_iter()
        .any(|token| !have.contains(&token))
}

fn content_tokens(text: &str) -> Vec<String> {
    alphanumeric_runs(text)
        .into_iter()
        .filter(|token| token.chars().count() >= 3)
        .collect()
}

fn env_name(value: &str) -> bool {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Z][A-Z0-9_]{3,}$").ok())
        .as_ref()
        .is_some_and(|re| re.is_match(value.trim()))
}

fn shares_stem(env: &str, key: &str, subject: &str) -> bool {
    let env_stems = stems(env);
    stems(key)
        .union(&stems(subject))
        .any(|token| env_stems.contains(token))
}

fn stems(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn strip_bounded(text: &str, value: &str) -> String {
    let needle: Vec<char> = value.trim().chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        if starts_bounded(&chars, index, &needle) {
            index += needle.len();
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn starts_bounded(chars: &[char], start: usize, needle: &[char]) -> bool {
    let end = start + needle.len();
    if end > chars.len() {
        return false;
    }
    let folded: Vec<char> = chars[start..end]
        .iter()
        .copied()
        .flat_map(char::to_lowercase)
        .collect();
    folded == needle
        && (start == 0 || !chars[start - 1].is_alphanumeric())
        && (end == chars.len() || !chars[end].is_alphanumeric())
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn commit_pair(
    built: &mut Built,
    pair: &Pair,
    until: Option<&str>,
    ended: &str,
    state: &mut State,
    lines: &mut Vec<String>,
    report: &mut IngestionReport,
) {
    if pair.close && !close_donor(built, pair, until, ended, state, lines, report) {
        return;
    }
    let current = state
        .details
        .get(&pair.canonical)
        .cloned()
        .unwrap_or_default();
    let Some(next) = append_detail(&current, &pair.text) else {
        return;
    };
    state.details.insert(pair.canonical, next.clone());
    add_count(&mut report.canonical_detail_merges);
    let subject = built.cands[pair.canonical].subject_raw.clone();
    let predicate = built.cands[pair.canonical].predicate.clone();
    let value = built.cands[pair.canonical].value.clone();
    let slot = built.cands[pair.canonical].slot.clone();
    let edge = built.cands[pair.canonical].edge.clone();
    lines.push(detail_line(&subject, &predicate, &value, &pair.text));
    if let Some(slot) = slot {
        set_slot_details(built, state, &slot, &next);
    }
    if let Some(edge) = edge {
        state.edge_details.insert(edge, next);
    }
}

fn close_donor(
    built: &mut Built,
    pair: &Pair,
    until: Option<&str>,
    ended: &str,
    state: &mut State,
    lines: &mut Vec<String>,
    report: &mut IngestionReport,
) -> bool {
    let donor = built.cands[pair.donor].clone();
    if donor.live && !donor.earlier {
        return false;
    }
    if donor.live {
        let (Some(until), Some(slot)) = (until, donor.slot.clone()) else {
            return false;
        };
        if !stamp_at(built, state, &slot, &donor.value, until, ended) {
            return false;
        }
    }
    state.drop_facts.extend(donor.facts.iter().copied());
    add_count(&mut report.canonical_detail_closes);
    lines.push(close_line(
        &donor.subject_raw,
        &donor.predicate,
        &donor.value,
    ));
    let canon_value = built.cands[pair.canonical].value.clone();
    state.retirements.push(Retirement {
        reason: CLOSE_REASON.to_string(),
        subject: donor.subject_raw,
        predicate: donor.predicate,
        key: donor.key,
        old_value: donor.value,
        new_value: canon_value,
    });
    true
}

fn stamp_at(
    built: &mut Built,
    state: &mut State,
    slot: &SlotAt,
    value: &str,
    until: &str,
    ended: &str,
) -> bool {
    let attrs = attrs(built, state, slot.entity);
    let Some(slots) = attrs.pointer_mut("/v2/slots").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(row) = slots.get_mut(slot.index) else {
        return false;
    };
    if text_of(row.get("value")) != value {
        return false;
    }
    stamp_slot(row, until, ended)
}

fn set_slot_details(built: &mut Built, state: &mut State, slot: &SlotAt, text: &str) {
    let entity = slot.entity;
    let index = slot.index;
    let attrs = attrs(built, state, entity);
    let Some(row) = attrs
        .pointer_mut("/v2/slots")
        .and_then(Value::as_array_mut)
        .and_then(|slots| slots.get_mut(index))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if text.trim().is_empty() {
        row.remove("details");
    } else {
        row.insert("details".to_string(), json!(text));
    }
}

fn attrs<'a>(built: &'a mut Built, state: &'a mut State, entity: usize) -> &'a mut Value {
    state.dirty.insert(entity);
    state
        .attributes
        .entry(entity)
        .or_insert_with(|| built.entities[entity].attributes.clone())
}

fn append_detail(existing: &str, extra: &str) -> Option<String> {
    let extra = extra.trim();
    if extra.is_empty() || has_detail(existing, extra) {
        return None;
    }
    let existing = existing.trim();
    if existing.is_empty() {
        Some(extra.to_string())
    } else {
        Some(format!("{existing}, {extra}"))
    }
}

fn has_detail(existing: &str, extra: &str) -> bool {
    existing.split(',').map(str::trim).any(|part| part == extra)
}

async fn persist(gm: &GraphMemory, built: &Built, state: &State) -> Result<(), GraphError> {
    for entity in &state.dirty {
        let Some(attributes) = state.attributes.get(entity) else {
            continue;
        };
        let id = built.entities[*entity].id.clone();
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", id))
            .bind(("attributes", attributes.clone()))
            .await?
            .check()?;
    }
    if state.edge_details.is_empty() {
        return Ok(());
    }
    define_edge_details(gm.db()).await?;
    for (id, details) in &state.edge_details {
        gm.db()
            .query("UPDATE type::record($id) SET details = $details")
            .bind(("id", id.clone()))
            .bind(("details", details.clone()))
            .await?
            .check()?;
    }
    Ok(())
}

fn paint_facts(
    facts: Vec<Fact>,
    targets: Vec<Option<MatchTarget>>,
    cands: &[Cand],
    state: &State,
) -> (Vec<Fact>, Vec<Option<MatchTarget>>) {
    let mut painted: HashMap<usize, String> = HashMap::new();
    for (index, cand) in cands.iter().enumerate() {
        let Some(text) = state.details.get(&index) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        for fact in &cand.facts {
            painted.insert(*fact, text.clone());
        }
    }
    let paired = targets.len() == facts.len();
    let mut kept_facts = Vec::new();
    let mut kept_targets = Vec::new();
    for (index, mut fact) in facts.into_iter().enumerate() {
        if state.drop_facts.contains(&index) {
            continue;
        }
        if let Some(text) = painted.get(&index) {
            fact.details.clone_from(text);
        }
        if paired {
            kept_targets.push(targets[index].clone());
        }
        kept_facts.push(fact);
    }
    (kept_facts, if paired { kept_targets } else { targets })
}

fn detail_line(subject: &str, predicate: &str, value: &str, text: &str) -> String {
    format!(
        "canonical detail: {} | {predicate} | {} | {}",
        plain(subject),
        plain(value),
        plain(text)
    )
}

fn close_line(subject: &str, predicate: &str, value: &str) -> String {
    format!(
        "canonical detail close: {} | {predicate} | {}",
        plain(subject),
        plain(value)
    )
}

fn ambiguous_line(cand: &Cand) -> String {
    format!(
        "canonical detail ambiguous: {} | {} | {}",
        plain(&cand.subject_raw),
        cand.predicate,
        plain(&cand.value)
    )
}

fn plain(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

fn add_count(slot: &mut Option<u32>) {
    *slot = Some(slot.unwrap_or(0).saturating_add(1));
}

fn text_of(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or("").to_string()
}

fn first_session(row: &Value) -> String {
    row.get("evidence")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .map(|item| text_of(item.get("session")))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::confidence::Provenance;
    use crate::graph::v2::schema::Kind;

    fn row(
        subject: &str,
        predicate: &str,
        key: &str,
        value: &str,
        status: &str,
        turn: usize,
    ) -> Cand {
        Cand {
            subject: normalise(subject),
            subject_raw: subject.to_string(),
            predicate: predicate.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            status: status.to_string(),
            turn: Some(turn),
            live: false,
            earlier: false,
            facts: Vec::new(),
            slot: None,
            edge: None,
            details: String::new(),
        }
    }

    fn planned(cands: &[Cand], olds: &[OldVal], parts: &[Part]) -> Planned {
        plan(cands, olds, parts)
    }

    #[test]
    fn d1_copies_a_rejected_alternative_and_leaves_it() {
        let cands = vec![
            row("Harbor", "uses", "", "cargo", "decided", 1),
            row("Harbor", "uses", "", "http", "rejected", 1),
        ];
        let plan = planned(&cands, &[], &[]);
        assert!(plan.ambiguous.is_empty());
        assert_eq!(plan.pairs.len(), 1);
        assert!(!plan.pairs[0].close);
        assert_eq!(plan.pairs[0].text, "http");
    }

    #[test]
    fn d1_two_decided_uses_on_the_project_change_nothing() {
        let cands = vec![
            row("Harbor", "uses", "", "cargo", "decided", 1),
            row("Harbor", "uses", "", "rustc", "decided", 1),
            row("Harbor", "uses", "", "http", "rejected", 1),
        ];
        let plan = planned(&cands, &[], &[]);
        assert!(plan.pairs.is_empty());
        assert_eq!(plan.ambiguous.len(), 1);
    }

    #[test]
    fn d1_accepts_a_donor_on_a_component() {
        let cands = vec![
            row("Harbor", "uses", "", "cargo", "decided", 1),
            row("worker", "uses", "", "http", "considered", 1),
        ];
        let parts = vec![Part {
            component: normalise("worker"),
            project: normalise("Harbor"),
        }];
        let plan = planned(&cands, &[], &parts);
        assert_eq!(plan.pairs.len(), 1);
        assert_eq!(plan.pairs[0].text, "http");
    }

    #[test]
    fn d2_copies_a_reason_and_closes_it() {
        let cands = vec![
            row("Harbor", "setting", "msrv", "1.23", "decided", 1),
            row(
                "Harbor",
                "rule",
                "",
                "range-over-func iterators",
                "decided",
                1,
            ),
        ];
        let plan = planned(&cands, &[], &[]);
        assert!(plan.ambiguous.is_empty());
        assert_eq!(plan.pairs.len(), 1);
        assert!(plan.pairs[0].close);
        assert_eq!(plan.pairs[0].text, "range-over-func iterators");
    }

    #[test]
    fn d2_two_settings_and_one_rule_change_nothing() {
        let cands = vec![
            row("Harbor", "setting", "msrv", "1.23", "decided", 1),
            row("Harbor", "setting", "jobs", "4", "decided", 1),
            row(
                "Harbor",
                "rule",
                "",
                "range-over-func iterators",
                "decided",
                1,
            ),
        ];
        let plan = planned(&cands, &[], &[]);
        assert!(plan.pairs.is_empty());
        assert_eq!(plan.ambiguous.len(), 1);
    }

    #[test]
    fn a_restating_rule_is_not_a_donor() {
        let cands = vec![
            row("Harbor", "setting", "msrv", "1.23", "decided", 1),
            row("Harbor", "rule", "", "msrv is 1.23", "decided", 1),
        ];
        let plan = planned(&cands, &[], &[]);
        assert!(plan.pairs.is_empty());
        assert!(plan.ambiguous.is_empty());
    }

    #[test]
    fn a_rule_that_names_the_old_value_is_excluded() {
        let cands = vec![
            row("Harbor", "setting", "msrv", "1.23", "decided", 2),
            row("Harbor", "rule", "", "stay on 1.22", "decided", 2),
        ];
        let olds = vec![OldVal {
            subject: normalise("Harbor"),
            key: "msrv".into(),
            value: "1.22".into(),
        }];
        let plan = planned(&cands, &olds, &[]);
        assert!(plan.pairs.is_empty(), "{:?}", plan.pairs);
    }

    #[test]
    fn d3_copies_an_env_name_and_closes_it() {
        let cands = vec![
            row("Harbor", "setting", "workers", "4", "decided", 1),
            row("Harbor", "setting", "env", "HARBOR_WORKERS", "decided", 1),
        ];
        let plan = planned(&cands, &[], &[]);
        assert_eq!(plan.pairs.len(), 1, "{:?}", plan.ambiguous);
        assert!(plan.pairs[0].close);
        assert_eq!(plan.pairs[0].text, "HARBOR_WORKERS");
        assert_eq!(plan.pairs[0].canonical, 0);
    }

    #[test]
    fn a_non_unique_env_stem_changes_nothing() {
        let cands = vec![
            row("Harbor", "setting", "workers", "4", "decided", 1),
            row("Harbor", "setting", "jobs", "8", "decided", 1),
            row("Harbor", "setting", "env", "HARBOR_WORKERS", "decided", 1),
        ];
        // `jobs` does not share a stem. Two env names do.
        let mut cands = cands;
        cands[1].key = "harbor_jobs".into();
        let plan = planned(&cands, &[], &[]);
        assert!(plan.pairs.is_empty(), "{:?}", plan.pairs);
        assert!(!plan.ambiguous.is_empty());
    }

    #[test]
    fn a_replacement_does_not_inherit_details() {
        let mut old = row("Harbor", "setting", "msrv", "1.23", "decided", 1);
        old.live = true;
        old.earlier = true;
        old.details = "range-over-func iterators".into();
        let newer = row("Harbor", "setting", "msrv", "1.24", "decided", 1);
        let plan = planned(&[old, newer], &[], &[]);
        assert!(plan.pairs.is_empty());
    }

    #[test]
    fn identity_helpers_keep_the_bare_value() {
        let mut fact = Fact {
            subject_name: "Harbor".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: "msrv".into(),
            object_name: "1.23".into(),
            object_kind: None,
            status: "decided".into(),
            turn: 1,
            quote: "msrv is 1.23".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
            details: "range-over-func iterators".into(),
        };
        assert!(values_equal(&fact.object_name, "1.23"));
        assert!(!values_equal(&fact.object_name, &fact.details));
        assert!(keys_match(&fact.key, "msrv"));
        assert!(!restates_setting(
            &fact.details,
            &fact.key,
            &fact.object_name
        ));
        fact.replaces = "1.22".into();
        assert_eq!(fact.replaces, "1.22");
        assert!(names_old_value("msrv", "1.22", "stay on 1.22"));
        assert!(!names_new_value("1.23", "stay on 1.22"));
        let ground = super::super::ground::SessionGround::prepare(&[]);
        let row = super::super::known::KnownRow {
            subject: "Harbor".into(),
            predicate: "setting".into(),
            key: "msrv".into(),
            value: "1.23".into(),
            role: String::new(),
            id: "entity:harbor#0".into(),
            status: "decided".into(),
        };
        let reconciled = super::super::known_match::reconcile(&[fact], &[row], &ground);
        assert_eq!(reconciled.facts[0].object_name, "1.23");
        assert_eq!(reconciled.facts[0].details, "range-over-func iterators");
        assert!(reconciled.targets[0].is_none());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_value_change_closes_the_detailed_row_and_leaves_no_donor() {
        use super::super::input::ProjectIdentity;
        use super::super::write::{ensure_v2_store, write_facts, WriteCtx};
        use crate::graph::llm::LlmProvider;

        struct Quiet;
        #[async_trait::async_trait]
        impl LlmProvider for Quiet {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                panic!("resolver called");
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        ensure_v2_store(&gm).await.unwrap();
        let setting = Fact {
            subject_name: "Harbor".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: "msrv".into(),
            object_name: "1.23".into(),
            object_kind: None,
            status: "decided".into(),
            turn: 1,
            quote: "msrv is 1.23".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
            details: String::new(),
        };
        let mut rule = setting.clone();
        rule.predicate = Predicate::Rule;
        rule.key.clear();
        rule.object_name = "range-over-func iterators".into();
        rule.quote = "range-over-func iterators".into();
        let mut report = IngestionReport::default();
        let applied = apply(
            &gm,
            "s1",
            "2026-04-03",
            "Harbor",
            vec![setting, rule],
            Vec::new(),
            &mut report,
        )
        .await
        .unwrap();
        assert_eq!(applied.facts.len(), 1);
        assert_eq!(applied.facts[0].object_name, "1.23");
        assert_eq!(applied.facts[0].details, "range-over-func iterators");
        assert_eq!(report.canonical_detail_merges, Some(1));
        assert_eq!(report.canonical_detail_closes, Some(1));
        assert!(report.canonical_detail_ambiguous.is_none());
        assert_eq!(applied.retirements[0].reason, "canonical_detail");
        let project = ProjectIdentity {
            name: "Harbor".into(),
            aliases: Vec::new(),
        };
        let llm = Quiet;
        let ctx = WriteCtx::new(&llm, &project, "s1", "2026-04-03", &[]);
        write_facts(&gm, &ctx, &applied.facts).await.unwrap();

        let mut next = applied.facts[0].clone();
        next.object_name = "1.24".into();
        next.details.clear();
        next.quote = "msrv is 1.24".into();
        next.turn = 1;
        let mut report = IngestionReport::default();
        let applied = apply(
            &gm,
            "s2",
            "2026-04-11",
            "Harbor",
            vec![next],
            Vec::new(),
            &mut report,
        )
        .await
        .unwrap();
        assert!(applied.facts[0].details.is_empty());
        assert!(report.canonical_detail_merges.is_none());
        let ctx = WriteCtx::new(&llm, &project, "s2", "2026-04-11", &[]);
        write_facts(&gm, &ctx, &applied.facts).await.unwrap();

        let slots = harbor_slots(&gm).await;
        let old = slots.iter().find(|slot| slot["value"] == "1.23").unwrap();
        let live = slots.iter().find(|slot| slot["value"] == "1.24").unwrap();
        assert!(!old["valid_until"].is_null(), "old row closes whole: {old}");
        assert_eq!(old["details"], "range-over-func iterators");
        assert!(live["valid_until"].is_null());
        assert!(live.get("details").is_none(), "{live}");
        assert!(slots.iter().all(|slot| slot["predicate"] != "rule"));
    }

    #[cfg(feature = "embedded")]
    async fn harbor_slots(gm: &GraphMemory) -> Vec<Value> {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == "Harbor")
            .unwrap();
        entity.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap()
            .clone()
    }
}
