// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opt-in pass after a session's writes. It stamps a live row the writer
//! could not see. It does not write evidence or edit a value.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::super::crud;
use super::super::error::GraphError;
use super::super::llm::LlmProvider;
use super::super::types::{Entity, IngestionReport, Retirement};
use super::super::{deserialize_take, GraphMemory};
use super::enumeration::enumerated;
use super::ground::{is_stop_word, sentences};
use super::known_match::{coexist, departure_near, key_tokens, mentioned_with_cue, value_spans};
use super::schema::{normalise, Kind, Predicate};
use super::write::{
    alphanumeric_runs, keys_match, opened_here, session_midnight, stamp_slot, values_equal,
    WriteCtx, WriteStats, CLOSE_EDGE,
};

const PROMPT: &str = "CONSOLIDATE_V1";
const CAP: usize = 6;
const PER_ANCHOR: usize = 2;
const MAX_TOKENS: u32 = 400;
const SYSTEM: &str = "\
You compare stored project facts. For each numbered pair, OLD is a stored fact and NEW is the newer evidence. Answer one verdict:
- supersedes: the evidence shows OLD no longer holds because NEW replaced it.
- duplicate: same fact, same meaning.
- unrelated: both can be true.
- unsure.
Copy quote verbatim from the EVIDENCE text that shows the change; if you cannot, answer unrelated or unsure. Do not use outside knowledge. Two values that can both hold (different settings, different components, a list) are unrelated.";

const DEFINE_CACHE: &str = "\
DEFINE TABLE IF NOT EXISTS consolidation_cache SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS key ON consolidation_cache TYPE string;
DEFINE FIELD IF NOT EXISTS verdict ON consolidation_cache TYPE string;
DEFINE FIELD IF NOT EXISTS quote ON consolidation_cache TYPE string;
DEFINE FIELD IF NOT EXISTS prompt ON consolidation_cache TYPE string;
DEFINE FIELD IF NOT EXISTS at ON consolidation_cache TYPE datetime;
DEFINE INDEX IF NOT EXISTS consolidation_cache_key ON consolidation_cache FIELDS key UNIQUE;";

#[derive(Clone)]
struct Row {
    id: String,
    entity: String,
    slot: Option<usize>,
    edge: Option<String>,
    subject: String,
    pred: String,
    key: String,
    value: String,
    evidence: Value,
    from: String,
    here: bool,
}

#[derive(Clone)]
struct Anchor {
    id: String,
    subject: String,
    pred: String,
    key: String,
    value: String,
    text: String,
    new_value: String,
    replaces: String,
    entity: String,
    reason: &'static str,
    turn: usize,
    quote: String,
    live: Option<usize>,
    a2: bool,
}

#[derive(Clone)]
struct Pair {
    anchor: usize,
    partner: usize,
    score: i32,
    evidence: String,
    stored: String,
}

struct Loaded {
    rows: Vec<Row>,
    owners: Vec<(String, String)>,
    part_of: Vec<(String, String)>,
}

enum Step {
    Dup,
    Sup,
    Drop,
    Judge,
}

/// One session. No anchors means no read and no call. A provider or parse
/// failure is counted and does not fail the session.
pub(crate) async fn run(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[super::schema::Fact],
    stats: &WriteStats,
    llm: &dyn LlmProvider,
    report: &mut IngestionReport,
) {
    let rough = rough_anchors(facts, stats);
    if rough.is_empty() {
        return;
    }
    if define_cache(gm).await.is_err() {
        report.consolidate.errors += 1;
        return;
    }
    let Ok(loaded) = load_rows(gm, ctx.session_id).await else {
        report.consolidate.errors += 1;
        return;
    };
    let anchors = finish_anchors(rough, &loaded.rows);
    if anchors.is_empty() {
        return;
    }
    report.consolidate.anchors = count(anchors.len());
    let (selected, overflow) = select(&anchors, &loaded, ctx);
    report.consolidate.pairs = count(selected.len());
    report.consolidate.overflow = overflow;
    let mut waiting = Vec::new();
    for pair in &selected {
        match classify(pair, &anchors, &loaded.rows, &selected, ctx) {
            Step::Dup => {
                report.consolidate.prefilter.dup += 1;
                close_duplicate(gm, ctx, &anchors, &loaded.rows, pair, report).await;
            }
            Step::Sup => {
                report.consolidate.prefilter.sup += 1;
                close_partner(
                    gm,
                    ctx,
                    &anchors,
                    &loaded.rows,
                    pair,
                    "consolidated",
                    report,
                )
                .await;
            }
            Step::Drop => report.consolidate.prefilter.drop += 1,
            Step::Judge => waiting.push(pair.clone()),
        }
    }
    let mut batch = Vec::new();
    let mut verdicts = Vec::new();
    for pair in waiting {
        let key = cache_key(
            &anchors[pair.anchor],
            &loaded.rows[pair.partner],
            &pair.evidence,
        );
        match cache_get(gm, &key).await {
            Ok(Some((verdict, quote))) => {
                report.consolidate.cache_hits += 1;
                verdicts.push((pair, verdict, quote, true));
            }
            Ok(None) => batch.push(pair),
            Err(_) => report.consolidate.errors += 1,
        }
    }
    if !batch.is_empty() {
        report.consolidate.judged = count(batch.len());
        report.consolidate.calls += 1;
        report.llm_calls += 1;
        let message = batch
            .iter()
            .enumerate()
            .map(|(index, pair)| line(index + 1, pair, &anchors, &loaded.rows, ctx.session_id))
            .collect::<Vec<_>>()
            .join("\n");
        match llm
            .complete_structured(SYSTEM, &message, &schema(), MAX_TOKENS)
            .await
        {
            Ok(completion) => match parse_reply(&completion.text) {
                Ok(items) => {
                    let mut seen = BTreeSet::new();
                    for (index, verdict, quote) in items {
                        if index == 0 || index > batch.len() || !seen.insert(index) {
                            continue;
                        }
                        let pair = batch[index - 1].clone();
                        let anchor = &anchors[pair.anchor];
                        let _ = cache_put(
                            gm,
                            &cache_key(anchor, &loaded.rows[pair.partner], &pair.evidence),
                            &verdict,
                            &quote,
                        )
                        .await;
                        verdicts.push((pair, verdict, quote, false));
                    }
                }
                Err(()) => report.consolidate.errors += 1,
            },
            Err(_) => report.consolidate.errors += 1,
        }
    }
    let mut accepted = Vec::new();
    for (pair, verdict, quote, _) in verdicts {
        let anchor = &anchors[pair.anchor];
        let partner = &loaded.rows[pair.partner];
        let turn = cited(ctx, anchor);
        let verdict = accept(
            &verdict,
            &quote,
            &pair.evidence,
            &partner.value,
            &anchor.new_value,
            turn,
            &anchor.replaces,
        );
        accepted.push((pair, verdict, fold_ws(&quote)));
    }
    let mut blocked = BTreeSet::new();
    let mut grouped: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, (pair, verdict, quote)) in accepted.iter().enumerate() {
        if verdict == "supersedes" {
            grouped.entry(pair.anchor).or_default().push(index);
            let _ = quote;
        }
    }
    for indexes in grouped.values() {
        let mut quotes: BTreeMap<&str, u32> = BTreeMap::new();
        for index in indexes {
            *quotes.entry(accepted[*index].2.as_str()).or_insert(0) += 1;
        }
        if quotes.values().any(|count| *count >= 2) {
            for index in indexes {
                blocked.insert(*index);
                *report
                    .consolidate
                    .skips
                    .entry("consolidate_ambiguous".into())
                    .or_insert(0) += 1;
            }
        }
    }
    for (index, (pair, verdict, _)) in accepted.iter().enumerate() {
        if blocked.contains(&index) {
            continue;
        }
        if verdict == "supersedes" {
            close_partner(
                gm,
                ctx,
                &anchors,
                &loaded.rows,
                pair,
                "consolidated",
                report,
            )
            .await;
        } else if verdict == "duplicate" {
            close_duplicate(gm, ctx, &anchors, &loaded.rows, pair, report).await;
        }
    }
}

fn rough_anchors(facts: &[super::schema::Fact], stats: &WriteStats) -> Vec<Anchor> {
    let mut anchors = Vec::new();
    for (index, retirement) in stats.retirements.iter().enumerate() {
        if !matches!(
            retirement.reason.as_str(),
            "rejected" | "superseded" | "replaced"
        ) {
            continue;
        }
        let fact = facts.iter().find(|fact| {
            normalise(&fact.subject_name) == normalise(&retirement.subject)
                && ((!retirement.new_value.is_empty()
                    && values_equal(&fact.object_name, &retirement.new_value))
                    || (retirement.reason == "rejected"
                        && values_equal(&fact.object_name, &retirement.old_value)))
        });
        let edge = Predicate::parse(&retirement.predicate).is_some_and(Predicate::is_edge);
        let rejected = retirement.reason == "rejected";
        let (turn, quote, replaces, new_value) = fact.map_or(
            (
                0,
                String::new(),
                String::new(),
                retirement.new_value.clone(),
            ),
            |fact| {
                (
                    fact.turn,
                    fact.quote.clone(),
                    fact.replaces.clone(),
                    if rejected {
                        String::new()
                    } else {
                        fact.object_name.clone()
                    },
                )
            },
        );
        let reason = if rejected {
            "rejected"
        } else if edge {
            "closed-edge"
        } else {
            "replaces"
        };
        anchors.push(Anchor {
            id: format!(
                "a1:{index}:{}:{}:{}",
                retirement.subject, retirement.predicate, retirement.key
            ),
            subject: retirement.subject.clone(),
            pred: retirement.predicate.clone(),
            key: retirement.key.clone(),
            value: retirement.old_value.clone(),
            text: format!("{} {replaces}", retirement.old_value),
            new_value,
            replaces: replaces.clone(),
            entity: if edge {
                retirement.old_value.clone()
            } else {
                replaces
            },
            reason,
            turn,
            quote,
            live: None,
            a2: false,
        });
    }
    for &index in &stats.replaces_unconsumed {
        let Some(fact) = facts.get(index) else {
            continue;
        };
        if fact.status != "decided" {
            continue;
        }
        anchors.push(anchor_from_fact(fact, index, true));
    }
    for (index, fact) in facts.iter().enumerate() {
        if fact.status != "decided"
            || !matches!(fact.predicate, Predicate::Setting | Predicate::Rule)
            || stats.replaces_unconsumed.contains(&index)
            || closed_by(fact, stats)
        {
            continue;
        }
        if fact.key.trim().is_empty() && tokens(&fact.object_name).len() < 3 {
            continue;
        }
        anchors.push(anchor_from_fact(fact, index, false));
    }
    anchors
}

fn anchor_from_fact(fact: &super::schema::Fact, index: usize, a2: bool) -> Anchor {
    Anchor {
        id: format!("{}:{index}", if a2 { "a2" } else { "a3" }),
        subject: fact.subject_name.clone(),
        pred: fact.predicate.as_str().into(),
        key: fact.key.clone(),
        value: fact.object_name.clone(),
        text: format!("{} {}", fact.object_name, fact.replaces),
        new_value: fact.object_name.clone(),
        replaces: fact.replaces.clone(),
        entity: fact.replaces.clone(),
        reason: if a2 { "replaces" } else { "new" },
        turn: fact.turn,
        quote: fact.quote.clone(),
        live: None,
        a2,
    }
}

fn closed_by(fact: &super::schema::Fact, stats: &WriteStats) -> bool {
    stats.retirements.iter().any(|retirement| {
        normalise(&retirement.subject) == normalise(&fact.subject_name)
            && !retirement.new_value.is_empty()
            && values_equal(&retirement.new_value, &fact.object_name)
    })
}

fn finish_anchors(mut anchors: Vec<Anchor>, rows: &[Row]) -> Vec<Anchor> {
    for anchor in &mut anchors {
        anchor.live = rows.iter().position(|row| {
            row.here
                && row.pred == anchor.pred
                && normalise(&row.subject) == normalise(&anchor.subject)
                && values_equal(&row.value, &anchor.new_value)
                && (anchor.key.is_empty()
                    || row.key == anchor.key
                    || keys_match(&row.key, &anchor.key))
        });
    }
    anchors.retain(|anchor| anchor.a2 || anchor.reason != "new" || anchor.live.is_some());
    anchors
}

fn select(anchors: &[Anchor], loaded: &Loaded, ctx: &WriteCtx<'_>) -> (Vec<Pair>, u32) {
    let rows = &loaded.rows;
    let neighbourhood = neighbourhood(anchors, loaded, ctx);
    let mut pairs = Vec::new();
    for (anchor_index, anchor) in anchors.iter().enumerate() {
        let turn = cited(ctx, anchor);
        for (partner_index, partner) in rows.iter().enumerate() {
            if partner.here || !neighbourhood.contains(&partner.entity) {
                continue;
            }
            if anchor.live == Some(partner_index) {
                continue;
            }
            if sentences(turn).any(|sentence| enumerated(&partner.value, sentence)) {
                continue;
            }
            // Identical values are pre-filter (d), not a coexist pair: the same
            // string occupies one span, so the coexist helper would always match.
            if !anchor.new_value.is_empty()
                && !values_equal(&partner.value, &anchor.new_value)
                && sentences(turn)
                    .any(|sentence| coexist(&partner.value, &anchor.new_value, sentence))
            {
                continue;
            }
            let Some(score) = score(anchor, partner) else {
                continue;
            };
            pairs.push(Pair {
                anchor: anchor_index,
                partner: partner_index,
                score,
                evidence: cut_evidence(turn, &[&anchor.value, &anchor.replaces, &anchor.quote]),
                stored: stored_session(&partner.evidence),
            });
        }
    }
    pairs.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| anchors[left.anchor].id.cmp(&anchors[right.anchor].id))
            .then_with(|| rows[left.partner].id.cmp(&rows[right.partner].id))
    });
    let mut kept = Vec::new();
    let mut used: BTreeMap<usize, usize> = BTreeMap::new();
    let mut overflow = 0u32;
    for pair in pairs {
        let count = used.entry(pair.anchor).or_insert(0);
        if *count >= PER_ANCHOR || kept.len() >= CAP {
            overflow += 1;
            continue;
        }
        *count += 1;
        kept.push(pair);
    }
    (kept, overflow)
}

fn classify(
    pair: &Pair,
    anchors: &[Anchor],
    rows: &[Row],
    selected: &[Pair],
    ctx: &WriteCtx<'_>,
) -> Step {
    let anchor = &anchors[pair.anchor];
    let partner = &rows[pair.partner];
    if let Some(live) = anchor.live {
        let mine = &rows[live];
        if normalise(&mine.subject) == normalise(&partner.subject)
            && mine.pred == partner.pred
            && values_equal(&mine.value, &partner.value)
        {
            return Step::Dup;
        }
    }
    if normalise(&anchor.subject) == normalise(&partner.subject)
        && keys_match(&anchor.key, &partner.key)
    {
        return Step::Drop;
    }
    if !anchor.new_value.is_empty() && values_equal(&partner.value, &anchor.new_value) {
        return Step::Drop;
    }
    if anchor.a2
        && replaces_unique(pair, anchors, rows, selected)
        && departs(cited(ctx, anchor), &partner.value, &anchor.replaces)
    {
        return Step::Sup;
    }
    Step::Judge
}

fn replaces_unique(pair: &Pair, anchors: &[Anchor], rows: &[Row], selected: &[Pair]) -> bool {
    let anchor = &anchors[pair.anchor];
    let mine = half_both(
        &tokens(&anchor.replaces),
        &tokens(&rows[pair.partner].value),
    );
    if !mine {
        return false;
    }
    selected
        .iter()
        .filter(|other| {
            other.anchor == pair.anchor
                && half_both(
                    &tokens(&anchor.replaces),
                    &tokens(&rows[other.partner].value),
                )
        })
        .count()
        == 1
}

fn score(anchor: &Anchor, partner: &Row) -> Option<i32> {
    let rule = anchor.pred == "rule" || partner.pred == "rule";
    let left = tokens(&anchor.text);
    let right = tokens(&partner.value);
    let shared = shared(&left, &right);
    let tok = overlap_ok(shared, left.len(), right.len(), rule);
    let names = tokens(&partner.subject);
    let key = key_overlap(&anchor.key, &partner.key, &names);
    let shape = value_class(&anchor.value) == value_class(&partner.value);
    let ent =
        !anchor.entity.trim().is_empty() && !value_spans(&partner.value, &anchor.entity).is_empty();
    if !(tok || (key && shape) || ent) {
        return None;
    }
    let same = !normalise(&anchor.subject).is_empty()
        && normalise(&anchor.subject) == normalise(&partner.subject);
    Some(i32::from(ent) * 3 + i32::from(tok) * 2 + i32::from(key && shape) * 2 + i32::from(same))
}

fn neighbourhood(anchors: &[Anchor], loaded: &Loaded, ctx: &WriteCtx<'_>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if !ctx.project.name.is_empty() {
        names.insert(normalise(&ctx.project.name));
    }
    for alias in &ctx.project.aliases {
        names.insert(normalise(alias));
    }
    for anchor in anchors {
        names.insert(normalise(&anchor.subject));
    }
    names.remove("");
    let mut ids = BTreeSet::new();
    for (id, name) in &loaded.owners {
        if names.contains(&normalise(name)) {
            ids.insert(id.clone());
        }
    }
    let base = ids.clone();
    for (from, to) in &loaded.part_of {
        if base.contains(to) {
            ids.insert(from.clone());
        }
    }
    ids
}

fn accept(
    verdict: &str,
    quote: &str,
    evidence: &str,
    old: &str,
    new_value: &str,
    turn: &str,
    replaces: &str,
) -> String {
    let duplicate = verdict == "duplicate";
    if verdict != "supersedes" && !duplicate {
        return if verdict == "unrelated" {
            "unrelated".into()
        } else {
            "unsure".into()
        };
    }
    let in_evidence = contains_folded(evidence, quote);
    let in_value = duplicate && (contains_folded(old, quote) || contains_folded(new_value, quote));
    if !(in_evidence || in_value) || (verdict == "supersedes" && !departs(turn, old, replaces)) {
        return "unsure".into();
    }
    verdict.to_string()
}

fn departs(turn: &str, old: &str, replaces: &str) -> bool {
    let hit = |value: &str| {
        !value.is_empty() && (mentioned_with_cue(value, turn) || departure_near(value, turn))
    };
    hit(old) || hit(replaces)
}

async fn close_partner(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    anchors: &[Anchor],
    rows: &[Row],
    pair: &Pair,
    reason: &str,
    report: &mut IngestionReport,
) {
    let partner = &rows[pair.partner];
    if partner.here {
        return;
    }
    let Some(until) = session_midnight(ctx.date) else {
        return;
    };
    let ended = format!("{}:{reason}", ctx.session_id);
    let closed = match close_row(gm, partner, &until, &ended).await {
        Ok(closed) => closed,
        Err(_) => {
            report.consolidate.errors += 1;
            return;
        }
    };
    if !closed {
        return;
    }
    if reason == "consolidated" {
        report.consolidate.closes_sup += 1;
    } else {
        report.consolidate.closes_dup += 1;
    }
    let anchor = &anchors[pair.anchor];
    report.retirements.push(Retirement {
        reason: reason.to_string(),
        subject: partner.subject.clone(),
        predicate: partner.pred.clone(),
        key: partner.key.clone(),
        old_value: partner.value.clone(),
        new_value: anchor.new_value.clone(),
    });
}

async fn close_duplicate(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    anchors: &[Anchor],
    rows: &[Row],
    pair: &Pair,
    report: &mut IngestionReport,
) {
    let partner = &rows[pair.partner];
    let Some(live) = anchors[pair.anchor].live else {
        if !partner.here {
            close_partner(gm, ctx, anchors, rows, pair, "duplicate", report).await;
        }
        return;
    };
    let mine = &rows[live];
    let close_partner_row =
        partner.from > mine.from || (partner.from == mine.from && !partner.here);
    if close_partner_row {
        close_partner(gm, ctx, anchors, rows, pair, "duplicate", report).await;
        return;
    }
    if mine.here {
        return;
    }
    let Some(until) = session_midnight(ctx.date) else {
        return;
    };
    let ended = format!("{}:duplicate", ctx.session_id);
    if matches!(close_row(gm, mine, &until, &ended).await, Ok(true)) {
        report.consolidate.closes_dup += 1;
        report.retirements.push(Retirement {
            reason: "duplicate".into(),
            subject: mine.subject.clone(),
            predicate: mine.pred.clone(),
            key: mine.key.clone(),
            old_value: mine.value.clone(),
            new_value: anchors[pair.anchor].new_value.clone(),
        });
    }
}

async fn close_row(
    gm: &GraphMemory,
    row: &Row,
    until: &str,
    ended: &str,
) -> Result<bool, GraphError> {
    if let Some(edge) = &row.edge {
        let mut response = gm
            .db()
            .query(CLOSE_EDGE)
            .bind(("id", edge.clone()))
            .bind(("valid_until", until.to_string()))
            .bind(("ended_by", ended.to_string()))
            .await?;
        let closed: Vec<Value> = deserialize_take(&mut response, 0)?;
        return Ok(!closed.is_empty());
    }
    let Some(index) = row.slot else {
        return Ok(false);
    };
    let mut response = gm
        .db()
        .query("SELECT attributes FROM type::record($id)")
        .bind(("id", row.entity.clone()))
        .await?;
    let found: Vec<Value> = deserialize_take(&mut response, 0)?;
    let Some(mut attributes) = found
        .into_iter()
        .next()
        .and_then(|record| record.get("attributes").cloned())
    else {
        return Ok(false);
    };
    let Some(slots) = attributes
        .get_mut("v2")
        .and_then(Value::as_object_mut)
        .and_then(|v2| v2.get_mut("slots"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(false);
    };
    let Some(slot) = slots.get_mut(index) else {
        return Ok(false);
    };
    if slot.get("value").and_then(Value::as_str) != Some(row.value.as_str()) {
        return Ok(false);
    }
    if !stamp_slot(slot, until, ended) {
        return Ok(false);
    }
    gm.db()
        .query("UPDATE type::record($id) SET attributes = $attributes")
        .bind(("id", row.entity.clone()))
        .bind(("attributes", attributes))
        .await?
        .check()?;
    Ok(true)
}

async fn load_rows(gm: &GraphMemory, session: &str) -> Result<Loaded, GraphError> {
    let mut response = gm.db().query("SELECT * OMIT embedding FROM entity").await?;
    let entities: Vec<Entity> = deserialize_take(&mut response, 0)?;
    let mut response = gm
        .db()
        .query("SELECT id, in, out, rel_type, status, valid_until, valid_from, evidence FROM relates_to")
        .await?;
    let edges: Vec<Value> = deserialize_take(&mut response, 0)?;
    let mut names = BTreeMap::new();
    let mut owners = Vec::new();
    let mut rows = Vec::new();
    for entity in &entities {
        let id = entity.id_string();
        names.insert(id.clone(), entity.name.clone());
        let Some(attributes) = entity.attributes.as_ref() else {
            continue;
        };
        let Some(v2) = attributes.get("v2") else {
            continue;
        };
        if v2
            .get("kind")
            .and_then(Value::as_str)
            .and_then(Kind::parse)
            .is_none()
        {
            continue;
        }
        owners.push((id.clone(), entity.name.clone()));
        let Some(slots) = v2.get("slots").and_then(Value::as_array) else {
            continue;
        };
        for (index, slot) in slots.iter().enumerate() {
            if crud::edge_already_closed(slot.get("valid_until")) {
                continue;
            }
            if slot.get("status").and_then(Value::as_str) != Some("decided") {
                continue;
            }
            rows.push(Row {
                id: format!("s:{id}:{index}"),
                entity: id.clone(),
                slot: Some(index),
                edge: None,
                subject: entity.name.clone(),
                pred: slot
                    .get("predicate")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                key: slot.get("key").and_then(Value::as_str).unwrap_or("").into(),
                value: slot
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                evidence: slot.get("evidence").cloned().unwrap_or(Value::Null),
                from: slot
                    .get("valid_from")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                here: opened_here(slot.get("evidence"), session),
            });
        }
    }
    let mut part_of = Vec::new();
    for edge in &edges {
        if crud::edge_already_closed(edge.get("valid_until")) {
            continue;
        }
        let rel = edge.get("rel_type").and_then(Value::as_str).unwrap_or("");
        let from = id_text(edge.get("in"));
        let to = id_text(edge.get("out"));
        if edge.get("status").and_then(Value::as_str) != Some("decided") {
            continue;
        }
        if rel == "part_of" {
            part_of.push((from.clone(), to.clone()));
        }
        if !Predicate::parse(rel).is_some_and(Predicate::is_edge) {
            continue;
        }
        let evidence = edge.get("evidence").cloned().unwrap_or(Value::Null);
        rows.push(Row {
            id: format!("e:{}", id_text(edge.get("id"))),
            entity: from.clone(),
            slot: None,
            edge: Some(id_text(edge.get("id"))),
            subject: names.get(&from).cloned().unwrap_or_default(),
            pred: rel.to_string(),
            key: String::new(),
            value: names.get(&to).cloned().unwrap_or_default(),
            from: json_text(edge.get("valid_from")),
            here: opened_here(Some(&evidence), session),
            evidence,
        });
    }
    Ok(Loaded {
        rows,
        owners,
        part_of,
    })
}

fn id_text(id: Option<&Value>) -> String {
    match id {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn json_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

async fn define_cache(gm: &GraphMemory) -> Result<(), GraphError> {
    gm.db().query(DEFINE_CACHE).await?.check()?;
    Ok(())
}

async fn cache_get(gm: &GraphMemory, key: &str) -> Result<Option<(String, String)>, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT verdict, quote FROM consolidation_cache WHERE key = $key")
        .bind(("key", key.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(rows.into_iter().next().and_then(|row| {
        Some((
            row.get("verdict")?.as_str()?.to_string(),
            row.get("quote")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ))
    }))
}

async fn cache_put(
    gm: &GraphMemory,
    key: &str,
    verdict: &str,
    quote: &str,
) -> Result<(), GraphError> {
    gm.db()
        .query("CREATE consolidation_cache SET key = $key, verdict = $verdict, quote = $quote, prompt = $prompt, at = time::now()")
        .bind(("key", key.to_string()))
        .bind(("verdict", verdict.to_string()))
        .bind(("quote", quote.to_string()))
        .bind(("prompt", PROMPT.to_string()))
        .await?
        .check()?;
    Ok(())
}

fn cache_key(anchor: &Anchor, partner: &Row, evidence: &str) -> String {
    let raw = format!(
        "{}\n{}\n{}\n{PROMPT}",
        normalise(&anchor.text),
        normalise(&partner.value),
        normalise(evidence)
    );
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

fn line(index: usize, pair: &Pair, anchors: &[Anchor], rows: &[Row], session: &str) -> String {
    let anchor = &anchors[pair.anchor];
    let partner = &rows[pair.partner];
    format!(
        "{index} | {}:{} OLD=\"{}\" (stored {}) | NEW=\"{}\" ({session}, reason: {}) | EVIDENCE=\"{}\"",
        partner.subject,
        partner.key,
        clip(&partner.value, 160),
        pair.stored,
        clip(&anchor.new_value, 160),
        anchor.reason,
        clip(&pair.evidence, 240)
    )
}

fn schema() -> Value {
    json!({"type":"object","required":["pairs"],"properties":{"pairs":{"type":"array","items":{"type":"object","required":["i","verdict","quote"],"properties":{"i":{"type":"integer"},"verdict":{"enum":["supersedes","duplicate","unrelated","unsure"]},"quote":{"type":"string","maxLength":240}}}}}})
}

fn parse_reply(text: &str) -> Result<Vec<(usize, String, String)>, ()> {
    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    let pairs = value.get("pairs").and_then(Value::as_array).ok_or(())?;
    let mut out = Vec::new();
    for item in pairs {
        let Some(index) = item.get("i").and_then(Value::as_u64) else {
            continue;
        };
        let Some(verdict) = item.get("verdict").and_then(Value::as_str) else {
            continue;
        };
        out.push((
            usize::try_from(index).unwrap_or(usize::MAX),
            verdict.to_string(),
            item.get("quote")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ));
    }
    Ok(out)
}

fn cited<'a>(ctx: &'a WriteCtx<'_>, anchor: &'a Anchor) -> &'a str {
    ctx.turns
        .iter()
        .find(|turn| turn.index == anchor.turn)
        .map(|turn| turn.text.as_str())
        .unwrap_or(anchor.quote.as_str())
}

fn cut_evidence(turn: &str, needles: &[&str]) -> String {
    let chars: Vec<char> = turn.chars().collect();
    if chars.len() <= 240 {
        return turn.to_string();
    }
    let mut at = 0usize;
    for needle in needles {
        if let Some((start, _)) = value_spans(turn, needle).into_iter().next() {
            at = turn[..start].chars().count();
            break;
        }
    }
    let end = (at + 160).min(chars.len());
    let start = end.saturating_sub(240);
    chars[start..end].iter().collect()
}

fn tokens(text: &str) -> Vec<String> {
    alphanumeric_runs(text)
        .into_iter()
        .filter(|token| token.chars().count() >= 3 && !is_stop_word(token))
        .map(plural_fold)
        .filter(|token| !is_stop_word(token))
        .collect()
}

fn plural_fold(token: String) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() >= 4 && chars.last() == Some(&'s') && chars[chars.len() - 2] != 's' {
        chars[..chars.len() - 1].iter().collect()
    } else {
        token
    }
}

fn shared(left: &[String], right: &[String]) -> usize {
    let right: BTreeSet<&str> = right.iter().map(String::as_str).collect();
    left.iter()
        .filter(|token| right.contains(token.as_str()))
        .collect::<BTreeSet<_>>()
        .len()
}

fn overlap_ok(shared: usize, left: usize, right: usize, rule: bool) -> bool {
    let smaller = left.min(right);
    let floor = if rule { 2 } else { 1 };
    smaller > 0 && shared >= floor && shared * 2 >= smaller
}

fn half_both(left: &[String], right: &[String]) -> bool {
    let shared = shared(left, right);
    !left.is_empty() && !right.is_empty() && shared * 2 >= left.len() && shared * 2 >= right.len()
}

fn key_overlap(left: &str, right: &str, names: &[String]) -> bool {
    let keep = |token: &String| !names.iter().any(|name| name == token);
    let left: BTreeSet<String> = key_tokens(left).into_iter().filter(keep).collect();
    key_tokens(right)
        .into_iter()
        .any(|token| left.contains(&token))
}

fn value_class(value: &str) -> &'static str {
    let value = value.trim();
    if regex_is(
        r"(?i)^\d+(?:\.\d+)?\s*(?:ms|sec|secs|second|seconds|min|mins|minute|minutes|hr|hrs|hour|hours|day|days|[smhd])$",
        value,
    ) {
        return "duration";
    }
    if regex_is(r"(?i)^v?\d+\.\d+(?:\.\d+)?(?:[-+][A-Za-z0-9.]+)?$", value) {
        return "version";
    }
    if regex_is(r"^[+-]?\d+(?:\.\d+)?$", value) {
        return "number";
    }
    if regex_is(r"(?i)(?:/|\\|\{\{|\{[A-Za-z0-9_]+\}|YYYY)", value) {
        return "path";
    }
    if regex_is(r"(?i)(?:^|\s)--?[A-Za-z]|&&|\|\||`[^`]+`", value) {
        return "command";
    }
    if regex_is(r"^[A-Za-z_][A-Za-z0-9_-]*$", value) {
        return "identifier";
    }
    "prose"
}

fn regex_is(pattern: &str, value: &str) -> bool {
    regex::Regex::new(pattern).is_ok_and(|pattern| pattern.is_match(value))
}

fn contains_folded(haystack: &str, needle: &str) -> bool {
    let needle = fold_ws(needle);
    !needle.is_empty() && fold_ws(haystack).contains(&needle)
}

fn fold_ws(text: &str) -> String {
    let mut out = String::new();
    let mut spaced = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            spaced = true;
            continue;
        }
        if spaced && !out.is_empty() {
            out.push(' ');
        }
        spaced = false;
        out.push(ch);
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn stored_session(evidence: &Value) -> String {
    evidence
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item.get("session"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(all(test, feature = "embedded"))]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::super::input::{ProjectIdentity, Turn, TurnRole};
    use super::super::schema::{Fact, Kind, Predicate};
    use super::super::write::{ensure_v2_store, write_facts, WriteCtx};
    use super::*;
    use crate::graph::confidence::Provenance;
    use crate::graph::ingest::IngestContext;
    use crate::graph::llm::Completion;
    use crate::graph::GraphMemory;

    struct Quiet;

    #[async_trait]
    impl LlmProvider for Quiet {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            panic!("resolver called");
        }
    }

    #[derive(Clone)]
    enum Answer {
        Panic,
        Err,
        Bad,
        Reply(String),
    }

    struct Stub {
        answer: Mutex<Answer>,
        calls: AtomicU32,
        tokens: AtomicU32,
        message: Mutex<String>,
    }

    impl Stub {
        fn new(answer: Answer) -> Self {
            Self {
                answer: Mutex::new(answer),
                calls: AtomicU32::new(0),
                tokens: AtomicU32::new(0),
                message: Mutex::new(String::new()),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for Stub {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            panic!("plain complete was called");
        }

        async fn complete_structured(
            &self,
            _: &str,
            user_message: &str,
            _: &Value,
            max_tokens: u32,
        ) -> Result<Completion, GraphError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.tokens.store(max_tokens, Ordering::SeqCst);
            *self.message.lock().expect("message") = user_message.to_string();
            match self.answer.lock().expect("answer").clone() {
                Answer::Panic => panic!("judge called"),
                Answer::Err => Err(GraphError::Llm("down".into())),
                Answer::Bad => Ok(Completion::unmeasured("not json")),
                Answer::Reply(body) => Ok(Completion::unmeasured(body)),
            }
        }
    }

    struct Split {
        answer: Answer,
        calls: AtomicU32,
        tokens: AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for Split {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            panic!("plain complete was called");
        }

        async fn complete_structured_chat(
            &self,
            _: &str,
            _: &[crate::graph::llm::Message],
            _: &Value,
            _: u32,
        ) -> Result<Completion, GraphError> {
            Ok(Completion::unmeasured(
                r#"{"facts":[{"subject":"shelf","subject_type":"project","predicate":"setting","role":"none","key":"cover","object":"linen cover","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Ship the shelf with a linen cover.","confidence":0.9}],"bugs":[],"releases":[]}"#,
            ))
        }

        async fn complete_structured(
            &self,
            _: &str,
            _: &str,
            _: &Value,
            max_tokens: u32,
        ) -> Result<Completion, GraphError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.tokens.store(max_tokens, Ordering::SeqCst);
            match &self.answer {
                Answer::Err => Err(GraphError::Llm("down".into())),
                Answer::Bad => Ok(Completion::unmeasured("not json")),
                Answer::Panic => panic!("judge called"),
                Answer::Reply(body) => Ok(Completion::unmeasured(body.clone())),
            }
        }
    }

    fn project() -> ProjectIdentity {
        ProjectIdentity {
            name: "acme".into(),
            aliases: Vec::new(),
        }
    }

    fn turn(text: &str) -> Turn {
        Turn {
            index: 0,
            role: TurnRole::User,
            text: text.into(),
        }
    }

    fn setting(subject: &str, kind: Kind, key: &str, value: &str, quote: &str) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind: kind,
            predicate: Predicate::Setting,
            role: None,
            key: key.into(),
            object_name: value.into(),
            object_kind: None,
            status: "decided".into(),
            turn: 0,
            quote: quote.into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    fn edge(
        predicate: Predicate,
        subject: &str,
        kind: Kind,
        object: &str,
        object_kind: Kind,
    ) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind: kind,
            predicate,
            role: None,
            key: String::new(),
            object_name: object.into(),
            object_kind: Some(object_kind),
            status: "decided".into(),
            turn: 0,
            quote: format!("{subject} links {object}"),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    async fn open() -> (tempfile::TempDir, GraphMemory) {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        ensure_v2_store(&gm).await.unwrap();
        (tmp, gm)
    }

    async fn commit(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
    ) -> WriteStats {
        let project = project();
        let ctx = WriteCtx::new(&Quiet, &project, session, date, turns);
        write_facts(gm, &ctx, facts).await.unwrap()
    }

    async fn pass(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        stats: &WriteStats,
        llm: &Stub,
    ) -> IngestionReport {
        let project = project();
        let ctx = WriteCtx::new(llm, &project, session, date, turns);
        let mut report = IngestionReport::default();
        run(gm, &ctx, facts, stats, llm, &mut report).await;
        report
    }

    async fn slots(gm: &GraphMemory, name: &str) -> Vec<Value> {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities.iter().find(|entity| entity.name == name).unwrap();
        entity.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn ended(slot: &Value) -> Option<&str> {
        slot.get("ended_by").and_then(Value::as_str)
    }

    async fn cache_defined(gm: &GraphMemory) -> bool {
        let mut response = gm.db().query("INFO FOR DB").await.unwrap();
        let rows: Vec<Value> = deserialize_take(&mut response, 0).unwrap();
        serde_json::to_string(&rows)
            .unwrap_or_default()
            .contains("consolidation_cache")
    }

    fn judged(items: &[(usize, &str, &str)]) -> String {
        let pairs: Vec<Value> = items
            .iter()
            .map(|(index, verdict, quote)| json!({"i": index, "verdict": verdict, "quote": quote}))
            .collect();
        serde_json::to_string(&json!({"pairs": pairs})).unwrap()
    }

    #[tokio::test]
    async fn no_anchors_does_not_call_or_define_the_table() {
        let (_tmp, gm) = open().await;
        let facts = [edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "redis",
            Kind::Technology,
        )];
        let stats = commit(&gm, "s1", "2026-03-01", &[turn("acme links redis")], &facts).await;
        let stub = Stub::new(Answer::Panic);
        let report = pass(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("acme links redis")],
            &facts,
            &stats,
            &stub,
        )
        .await;
        assert_eq!(stub.calls(), 0);
        assert!(report.consolidate.is_blank());
        assert!(!cache_defined(&gm).await);
    }

    #[tokio::test]
    async fn prefilter_replaces_token_closes_without_a_call() {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            "pool",
            "logical cores",
            "pool is logical cores",
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let quote = "Use 4 workers instead of the core count.";
        let mut next = setting("acme", Kind::Project, "workers", "4", quote);
        next.replaces = "core count".into();
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        assert!(
            stats.replaces_unconsumed.contains(&0),
            "replaces closed a row: {:?}",
            stats.retirements
        );
        let stub = Stub::new(Answer::Panic);
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 0);
        assert_eq!(report.consolidate.prefilter.sup, 1);
        assert_eq!(report.consolidate.closes_sup, 1);
        assert_eq!(report.consolidate.calls, 0);
        let rows = slots(&gm, "acme").await;
        assert_eq!(
            ended(live_any(&rows, "pool", "logical cores")),
            Some("s2:consolidated")
        );
    }

    fn live_any<'a>(rows: &'a [Value], key: &str, value: &str) -> &'a Value {
        rows.iter()
            .find(|slot| slot["key"] == key && slot["value"] == value)
            .unwrap_or_else(|| panic!("missing {key}={value}"))
    }

    #[tokio::test]
    async fn equal_values_are_not_superseded() {
        let (_tmp, gm) = open().await;
        let link = edge(
            Predicate::PartOf,
            "picker",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let old = setting(
            "picker",
            Kind::Component,
            "title",
            "north",
            "title is north",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("picker is part of acme. title is north")],
            &[link, old],
        )
        .await;
        let quote = "Use north instead of south.";
        let next = setting("acme", Kind::Project, "label", "north", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(
            1,
            "supersedes",
            "Use north instead of south.",
        )])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 0, "{}", stub.message.lock().unwrap());
        assert_eq!(report.consolidate.closes_sup, 0);
        assert_eq!(report.consolidate.prefilter.drop, 1);
        assert!(ended(live_any(&slots(&gm, "picker").await, "title", "north")).is_none());
    }

    #[tokio::test]
    async fn verbatim_quote_and_cue_closes_only_the_stamp() {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            "format",
            "plain text",
            "format is plain text",
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let quote = "Switch to rich text instead of plain text.";
        let next = setting("acme", Kind::Project, "layout", "rich text", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let before = live_any(&slots(&gm, "acme").await, "format", "plain text").clone();
        let alpha = before["alpha"].clone();
        let beta = before["beta"].clone();
        let stub = Stub::new(Answer::Reply(judged(&[(
            1,
            "supersedes",
            "instead of plain text",
        )])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 1);
        assert_eq!(stub.tokens.load(Ordering::SeqCst), 400);
        assert_eq!(report.llm_calls, 1);
        assert_eq!(report.consolidate.closes_sup, 1);
        let after = live_any(&slots(&gm, "acme").await, "format", "plain text").clone();
        assert_eq!(ended(&after), Some("s2:consolidated"));
        assert_eq!(after["alpha"], alpha);
        assert_eq!(after["beta"], beta);
        let mut left = before.as_object().unwrap().clone();
        let mut right = after.as_object().unwrap().clone();
        left.remove("valid_until");
        left.remove("ended_by");
        right.remove("valid_until");
        right.remove("ended_by");
        assert_eq!(left, right);
        assert_eq!(after["valid_until"], "2026-03-02T00:00:00Z");
    }

    #[tokio::test]
    async fn quote_missing_from_evidence_closes_nothing() {
        let report = judge_layout(
            "instead of plain text is not what was said",
            "Switch to rich text instead of plain text.",
        )
        .await;
        assert_eq!(report.0.consolidate.closes_sup, 0);
        assert_eq!(report.0.consolidate.calls, 1);
        assert!(ended(&report.1).is_none());
    }

    #[tokio::test]
    async fn missing_departure_cue_closes_nothing() {
        let report = judge_layout(
            "plain text",
            "The layout is rich text while plain text is still stored.",
        )
        .await;
        assert_eq!(report.0.consolidate.closes_sup, 0);
        assert_eq!(report.0.consolidate.calls, 1);
        assert!(ended(&report.1).is_none());
    }

    async fn judge_layout(quote: &str, turn_text: &str) -> (IngestionReport, Value) {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            "format",
            "plain text",
            "format is plain text",
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let next = setting("acme", Kind::Project, "layout", "rich text", turn_text);
        let turns = [turn(turn_text)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        let slot = live_any(&slots(&gm, "acme").await, "format", "plain text").clone();
        (report, slot)
    }

    #[tokio::test]
    async fn two_supersedes_on_one_quote_close_nothing() {
        let (_tmp, gm) = open().await;
        let plank = setting(
            "acme",
            Kind::Project,
            "plank",
            "cedar plank",
            "plank is cedar plank",
        );
        let board = setting(
            "acme",
            Kind::Project,
            "board",
            "cedar board",
            "board is cedar board",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("cedar plank and cedar board")],
            &[plank, board],
        )
        .await;
        let quote =
            "Use cedar beam instead of cedar plank. Prefer cedar beam instead of cedar board.";
        let next = setting("acme", Kind::Project, "beam", "cedar beam", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[
            (1, "supersedes", "instead of cedar"),
            (2, "supersedes", "instead of cedar"),
        ])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 1, "{}", stub.message.lock().unwrap());
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(
            report
                .consolidate
                .skips
                .get("consolidate_ambiguous")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        let rows = slots(&gm, "acme").await;
        assert!(ended(live_any(&rows, "plank", "cedar plank")).is_none());
        assert!(ended(live_any(&rows, "board", "cedar board")).is_none());
    }

    #[tokio::test]
    async fn a_partner_outside_the_neighbourhood_is_not_paired() {
        let (_tmp, gm) = open().await;
        let link = edge(
            Predicate::PartOf,
            "picker",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let inside = setting(
            "picker",
            Kind::Component,
            "swatch",
            "marigold token",
            "swatch is marigold token",
        );
        let outside = setting(
            "bravo",
            Kind::Project,
            "swatch",
            "marigold token",
            "swatch is marigold token",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("picker is part of acme")],
            &[link, inside, outside],
        )
        .await;
        let quote = "Use marigold signal instead of marigold token.";
        let next = setting("acme", Kind::Project, "hue", "marigold signal", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[
            (1, "supersedes", "instead of marigold token"),
            (2, "supersedes", "instead of marigold token"),
        ])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        let message = stub.message.lock().unwrap().clone();
        assert!(message.contains("picker:"), "{message}");
        assert!(!message.contains("bravo"), "{message}");
        assert_eq!(report.consolidate.closes_sup, 1);
        assert!(ended(live_any(
            &slots(&gm, "picker").await,
            "swatch",
            "marigold token"
        ))
        .is_some());
        assert!(ended(live_any(
            &slots(&gm, "bravo").await,
            "swatch",
            "marigold token"
        ))
        .is_none());
    }

    #[tokio::test]
    async fn cap_sends_six_of_nine_in_a_stable_order() {
        let first = capped(9).await;
        let second = capped(9).await;
        assert_eq!(first.message, second.message);
        assert_eq!(first.calls, 1);
        assert_eq!(first.overflow, 3);
        assert_eq!(first.judged, 6);
        let lines: Vec<&str> = first.message.lines().collect();
        assert_eq!(lines.len(), 6);
        for (index, line) in lines.iter().enumerate() {
            assert!(line.contains(&format!("crimson0{index}")));
        }
        for index in 6..9 {
            assert!(!first.message.contains(&format!("crimson0{index}")));
        }
    }

    #[tokio::test]
    async fn four_pairs_make_one_structured_call() {
        let run = capped(4).await;
        assert_eq!(run.calls, 1);
        assert_eq!(run.tokens, 400);
        assert_eq!(run.message.lines().count(), 4);
        assert_eq!(run.overflow, 0);
    }

    struct Capped {
        message: String,
        calls: u32,
        tokens: u32,
        overflow: u32,
        judged: u32,
    }

    async fn capped(count: usize) -> Capped {
        let (_tmp, gm) = open().await;
        let mut old = Vec::new();
        let mut next = Vec::new();
        for index in 0..count {
            let token = format!("crimson0{index}");
            old.push(setting(
                "acme",
                Kind::Project,
                &format!("p0{index}"),
                &format!("{token} marker"),
                "stored",
            ));
            next.push(setting(
                "acme",
                Kind::Project,
                &format!("n0{index}"),
                &format!("{token} banner"),
                "noted",
            ));
        }
        commit(&gm, "s1", "2026-03-01", &[turn("stored")], &old).await;
        let turns = [turn("noted")];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, &next).await;
        let body = judged(
            &(0..count)
                .map(|index| (index + 1, "unrelated", "noted"))
                .collect::<Vec<_>>(),
        );
        let stub = Stub::new(Answer::Reply(body));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &next, &stats, &stub).await;
        let message = stub.message.lock().unwrap().clone();
        Capped {
            message,
            calls: stub.calls(),
            tokens: stub.tokens.load(Ordering::SeqCst),
            overflow: report.consolidate.overflow,
            judged: report.consolidate.judged,
        }
    }

    #[tokio::test]
    async fn a_cached_pair_is_not_judged_again() {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            "old",
            "quartz marker",
            "old is quartz marker",
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let quote = "Keep the quartz beacon for now.";
        let first = setting("acme", Kind::Project, "new", "quartz beacon", quote);
        let turns = [turn(quote)];
        let stats = commit(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            std::slice::from_ref(&first),
        )
        .await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "unrelated", "quartz beacon")])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[first], &stats, &stub).await;
        assert_eq!(stub.calls(), 1);
        assert_eq!(report.consolidate.cache_hits, 0);
        let again = setting("acme", Kind::Project, "newer", "quartz beacon", quote);
        let stats = commit(
            &gm,
            "s3",
            "2026-03-03",
            &turns,
            std::slice::from_ref(&again),
        )
        .await;
        let stub = Stub::new(Answer::Panic);
        let report = pass(&gm, "s3", "2026-03-03", &turns, &[again], &stats, &stub).await;
        assert_eq!(stub.calls(), 0);
        assert!(report.consolidate.cache_hits >= 1);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(live_any(&slots(&gm, "acme").await, "old", "quartz marker")).is_none());
    }

    #[tokio::test]
    async fn opened_here_and_enumerated_partners_are_not_paired() {
        let (_tmp, gm) = open().await;
        let listed = setting("acme", Kind::Project, "ink", "scarlet", "ink is scarlet");
        commit(&gm, "s1", "2026-03-01", &[turn(&listed.quote)], &[listed]).await;
        let quote = "Keep scarlet and blue and green. Violet marker sits beside violet banner.";
        let left = setting("acme", Kind::Project, "left", "violet marker", quote);
        let right = setting("acme", Kind::Project, "right", "violet banner", quote);
        let dye = setting("acme", Kind::Project, "dye", "scarlet shade", quote);
        let turns = [turn(quote)];
        let facts = [left, right, dye];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, &facts).await;
        let stub = Stub::new(Answer::Panic);
        let report = pass(&gm, "s2", "2026-03-02", &turns, &facts, &stats, &stub).await;
        assert_eq!(stub.calls(), 0);
        assert_eq!(report.consolidate.pairs, 0);
        assert!(ended(live_any(&slots(&gm, "acme").await, "ink", "scarlet")).is_none());
    }

    #[tokio::test]
    async fn provider_error_and_bad_json_close_nothing() {
        for answer in [Answer::Err, Answer::Bad] {
            let (_tmp, gm) = open().await;
            let old = setting(
                "acme",
                Kind::Project,
                "format",
                "plain text",
                "format is plain text",
            );
            commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
            let quote = "Switch to rich text instead of plain text.";
            let next = setting("acme", Kind::Project, "layout", "rich text", quote);
            let turns = [turn(quote)];
            let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
            let stub = Stub::new(answer);
            let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
            assert_eq!(stub.calls(), 1);
            assert_eq!(report.consolidate.errors, 1);
            assert_eq!(report.consolidate.closes_sup, 0);
            assert!(ended(live_any(&slots(&gm, "acme").await, "format", "plain text")).is_none());
        }
    }

    #[tokio::test]
    async fn ingest_survives_a_judge_error_and_bad_json() {
        for answer in [Answer::Err, Answer::Bad] {
            let tmp = tempfile::tempdir().unwrap();
            let memory = tmp.path();
            std::fs::write(
                memory.join(".recall-echo.toml"),
                "[graph]\nconsolidate = true\n",
            )
            .unwrap();
            std::fs::create_dir_all(memory.join("conversations")).unwrap();
            std::fs::write(
                memory.join("conversations/conversation-002.md"),
                "---\nlog: 2\ndate: \"2026-04-02T00:00:00Z\"\nsession_id: \"s2\"\n---\n\n### User\n\nShip the shelf with a linen cover.\n",
            )
            .unwrap();
            let gm = GraphMemory::open_embedded(&memory.join("graph"))
                .await
                .unwrap();
            ensure_v2_store(&gm).await.unwrap();
            let old = setting(
                "shelf",
                Kind::Project,
                "wrap",
                "linen wrap",
                "wrap is linen wrap",
            );
            let shelf = ProjectIdentity {
                name: "shelf".into(),
                aliases: Vec::new(),
            };
            let spoken = [turn(&old.quote)];
            let ctx = WriteCtx::new(&Quiet, &shelf, "s1", "2026-04-01", &spoken);
            write_facts(&gm, &ctx, &[old]).await.unwrap();
            let provider = Split {
                answer,
                calls: AtomicU32::new(0),
                tokens: AtomicU32::new(0),
            };
            let report = super::super::pass1::extract_session(
                &gm,
                memory,
                2,
                &IngestContext::new("s2", Some(2)),
                &provider,
            )
            .await
            .unwrap();
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(report.consolidate.errors, 1);
            assert_eq!(report.consolidate.closes_sup, 0);
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert!(ended(live_any(&slots(&gm, "shelf").await, "wrap", "linen wrap")).is_none());
        }
    }
}
