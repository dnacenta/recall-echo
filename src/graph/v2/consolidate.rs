// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opt-in pass after a session's writes. It stamps a live row the writer
//! could not see. It does not write evidence or edit a value.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::super::crud;
use super::super::error::GraphError;
use super::super::llm::LlmProvider;
use super::super::types::{Entity, IngestionReport, Retirement};
use super::super::{deserialize_take, GraphMemory};
use super::enumeration::enumerated;
use super::ground::{is_stop_word, sentences};
use super::known_match::{coexist, cue_words, key_tokens, mention, value_spans};
use super::schema::{normalise, Kind, Predicate};
use super::write::{
    alphanumeric_runs, keys_match, opened_here, session_midnight, stamp_slot, values_equal,
    WriteCtx, WriteStats, CLOSE_EDGE, REJECTED, REPLACED, SUPERSEDED,
};

const PROMPT: &str = "CONSOLIDATE_V2";
const NEIGHBOUR_CAP: usize = 48;
const ROW_CAP: usize = 256;
const CAP: usize = 6;
const PER_ANCHOR: usize = 2;
const MAX_TOKENS: u32 = 400;
const DROP_QUOTE_SHORT: &str = "quote too short";
const DROP_QUOTE_MISSING: &str = "quote not verbatim in the evidence";
const DROP_NO_CUE: &str = "no departure cue";
const DROP_SECOND: &str = "second supersede on the same anchor";
const DROP_NO_COPY: &str = "no kept live copy (duplicate)";
const DROP_OPENED_HERE: &str = "row opened this session";
const DROP_GONE: &str = "anchor or pair no longer present";
const SYSTEM: &str = "\
You compare stored project facts. For each numbered pair, OLD is a stored fact and NEW is the newer evidence. Answer one verdict:
- supersedes: the evidence shows OLD no longer holds because NEW replaced it.
- duplicate: same fact, same meaning.
- unrelated: both can be true.
- unsure.
Copy quote verbatim from the EVIDENCE text that shows the change; if you cannot, answer unrelated or unsure. Do not use outside knowledge. Two values that can both hold (different settings, different components, a list) are unrelated.
Reply with one JSON object and nothing else (no prose, no markdown fence): {\"pairs\":[{\"i\":1,\"verdict\":\"supersedes\",\"quote\":\"old line\"},{\"i\":2,\"verdict\":\"duplicate\",\"quote\":\"same line\"},{\"i\":3,\"verdict\":\"unrelated\",\"quote\":\"\"},{\"i\":4,\"verdict\":\"unsure\",\"quote\":\"\"}]}";

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
    pending: bool,
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
    /// Row id of the unique writer close that produced this anchor.
    closed_row: Option<String>,
}

#[derive(Clone)]
struct Pair {
    anchor: usize,
    partner: usize,
    score: i32,
    evidence: String,
    stored: String,
    hold: bool,
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
    let rough = rough_anchors(facts, stats, ctx.components_on && ctx.links_on);
    let ambiguous = if ctx.hold_on {
        match super::hold::settle(gm, ctx, facts, report).await {
            Ok(open) => open,
            Err(err) => {
                eprintln!("hold error: {err}");
                report.hold.errors += 1;
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    // Dependents of this session's writer closes, outside the neighbourhood cap.
    let dependents = derived_dependents(gm, ctx, stats, report).await;
    if ctx.components_on {
        arm_component_counts(&mut report.consolidate);
    }
    if rough.is_empty() && ambiguous.is_empty() && dependents.is_empty() {
        return;
    }
    if define_cache(gm).await.is_err() {
        report.consolidate.errors += 1;
        report.hold.errors += count(ambiguous.len());
        return;
    }
    let mut names = wanted_names(&rough, ctx);
    for hold in &ambiguous {
        if names.len() >= NEIGHBOUR_CAP {
            break;
        }
        if !names.iter().any(|name| name == &hold.subject) {
            names.push(hold.subject.clone());
        }
    }
    let Ok(loaded) = load_rows(gm, ctx.session_id, &names, &dependents).await else {
        report.consolidate.errors += 1;
        report.hold.errors += count(ambiguous.len());
        return;
    };
    let mut anchors = finish_anchors(rough, &loaded.rows);
    let (added, overflow) =
        push_derived_anchors(&mut anchors, &loaded.rows, &dependents, facts, stats);
    report.links.d_anchors_added = added;
    report.links.d_anchor_overflow = overflow;
    if anchors.is_empty() && ambiguous.is_empty() {
        return;
    }
    report.consolidate.anchors = count(anchors.len());
    let (selected, overflow) = select(&anchors, &loaded, ctx);
    report.consolidate.pairs = count(selected.len());
    report.consolidate.overflow = overflow;
    // Rows closed so far this pass. A kept copy must still be live.
    let mut gone: BTreeSet<usize> = BTreeSet::new();
    let mut waiting = Vec::new();
    for pair in &selected {
        match classify(pair, &anchors, &loaded.rows, &selected, ctx) {
            Step::Dup => {
                report.consolidate.prefilter.dup += 1;
                if let Some(reason) =
                    close_duplicate(gm, ctx, &anchors, &loaded.rows, pair, &mut gone, report).await
                {
                    note_drop(
                        report,
                        reason,
                        &anchors,
                        &loaded.rows,
                        pair,
                        &anchors[pair.anchor].quote,
                    );
                }
            }
            Step::Sup => {
                report.consolidate.prefilter.sup += 1;
                let before = report.consolidate.closes_sup;
                let reason = close_partner(
                    gm,
                    ctx,
                    &anchors,
                    &loaded.rows,
                    pair,
                    CloseWhy {
                        reason: "consolidated",
                        quote: None,
                        tally: CueTally::none(),
                    },
                    report,
                )
                .await;
                if report.consolidate.closes_sup > before {
                    gone.insert(pair.partner);
                } else if let Some(reason) = reason {
                    note_drop(
                        report,
                        reason,
                        &anchors,
                        &loaded.rows,
                        pair,
                        &anchors[pair.anchor].quote,
                    );
                }
            }
            Step::Drop => report.consolidate.prefilter.drop += 1,
            Step::Judge => waiting.push(pair.clone()),
        }
    }
    let added = attach_holds(ctx, &ambiguous, &mut anchors, &loaded, &mut waiting, report);
    report.consolidate.pairs = report.consolidate.pairs.saturating_add(added);
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
        let (cue, tally) = accept_cue(
            ctx,
            anchor,
            partner,
            &loaded,
            &dependents,
            CueText {
                turn,
                quote: quote.as_str(),
            },
        );
        match accept(
            &verdict,
            &quote,
            &pair.evidence,
            &partner.value,
            &anchor.new_value,
            cue,
        ) {
            Ok(verdict) => accepted.push((pair, verdict.to_string(), quote, tally)),
            Err(reason) => note_drop(report, reason, &anchors, &loaded.rows, &pair, &quote),
        }
    }
    let mut blocked = BTreeSet::new();
    let mut grouped: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, (pair, verdict, _, _)) in accepted.iter().enumerate() {
        if verdict == "supersedes" {
            grouped.entry(pair.anchor).or_default().push(index);
        }
    }
    for indexes in grouped.values() {
        // Two supersedes for one anchor close nothing, whatever the quotes.
        if indexes.len() >= 2 {
            for index in indexes {
                blocked.insert(*index);
                *report
                    .consolidate
                    .skips
                    .entry("consolidate_ambiguous".into())
                    .or_insert(0) += 1;
                let (pair, _, quote, _) = &accepted[*index];
                note_drop(report, DROP_SECOND, &anchors, &loaded.rows, pair, quote);
            }
        }
    }
    for (index, (pair, verdict, quote, tally)) in accepted.iter().enumerate() {
        if blocked.contains(&index) {
            continue;
        }
        if verdict == "supersedes" {
            // A partner closed earlier in this batch already has one live
            // successor. Clearing another hold against it would make a second.
            if pair.hold && gone.contains(&pair.partner) {
                note_drop(report, DROP_GONE, &anchors, &loaded.rows, pair, quote);
                continue;
            }
            let taken = if pair.hold {
                confirm_held(gm, &anchors, &loaded, pair, report).await
            } else {
                None
            };
            if pair.hold && taken.is_none() {
                continue;
            }
            let before = report.consolidate.closes_sup;
            let reason = close_partner(
                gm,
                ctx,
                &anchors,
                &loaded.rows,
                pair,
                CloseWhy {
                    reason: "consolidated",
                    quote: Some(quote.as_str()),
                    tally: *tally,
                },
                report,
            )
            .await;
            if report.consolidate.closes_sup > before {
                if ctx.components_on {
                    count_kept(report, tally);
                }
                gone.insert(pair.partner);
                if pair.hold {
                    report.hold.holds_confirmed += 1;
                    report.hold.holds_ambiguous = report.hold.holds_ambiguous.saturating_sub(1);
                }
            } else if pair.hold {
                if let Some(pending) = taken {
                    if let Some(index) = anchors[pair.anchor].live {
                        let reference = ref_of(&loaded.rows[index]);
                        let anchor = &anchors[pair.anchor];
                        let until = session_midnight(ctx.date).unwrap_or_default();
                        let dropped_by = format!("{}:hold_dropped", ctx.session_id);
                        if let Ok(super::hold::PendingRecover::Dropped) =
                            super::hold::recover_pending(
                                gm,
                                &super::hold::PendingTarget {
                                    new_ref: &reference,
                                    pred: &anchor.pred,
                                    key: &anchor.key,
                                    new_value: &anchor.new_value,
                                },
                                &pending,
                                &until,
                                &dropped_by,
                            )
                            .await
                        {
                            report.hold.holds_dropped += 1;
                        }
                    }
                }
                report.hold.errors += 1;
            } else if let Some(reason) = reason {
                note_drop(report, reason, &anchors, &loaded.rows, pair, quote);
            }
        } else if verdict == "duplicate" {
            if let Some(reason) =
                close_duplicate(gm, ctx, &anchors, &loaded.rows, pair, &mut gone, report).await
            {
                note_drop(report, reason, &anchors, &loaded.rows, pair, quote);
            }
        }
    }
}

fn rough_anchors(
    facts: &[super::schema::Fact],
    stats: &WriteStats,
    link_rows: bool,
) -> Vec<Anchor> {
    let mut anchors = Vec::new();
    push_retired(&mut anchors, facts, stats, link_rows);
    push_unconsumed(&mut anchors, facts, stats);
    push_new_anchors(&mut anchors, facts, stats);
    anchors
}

fn push_retired(
    anchors: &mut Vec<Anchor>,
    facts: &[super::schema::Fact],
    stats: &WriteStats,
    link_rows: bool,
) {
    for (index, retirement) in stats.retirements.iter().enumerate() {
        if !writer_a1(&retirement.reason) {
            continue;
        }
        let mut anchor = a1_anchor(index, retirement, facts);
        if link_rows {
            anchor.closed_row = unique_closed_row(retirement, facts, stats);
        }
        anchors.push(anchor);
    }
}

fn push_unconsumed(anchors: &mut Vec<Anchor>, facts: &[super::schema::Fact], stats: &WriteStats) {
    for &index in &stats.replaces_unconsumed {
        let Some(fact) = facts.get(index) else {
            continue;
        };
        if fact.status == "decided" {
            anchors.push(anchor_from_fact(fact, index, true));
        }
    }
}

fn push_new_anchors(anchors: &mut Vec<Anchor>, facts: &[super::schema::Fact], stats: &WriteStats) {
    for (index, fact) in facts.iter().enumerate() {
        if new_anchor(fact, index, stats) {
            anchors.push(anchor_from_fact(fact, index, false));
        }
    }
}

fn new_anchor(fact: &super::schema::Fact, index: usize, stats: &WriteStats) -> bool {
    if !open_setting_or_rule(fact, index, stats) {
        return false;
    }
    !fact.key.trim().is_empty() || tokens(&fact.object_name).len() >= 3
}

fn open_setting_or_rule(fact: &super::schema::Fact, index: usize, stats: &WriteStats) -> bool {
    fact.status == "decided"
        && matches!(fact.predicate, Predicate::Setting | Predicate::Rule)
        && !stats.replaces_unconsumed.contains(&index)
        && !closed_by(fact, stats)
}

fn a1_anchor(index: usize, retirement: &Retirement, facts: &[super::schema::Fact]) -> Anchor {
    let fact = retirement_fact(retirement, facts);
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
    Anchor {
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
        closed_row: None,
    }
}

fn retirement_fact<'a>(
    retirement: &Retirement,
    facts: &'a [super::schema::Fact],
) -> Option<&'a super::schema::Fact> {
    facts
        .iter()
        .find(|fact| fact_matches_retirement(fact, retirement))
}

fn fact_matches_retirement(fact: &super::schema::Fact, retirement: &Retirement) -> bool {
    normalise(&fact.subject_name) == normalise(&retirement.subject)
        && value_matches_retirement(fact, retirement)
}

fn value_matches_retirement(fact: &super::schema::Fact, retirement: &Retirement) -> bool {
    if !retirement.new_value.is_empty() && values_equal(&fact.object_name, &retirement.new_value) {
        return true;
    }
    retirement.reason == "rejected" && values_equal(&fact.object_name, &retirement.old_value)
}

fn unique_closed_row(
    retirement: &Retirement,
    facts: &[super::schema::Fact],
    stats: &WriteStats,
) -> Option<String> {
    if !writer_a1(&retirement.reason) {
        return None;
    }
    let fact_index = sole_fact_index(retirement, facts)?;
    if !one_retirement(fact_index, facts, stats) {
        return None;
    }
    let events = a1_events(fact_index, &stats.close_events);
    let [event] = events.as_slice() else {
        return None;
    };
    if event.reason != retirement.reason {
        return None;
    }
    Some(event.row_id.clone())
}

fn sole_fact_index(retirement: &Retirement, facts: &[super::schema::Fact]) -> Option<usize> {
    let mut found = None;
    for (index, fact) in facts.iter().enumerate() {
        if !fact_matches_retirement(fact, retirement) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(index);
    }
    found
}

fn a1_events(
    fact_index: usize,
    events: &[super::write::CloseEvent],
) -> Vec<&super::write::CloseEvent> {
    events
        .iter()
        .filter(|event| event.fact_index == fact_index && writer_a1(&event.reason))
        .collect()
}

fn one_retirement(fact_index: usize, facts: &[super::schema::Fact], stats: &WriteStats) -> bool {
    stats
        .retirements
        .iter()
        .filter(|other| {
            writer_a1(&other.reason) && sole_fact_index(other, facts) == Some(fact_index)
        })
        .count()
        == 1
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
        closed_row: None,
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
            if partner.pending || partner.here || !neighbourhood.contains(&partner.entity) {
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
                hold: false,
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
    cue: bool,
) -> Result<&'static str, &'static str> {
    let duplicate = verdict == "duplicate";
    if verdict != "supersedes" && !duplicate {
        return Ok(if verdict == "unrelated" {
            "unrelated"
        } else {
            "unsure"
        });
    }
    if !quote_span(quote) {
        return Err(DROP_QUOTE_SHORT);
    }
    let in_evidence = contains_folded(evidence, quote);
    let in_value = duplicate && (contains_folded(old, quote) || contains_folded(new_value, quote));
    if !(in_evidence || in_value) {
        return Err(DROP_QUOTE_MISSING);
    }
    if verdict == "supersedes" && !cue {
        return Err(DROP_NO_CUE);
    }
    Ok(if duplicate { "duplicate" } else { "supersedes" })
}

/// Count one dropped verdict and print the drop line on stderr.
///
/// The line is the reason, the stored row's subject and key, the anchor's
/// subject and key, and the first 80 characters of the quote. It does not
/// carry the evidence or the old and new values.
fn note_drop(
    report: &mut IngestionReport,
    reason: &'static str,
    anchors: &[Anchor],
    rows: &[Row],
    pair: &Pair,
    quote: &str,
) {
    *report
        .consolidate
        .dropped
        .entry(reason.to_string())
        .or_insert(0) += 1;
    let partner = &rows[pair.partner];
    let anchor = &anchors[pair.anchor];
    eprintln!(
        "{}",
        drop_line(
            reason,
            &format!("{}:{}", partner.subject, partner.key),
            &format!("{}:{}", anchor.subject, anchor.key),
            quote
        )
    );
}

/// Stderr line for one drop. `old_key` is the stored row, `new_key` the anchor.
fn drop_line(reason: &str, old_key: &str, new_key: &str, quote: &str) -> String {
    format!(
        "consolidate drop: {reason} | {old_key} | {new_key} | {}",
        quote_head(quote)
    )
}

/// First 80 characters of `quote`, with any control character (line break,
/// escape) turned into a space so a quote cannot forge or recolour log lines.
fn quote_head(quote: &str) -> String {
    quote
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(80)
        .collect()
}

/// A quote has to be a real span of stored text, not one letter of it.
fn quote_span(quote: &str) -> bool {
    let folded = fold_ws(quote);
    folded.chars().count() >= 12 && alphanumeric_runs(&folded).len() >= 2
}

/// Another loaded row still carries `value` and has not been closed in this pass.
fn kept_copy(rows: &[Row], gone: &BTreeSet<usize>, closing: usize, value: &str) -> bool {
    rows.iter().enumerate().any(|(index, row)| {
        index != closing && !gone.contains(&index) && values_equal(&row.value, value)
    })
}

fn wanted_names(anchors: &[Anchor], ctx: &WriteCtx<'_>) -> Vec<String> {
    let mut names = BTreeSet::new();
    if !ctx.project.name.is_empty() {
        names.insert(ctx.project.name.clone());
    }
    for alias in &ctx.project.aliases {
        if !alias.is_empty() {
            names.insert(alias.clone());
        }
    }
    for anchor in anchors {
        if !anchor.subject.is_empty() {
            names.insert(anchor.subject.clone());
        }
    }
    names.into_iter().take(NEIGHBOUR_CAP).collect()
}

fn departs(turn: &str, old: &str, replaces: &str) -> bool {
    super::hold::departs(turn, old, replaces)
}

/// Replacement phrasing in the quote, when the old value is not in that quote.
///
/// `instead` is already a departure word, so it is not repeated here. A list
/// item is not a replacement. The phrase has to be in the quote itself.
fn replacement_quote(quote: &str, old: &str, new_value: &str, key: &str) -> bool {
    let words = cue_words(quote);
    replacement_guards(quote, old, new_value, key, &words)
        && plain_shape(quote, key, &words)
        && quote_replaces(quote)
}

fn replacement_guards(
    quote: &str,
    old: &str,
    new_value: &str,
    key: &str,
    words: &[(String, usize, usize)],
) -> bool {
    !names_value(old, quote)
        && names_value(new_value, quote)
        && !listed_new(new_value, quote)
        && !hedged(quote, words)
        && names_key(key, words)
        && !tentative(words)
}

/// A question, a negation, a condition, a hedge, or an addition is not a
/// decided replacement, wherever it sits in the quote.
fn hedged(quote: &str, words: &[(String, usize, usize)]) -> bool {
    quote.contains('?')
        || words.iter().any(|(word, _, _)| {
            word.ends_with("n't")
                || matches!(
                    word.as_str(),
                    "not"
                        | "no"
                        | "never"
                        | "nor"
                        | "neither"
                        | "if"
                        | "unless"
                        | "whether"
                        | "maybe"
                        | "perhaps"
                        | "might"
                        | "could"
                        | "would"
                        | "should"
                        | "may"
                        | "suppose"
                        | "suggest"
                        | "propose"
                        | "proposal"
                        | "idea"
                        | "consider"
                        | "also"
                        | "too"
                        | "well"
                        | "additionally"
                        | "available"
                        | "plus"
                )
        })
}

/// Reported speech, a trial, a revert, or a time limit is not a decision.
fn tentative(words: &[(String, usize, usize)]) -> bool {
    words
        .windows(2)
        .any(|pair| pair[0].0 == "for" && pair[1].0 == "now")
        || words.iter().any(|(word, _, _)| {
            matches!(
                word.as_str(),
                "said"
                    | "says"
                    | "say"
                    | "told"
                    | "thinks"
                    | "think"
                    | "thought"
                    | "proposes"
                    | "proposed"
                    | "suggests"
                    | "suggested"
                    | "tried"
                    | "trying"
                    | "try"
                    | "testing"
                    | "trial"
                    | "temporarily"
                    | "temporary"
                    | "briefly"
                    | "only"
                    | "until"
                    | "but"
                    | "reverted"
                    | "revert"
                    | "later"
            )
        })
}

/// Each cue is the head of its sentence: before `is now`, `are now` or
/// `now uses` sit only articles, `default`, the key's own tokens, and at most
/// one other word right after a key token (`the layout store`). `new <word>:`
/// needs `<word>` to be a key token. Another subject, a speaker or a time
/// phrase in front of the cue rejects the quote.
fn plain_shape(quote: &str, key: &str, words: &[(String, usize, usize)]) -> bool {
    let tokens = key_tokens(key);
    let mut any = false;
    for (index, pair) in words.windows(2).enumerate() {
        match shape_step(quote, words, index, pair, &tokens) {
            ShapeStep::Keep => any = true,
            ShapeStep::Skip => {}
            ShapeStep::Reject => return false,
        }
    }
    any
}

enum ShapeStep {
    Keep,
    Skip,
    Reject,
}

fn shape_step(
    quote: &str,
    words: &[(String, usize, usize)],
    index: usize,
    pair: &[(String, usize, usize)],
    tokens: &[String],
) -> ShapeStep {
    if new_label(quote, pair) {
        return label_step(&pair[1].0, tokens);
    }
    if !adjacent_cue(pair) {
        return ShapeStep::Skip;
    }
    if cue_head(quote, words, index, tokens) {
        ShapeStep::Keep
    } else {
        ShapeStep::Reject
    }
}

fn label_step(word: &str, tokens: &[String]) -> ShapeStep {
    if token_is(tokens, word) {
        ShapeStep::Keep
    } else {
        ShapeStep::Reject
    }
}

fn adjacent_cue(pair: &[(String, usize, usize)]) -> bool {
    matches!(
        (pair[0].0.as_str(), pair[1].0.as_str()),
        ("is" | "are", "now") | ("now", "uses")
    )
}

fn cue_head(
    quote: &str,
    words: &[(String, usize, usize)],
    index: usize,
    tokens: &[String],
) -> bool {
    let mut from = index;
    while from > 0 && !sentence_start(quote, words[from].1) {
        from -= 1;
    }
    head_allows(&words[from..index], tokens)
}

/// Articles, `default`, the key's tokens, and one extra word after a key token.
fn head_allows(words: &[(String, usize, usize)], tokens: &[String]) -> bool {
    let mut seen_key = false;
    let mut extra = false;
    for (word, _, _) in words {
        if token_is(tokens, word) {
            seen_key = true;
            continue;
        }
        if !head_word(word, seen_key, extra) {
            return false;
        }
        if !head_filler(word) {
            extra = true;
        }
    }
    seen_key
}

fn token_is(tokens: &[String], word: &str) -> bool {
    tokens.iter().any(|token| token == word)
}

fn head_word(word: &str, seen_key: bool, extra: bool) -> bool {
    if head_filler(word) {
        return !(seen_key && !extra && word != "default");
    }
    seen_key && !extra
}

fn head_filler(word: &str) -> bool {
    matches!(word, "the" | "a" | "an" | "our" | "its" | "default")
}

/// The quote names the setting being replaced: a token of the anchor's key.
fn names_key(key: &str, words: &[(String, usize, usize)]) -> bool {
    let tokens = key_tokens(key);
    !tokens.is_empty() && words.iter().any(|(word, _, _)| tokens.contains(word))
}

/// A quote with any list mark is not a sole replacement.
///
/// A comma, or `and`, `or`, `nor`, or `plus`, is enough. The new value does
/// not have to be one of the listed items. Addition words such as `also` are
/// rejected by [`hedged`], not here.
fn listed_new(value: &str, quote: &str) -> bool {
    !value.is_empty() && list_mark(quote)
}

fn list_mark(quote: &str) -> bool {
    // Any list mark counts, whether or not the value itself is enumerated.
    quote.contains(',')
        || quote.split_whitespace().any(|word| {
            let word = word
                .trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_lowercase();
            matches!(word.as_str(), "and" | "or" | "nor" | "plus")
        })
}

fn names_value(value: &str, text: &str) -> bool {
    !value.is_empty() && !value_spans(text, value).is_empty()
}

fn same_slot(anchor: &Anchor, partner: &Row) -> bool {
    let left = slot_tokens(&anchor.key, &partner.subject);
    let right = slot_tokens(&partner.key, &anchor.subject);
    !left.is_empty() && left == right
}

fn slot_tokens(key: &str, subject: &str) -> BTreeSet<String> {
    let names = tokens(subject);
    key_tokens(key)
        .into_iter()
        .filter(|token| !names.iter().any(|name| name == token))
        .filter(|token| !slot_unit(token))
        .collect()
}

/// Duration and size words peeled from a slot.
///
/// `min` and `mins` stay. `coverage_min` is a different slot from `coverage`.
fn slot_unit(token: &str) -> bool {
    matches!(
        token,
        "ms" | "sec"
            | "secs"
            | "second"
            | "seconds"
            | "minute"
            | "minutes"
            | "hr"
            | "hrs"
            | "hour"
            | "hours"
            | "day"
            | "days"
            | "byte"
            | "bytes"
            | "kb"
            | "kib"
            | "mb"
            | "mib"
            | "gb"
            | "gib"
            | "pct"
            | "percent"
    )
}

fn quote_unit(token: &str) -> bool {
    token == "min" || token == "mins" || slot_unit(token)
}

fn numeric_pair(anchor: &Anchor, partner: &Row) -> bool {
    if anchor.new_value.is_empty() || partner.value.is_empty() {
        return false;
    }
    if values_equal(&anchor.new_value, &partner.value) {
        return false;
    }
    let class = value_class(&anchor.new_value);
    class == value_class(&partner.value) && numeric_class(class)
}

fn numeric_class(class: &str) -> bool {
    matches!(class, "number" | "duration" | "version")
}

fn trailing_quote(quote: &str, old: &str, new_value: &str, key: &str) -> bool {
    let words = cue_words(quote);
    replacement_guards(quote, old, new_value, key, &words)
        && trailing_shape(quote, new_value, key, &words)
}

fn trailing_shape(
    quote: &str,
    new_value: &str,
    key: &str,
    words: &[(String, usize, usize)],
) -> bool {
    let tokens = key_tokens(key);
    value_spans(quote, new_value)
        .iter()
        .any(|&(start, end)| trailing_span(quote, &tokens, words, start, end))
}

fn trailing_span(
    quote: &str,
    tokens: &[String],
    words: &[(String, usize, usize)],
    start: usize,
    end: usize,
) -> bool {
    let Some(verb) = word_ending_at(words, quote, start) else {
        return false;
    };
    if !matches!(words[verb].0.as_str(), "is" | "are") {
        return false;
    }
    let Some(now) = now_after_value(quote, words, end) else {
        return false;
    };
    sentence_stops(quote, words[now].2) && head_allows(sentence_head(quote, words, verb), tokens)
}

fn word_ending_at(words: &[(String, usize, usize)], quote: &str, start: usize) -> Option<usize> {
    words.iter().enumerate().rev().find_map(|(index, word)| {
        (word.2 <= start && blank_between(quote, word.2, start)).then_some(index)
    })
}

fn now_after_value(quote: &str, words: &[(String, usize, usize)], end: usize) -> Option<usize> {
    let next = words.iter().position(|word| word.1 >= end)?;
    if !blank_between(quote, end, words[next].1) {
        return None;
    }
    if words[next].0 == "now" {
        return Some(next);
    }
    if !quote_unit(&words[next].0) {
        return None;
    }
    let now = next + 1;
    let follows = words.get(now)?;
    (follows.0 == "now" && blank_between(quote, words[next].2, follows.1)).then_some(now)
}

fn sentence_head<'a>(
    quote: &str,
    words: &'a [(String, usize, usize)],
    verb: usize,
) -> &'a [(String, usize, usize)] {
    let mut from = verb;
    while from > 0 && !sentence_start(quote, words[from].1) {
        from -= 1;
    }
    &words[from..verb]
}

fn sentence_stops(quote: &str, end: usize) -> bool {
    let rest = quote[end..].trim_start_matches([' ', '\t']);
    matches!(rest.chars().next(), None | Some('.' | '!' | '\n'))
}

fn blank_between(quote: &str, from: usize, to: usize) -> bool {
    from <= to && quote[from..to].chars().all(char::is_whitespace)
}

/// Whole-phrase replacement wording, case-insensitive.
///
/// `now`, `new`, and `also` alone are not cues. `new <word>:` counts only at
/// the start of the quote or of a sentence.
fn quote_replaces(quote: &str) -> bool {
    let words = cue_words(quote);
    phrase_pair(&words, "is", "now")
        || phrase_pair(&words, "are", "now")
        || phrase_pair(&words, "now", "uses")
        || new_label(quote, &words)
}

fn phrase_pair(words: &[(String, usize, usize)], first: &str, second: &str) -> bool {
    words
        .windows(2)
        .any(|pair| pair[0].0 == first && pair[1].0 == second)
}

fn new_label(quote: &str, words: &[(String, usize, usize)]) -> bool {
    words.iter().enumerate().any(|(index, (word, start, _))| {
        word == "new"
            && sentence_start(quote, *start)
            && words
                .get(index + 1)
                .is_some_and(|(_, _, end)| colon_after(quote, *end))
    })
}

fn sentence_start(quote: &str, start: usize) -> bool {
    let before = quote[..start].trim_end();
    before.is_empty()
        || matches!(
            before.as_bytes().last().copied(),
            Some(b'.' | b'!' | b'?' | b'\n')
        )
}

fn colon_after(quote: &str, end: usize) -> bool {
    quote[end..].trim_start().starts_with(':')
}

/// Same subject and key, or a `part_of` component and that same key pairing.
fn shares_pair(
    anchor: &Anchor,
    partner: &Row,
    part_of: &[(String, String)],
    owners: &[(String, String)],
) -> bool {
    let names = tokens(&partner.subject);
    let key =
        keys_match(&anchor.key, &partner.key) || key_overlap(&anchor.key, &partner.key, &names);
    key && (same_name(&anchor.subject, &partner.subject)
        || component_link(anchor, partner, part_of, owners))
}

fn same_name(left: &str, right: &str) -> bool {
    let left = normalise(left);
    !left.is_empty() && left == normalise(right)
}

fn component_link(
    anchor: &Anchor,
    partner: &Row,
    part_of: &[(String, String)],
    owners: &[(String, String)],
) -> bool {
    let Some(anchor_id) = owner_id(&anchor.subject, owners) else {
        return false;
    };
    let Some(partner_id) = owner_id(&partner.subject, owners) else {
        return false;
    };
    part_of.iter().any(|(from, to)| {
        let from = from.as_str();
        let to = to.as_str();
        (from == partner_id && to == anchor_id) || (from == anchor_id && to == partner_id)
    })
}

struct CueText<'a> {
    turn: &'a str,
    quote: &'a str,
}

struct CueInputs<'a> {
    depart: bool,
    quoted: bool,
    quote: &'a str,
}

enum CueBranch {
    Sibling,
    Derived,
    Trailing,
}

#[derive(Clone, Copy)]
struct CueTally {
    sibling: bool,
    derived: bool,
    trailing: bool,
}

impl CueTally {
    fn none() -> Self {
        Self {
            sibling: false,
            derived: false,
            trailing: false,
        }
    }
}

fn accept_cue(
    ctx: &WriteCtx<'_>,
    anchor: &Anchor,
    partner: &Row,
    loaded: &Loaded,
    dependents: &[DependentHit],
    text: CueText<'_>,
) -> (bool, CueTally) {
    let inputs = CueInputs {
        depart: departs(text.turn, &partner.value, &anchor.replaces),
        quoted: replacement_quote(text.quote, &partner.value, &anchor.new_value, &anchor.key),
        quote: text.quote,
    };
    if !ctx.components_on {
        return (
            plain_cue(anchor, partner, loaded, inputs.depart, inputs.quoted),
            CueTally::none(),
        );
    }
    component_cue(ctx, anchor, partner, loaded, dependents, inputs)
}

fn plain_cue(anchor: &Anchor, partner: &Row, loaded: &Loaded, depart: bool, quoted: bool) -> bool {
    let paired = shares_pair(anchor, partner, &loaded.part_of, &loaded.owners);
    depart || (paired && quoted)
}

fn component_cue(
    ctx: &WriteCtx<'_>,
    anchor: &Anchor,
    partner: &Row,
    loaded: &Loaded,
    dependents: &[DependentHit],
    inputs: CueInputs<'_>,
) -> (bool, CueTally) {
    let kind = widened_cue(ctx, anchor, partner, loaded, dependents);
    let trailing = trailing_allowed(&kind.links, anchor, partner, inputs.quote);
    let accept = joined_cue(inputs.depart, kind.paired, inputs.quoted, trailing);
    let tally = kept_tally(
        &kind.links,
        cue_branch(&kind.links, inputs.quoted, trailing),
        accept && !inputs.depart && kind.paired,
    );
    (accept, tally)
}

fn kept_tally(links: &PairLinks, branch: Option<CueBranch>, made: bool) -> CueTally {
    let Some(branch) = branch.filter(|_| made) else {
        return CueTally::none();
    };
    CueTally {
        trailing: matches!(branch, CueBranch::Trailing),
        sibling: sibling_branch(links, &branch),
        derived: derived_branch(links, &branch),
    }
}

fn sibling_branch(links: &PairLinks, branch: &CueBranch) -> bool {
    matches!(branch, CueBranch::Sibling)
        || (matches!(branch, CueBranch::Trailing) && links.sibling && !links.same && !links.direct)
}

fn derived_branch(links: &PairLinks, branch: &CueBranch) -> bool {
    matches!(branch, CueBranch::Derived)
        || (matches!(branch, CueBranch::Trailing)
            && links.derived
            && !links.same
            && !links.direct
            && !links.sibling)
}

fn cue_branch(links: &PairLinks, quoted: bool, trailing: bool) -> Option<CueBranch> {
    quote_branch(quoted, trailing).or_else(|| open_link(links))
}

fn quote_branch(quoted: bool, trailing: bool) -> Option<CueBranch> {
    (trailing && !quoted).then_some(CueBranch::Trailing)
}

fn open_link(links: &PairLinks) -> Option<CueBranch> {
    if links.same || links.direct {
        return None;
    }
    if links.sibling {
        return Some(CueBranch::Sibling);
    }
    links.derived.then_some(CueBranch::Derived)
}

fn joined_cue(depart: bool, paired: bool, quoted: bool, trailing: bool) -> bool {
    depart || (paired && (quoted || trailing))
}

fn count_kept(report: &mut IngestionReport, tally: &CueTally) {
    if tally.sibling {
        add_count(&mut report.consolidate.link_sibling);
    }
    if tally.derived {
        add_count(&mut report.consolidate.link_derived);
    }
    if tally.trailing {
        add_count(&mut report.consolidate.quote_trailing);
    }
}

struct Widened {
    paired: bool,
    links: PairLinks,
}

fn widened_cue(
    ctx: &WriteCtx<'_>,
    anchor: &Anchor,
    partner: &Row,
    loaded: &Loaded,
    dependents: &[DependentHit],
) -> Widened {
    let links = pair_links(anchor, partner, loaded, dependents, ctx);
    let names = tokens(&partner.subject);
    let key =
        keys_match(&anchor.key, &partner.key) || key_overlap(&anchor.key, &partner.key, &names);
    Widened {
        paired: key && (links.same || links.direct || links.sibling || links.derived),
        links,
    }
}

fn trailing_allowed(links: &PairLinks, anchor: &Anchor, partner: &Row, quote: &str) -> bool {
    links.waiver()
        && same_slot(anchor, partner)
        && numeric_pair(anchor, partner)
        && trailing_quote(quote, &partner.value, &anchor.new_value, &anchor.key)
}

struct PairLinks {
    same: bool,
    direct: bool,
    sibling: bool,
    derived: bool,
}

impl PairLinks {
    fn waiver(&self) -> bool {
        !self.same && (self.direct || self.sibling || self.derived)
    }
}

fn pair_links(
    anchor: &Anchor,
    partner: &Row,
    loaded: &Loaded,
    dependents: &[DependentHit],
    ctx: &WriteCtx<'_>,
) -> PairLinks {
    PairLinks {
        same: same_name(&anchor.subject, &partner.subject),
        direct: component_link(anchor, partner, &loaded.part_of, &loaded.owners),
        sibling: sibling_link(
            anchor,
            partner,
            &loaded.rows,
            &loaded.part_of,
            &loaded.owners,
            ctx.project,
        ),
        derived: derived_link(anchor, partner, dependents, ctx.links_on),
    }
}

fn sibling_link(
    anchor: &Anchor,
    partner: &Row,
    rows: &[Row],
    part_of: &[(String, String)],
    owners: &[(String, String)],
    project: &super::input::ProjectIdentity,
) -> bool {
    let Some(anchor_id) = anchor_entity(anchor, rows, owners) else {
        return false;
    };
    let partner_id = partner.entity.as_str();
    if anchor_id == partner_id {
        return false;
    }
    shared_project_parent(anchor_id, partner_id, part_of, owners, project)
}

fn anchor_entity<'a>(
    anchor: &Anchor,
    rows: &'a [Row],
    owners: &'a [(String, String)],
) -> Option<&'a str> {
    if let Some(row) = anchor.live.and_then(|index| rows.get(index)) {
        return Some(row.entity.as_str());
    }
    unique_owner(&anchor.subject, owners)
}

fn unique_owner<'a>(name: &str, owners: &'a [(String, String)]) -> Option<&'a str> {
    let name = normalise(name);
    let mut found = None;
    for (id, owned) in owners {
        if normalise(owned) != name {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(id.as_str());
    }
    found
}

fn shared_project_parent(
    anchor_id: &str,
    partner_id: &str,
    part_of: &[(String, String)],
    owners: &[(String, String)],
    project: &super::input::ProjectIdentity,
) -> bool {
    project_owner_ids(project, owners)
        .iter()
        .any(|parent| edge_to(anchor_id, parent, part_of) && edge_to(partner_id, parent, part_of))
}

fn edge_to(child: &str, parent: &str, part_of: &[(String, String)]) -> bool {
    part_of
        .iter()
        .any(|(from, to)| from == child && to == parent)
}

fn project_owner_ids<'a>(
    project: &super::input::ProjectIdentity,
    owners: &'a [(String, String)],
) -> Vec<&'a str> {
    let mut ids = Vec::new();
    push_owner(&mut ids, &project.name, owners);
    for alias in &project.aliases {
        push_owner(&mut ids, alias, owners);
    }
    ids
}

fn push_owner<'a>(ids: &mut Vec<&'a str>, name: &str, owners: &'a [(String, String)]) {
    let Some(id) = owner_id(name, owners) else {
        return;
    };
    if !ids.contains(&id) {
        ids.push(id);
    }
}

fn derived_link(
    anchor: &Anchor,
    partner: &Row,
    dependents: &[DependentHit],
    links_on: bool,
) -> bool {
    if !links_on {
        return false;
    }
    let Some(closed) = anchor.closed_row.as_deref() else {
        return false;
    };
    let canonical = ref_of(partner);
    dependents
        .iter()
        .any(|hit| hit_closes(hit, &canonical, closed))
}

fn hit_closes(hit: &DependentHit, canonical: &str, closed: &str) -> bool {
    hit.canonical == canonical && hit.derived_from.iter().any(|id| id == closed)
}

fn arm_component_counts(report: &mut crate::graph::types::ConsolidateReport) {
    report.link_sibling.get_or_insert(0);
    report.link_derived.get_or_insert(0);
    report.quote_trailing.get_or_insert(0);
    report.closes_component.get_or_insert(0);
}

fn add_count(slot: &mut Option<u32>) {
    *slot = Some(slot.unwrap_or(0).saturating_add(1));
}

fn owner_id<'a>(name: &str, owners: &'a [(String, String)]) -> Option<&'a str> {
    let name = normalise(name);
    owners
        .iter()
        .find(|(_, owned)| normalise(owned) == name)
        .map(|(id, _)| id.as_str())
}

struct CloseWhy<'a> {
    reason: &'a str,
    quote: Option<&'a str>,
    tally: CueTally,
}

async fn close_partner(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    anchors: &[Anchor],
    rows: &[Row],
    pair: &Pair,
    why: CloseWhy<'_>,
    report: &mut IngestionReport,
) -> Option<&'static str> {
    let partner = &rows[pair.partner];
    if partner.here {
        return Some(DROP_OPENED_HERE);
    }
    let until = session_midnight(ctx.date)?;
    let ended = format!("{}:{}", ctx.session_id, why.reason);
    let closed = match close_row(gm, partner, &until, &ended).await {
        Ok(closed) => closed,
        Err(_) => {
            report.consolidate.errors += 1;
            return None;
        }
    };
    if !closed {
        return Some(DROP_GONE);
    }
    if why.reason == "consolidated" {
        report.consolidate.closes_sup += 1;
        note_component_close(ctx, anchors, rows, pair, why.quote, why.tally, report);
    } else {
        report.consolidate.closes_dup += 1;
    }
    let anchor = &anchors[pair.anchor];
    report.retirements.push(Retirement {
        reason: why.reason.to_string(),
        subject: partner.subject.clone(),
        predicate: partner.pred.clone(),
        key: partner.key.clone(),
        old_value: partner.value.clone(),
        new_value: anchor.new_value.clone(),
    });
    None
}

fn note_component_close(
    ctx: &WriteCtx<'_>,
    anchors: &[Anchor],
    rows: &[Row],
    pair: &Pair,
    quote: Option<&str>,
    tally: CueTally,
    report: &mut IngestionReport,
) {
    if !ctx.components_on {
        return;
    }
    let anchor = &anchors[pair.anchor];
    let partner = &rows[pair.partner];
    if !same_name(&anchor.subject, &partner.subject) {
        add_count(&mut report.consolidate.closes_component);
    }
    let Some(quote) = quote else {
        return;
    };
    eprintln!(
        "{}",
        component_close_line(
            close_tag(&tally, same_name(&anchor.subject, &partner.subject)),
            &format!("{}:{}", partner.subject, partner.key),
            &format!("{}:{}", anchor.subject, anchor.key),
            quote,
        )
    );
}

fn close_tag(tally: &CueTally, same: bool) -> &'static str {
    if tally.trailing {
        return "trailing";
    }
    if tally.sibling {
        return "sibling";
    }
    if tally.derived {
        return "derived";
    }
    if same {
        return "same subject";
    }
    "part_of"
}

fn component_close_line(kind: &str, old_key: &str, new_key: &str, quote: &str) -> String {
    format!(
        "consolidate close: component {kind} | {old_key} | {new_key} | {}",
        quote_head(quote)
    )
}

async fn close_duplicate(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    anchors: &[Anchor],
    rows: &[Row],
    pair: &Pair,
    gone: &mut BTreeSet<usize>,
    report: &mut IngestionReport,
) -> Option<&'static str> {
    let partner = &rows[pair.partner];
    if gone.contains(&pair.partner) {
        return Some(DROP_GONE);
    }
    let Some(live) = anchors[pair.anchor].live else {
        // No kept copy of this value. A duplicate close would drop the only
        // live row. That needs the supersede path, which checks quote and cue.
        if partner.here {
            return Some(DROP_OPENED_HERE);
        }
        if kept_copy(rows, gone, pair.partner, &partner.value) {
            let before = report.consolidate.closes_dup;
            let reason = close_partner(
                gm,
                ctx,
                anchors,
                rows,
                pair,
                CloseWhy {
                    reason: "duplicate",
                    quote: None,
                    tally: CueTally::none(),
                },
                report,
            )
            .await;
            if report.consolidate.closes_dup > before {
                gone.insert(pair.partner);
                return None;
            }
            return reason;
        }
        *report
            .consolidate
            .skips
            .entry("consolidate_dup_no_copy".into())
            .or_insert(0) += 1;
        return Some(DROP_NO_COPY);
    };
    let mine = &rows[live];
    if gone.contains(&live) || !values_equal(&mine.value, &partner.value) {
        *report
            .consolidate
            .skips
            .entry("consolidate_dup_no_copy".into())
            .or_insert(0) += 1;
        return Some(DROP_NO_COPY);
    }
    let close_partner_row =
        partner.from > mine.from || (partner.from == mine.from && !partner.here);
    if close_partner_row {
        let before = report.consolidate.closes_dup;
        let reason = close_partner(
            gm,
            ctx,
            anchors,
            rows,
            pair,
            CloseWhy {
                reason: "duplicate",
                quote: None,
                tally: CueTally::none(),
            },
            report,
        )
        .await;
        if report.consolidate.closes_dup > before {
            gone.insert(pair.partner);
            return None;
        }
        return reason;
    }
    if mine.here {
        return Some(DROP_OPENED_HERE);
    }
    let until = session_midnight(ctx.date)?;
    let ended = format!("{}:duplicate", ctx.session_id);
    match close_row(gm, mine, &until, &ended).await {
        Ok(true) => {
            report.consolidate.closes_dup += 1;
            gone.insert(live);
            report.retirements.push(Retirement {
                reason: "duplicate".into(),
                subject: mine.subject.clone(),
                predicate: mine.pred.clone(),
                key: mine.key.clone(),
                old_value: mine.value.clone(),
                new_value: anchors[pair.anchor].new_value.clone(),
            });
            None
        }
        Ok(false) => Some(DROP_GONE),
        Err(_) => {
            report.consolidate.errors += 1;
            None
        }
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

async fn load_rows(
    gm: &GraphMemory,
    session: &str,
    names: &[String],
    extra: &[DependentHit],
) -> Result<Loaded, GraphError> {
    let mut entities = load_named(gm, names).await?;
    let mut seen: BTreeSet<String> = entities.iter().map(Entity::id_string).collect();
    let seeds: Vec<String> = seen.iter().cloned().collect();
    let mut part_links = Vec::new();
    for id in &seeds {
        if part_links.len() >= NEIGHBOUR_CAP {
            break;
        }
        let mut response = gm
            .db()
            .query(
                "SELECT id, in FROM type::record($id)<-(relates_to WHERE rel_type = 'part_of' AND status = 'decided' AND valid_until = NONE) ORDER BY id LIMIT 16",
            )
            .bind(("id", id.clone()))
            .await?;
        let incoming: Vec<Value> = deserialize_take(&mut response, 0)?;
        for row in incoming {
            let from = id_text(row.get("in"));
            if from.is_empty() {
                continue;
            }
            part_links.push((from.clone(), id.clone()));
            if seen.contains(&from) || seen.len() >= NEIGHBOUR_CAP {
                continue;
            }
            seen.insert(from.clone());
            if let Some(entity) = load_entity(gm, &from).await? {
                entities.push(entity);
            }
        }
    }
    let mut edges = Vec::new();
    for entity in entities.iter().take(NEIGHBOUR_CAP) {
        if edges.len() >= ROW_CAP {
            break;
        }
        let mut response = gm
            .db()
            .query(
                "SELECT id, in, out, rel_type, status, valid_until, valid_from, evidence, pending FROM type::record($id)->(relates_to WHERE valid_until = NONE) ORDER BY id LIMIT 32",
            )
            .bind(("id", entity.id_string()))
            .await?;
        let batch: Vec<Value> = deserialize_take(&mut response, 0)?;
        edges.extend(batch);
    }
    let mut part_of = part_links;
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
            if rows.len() >= ROW_CAP {
                break;
            }
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
                pending: super::write::is_pending(slot),
            });
        }
    }
    for edge in edges.iter().take(ROW_CAP) {
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
            pending: edge.get("pending").is_some_and(Value::is_object),
            evidence,
        });
    }
    for hit in extra {
        let already = seen.contains(&hit.entity_id);
        append_extra_entity(
            gm,
            session,
            hit,
            already,
            &mut ExtraRows {
                names: &mut names,
                owners: &mut owners,
                rows: &mut rows,
                part_of: &mut part_of,
            },
        )
        .await?;
        seen.insert(hit.entity_id.clone());
    }
    Ok(Loaded {
        rows,
        owners,
        part_of,
    })
}

/// At most this many dependents become extra anchors in one session.
const D_ANCHOR_CAP: usize = 4;

struct DependentHit {
    entity_id: String,
    canonical: String,
    derived_from: Vec<String>,
}

async fn derived_dependents(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    stats: &WriteStats,
    report: &mut IngestionReport,
) -> Vec<DependentHit> {
    if !ctx.links_on
        || !stats
            .close_events
            .iter()
            .any(|event| writer_a1(&event.reason))
    {
        return Vec::new();
    }
    let closed: BTreeSet<String> = stats
        .close_events
        .iter()
        .filter(|event| writer_a1(&event.reason))
        .map(|event| event.row_id.clone())
        .collect();
    match linked_hits(gm, &closed).await {
        Ok(hits) => hits,
        Err(_) => {
            report.consolidate.errors += 1;
            Vec::new()
        }
    }
}

fn writer_a1(reason: &str) -> bool {
    matches!(reason, REJECTED | SUPERSEDED | REPLACED)
}

async fn linked_hits(
    gm: &GraphMemory,
    closed: &BTreeSet<String>,
) -> Result<Vec<DependentHit>, GraphError> {
    let mut response = gm.db().query("SELECT id, attributes FROM entity").await?;
    let entities: Vec<Value> = deserialize_take(&mut response, 0)?;
    let mut hits = collect_slot_hits(&entities, closed);
    hits.extend(collect_edge_hits(&linked_edges(gm).await?, closed));
    sort_hits(&mut hits);
    Ok(hits)
}

fn sort_hits(hits: &mut Vec<DependentHit>) {
    hits.sort_by(|left, right| left.canonical.cmp(&right.canonical));
    hits.dedup_by(|left, right| left.canonical == right.canonical);
}

fn collect_slot_hits(entities: &[Value], closed: &BTreeSet<String>) -> Vec<DependentHit> {
    let mut hits = Vec::new();
    for entity in entities {
        push_entity_hits(&mut hits, entity, closed);
    }
    hits
}

fn push_entity_hits(hits: &mut Vec<DependentHit>, entity: &Value, closed: &BTreeSet<String>) {
    let entity_id = id_text(entity.get("id"));
    if entity_id.is_empty() {
        return;
    }
    let Some(slots) = entity
        .pointer("/attributes/v2/slots")
        .and_then(Value::as_array)
    else {
        return;
    };
    for (index, slot) in slots.iter().enumerate() {
        if let Some(hit) = slot_dependent(&entity_id, index, slot, closed) {
            hits.push(hit);
        }
    }
}

fn slot_dependent(
    entity_id: &str,
    index: usize,
    slot: &Value,
    closed: &BTreeSet<String>,
) -> Option<DependentHit> {
    if !eligible_dependent(slot) {
        return None;
    }
    let derived = string_list(slot.get("derived_from"));
    if !mentions_closed(&derived, closed) {
        return None;
    }
    Some(DependentHit {
        entity_id: entity_id.to_string(),
        canonical: format!("{entity_id}#{index}"),
        derived_from: derived,
    })
}

fn collect_edge_hits(edges: &[Value], closed: &BTreeSet<String>) -> Vec<DependentHit> {
    let mut hits = Vec::new();
    for edge in edges {
        if let Some(hit) = edge_dependent(edge, closed) {
            hits.push(hit);
        }
    }
    hits
}

fn edge_dependent(edge: &Value, closed: &BTreeSet<String>) -> Option<DependentHit> {
    if !setting_or_rule(edge.get("rel_type").and_then(Value::as_str).unwrap_or("")) {
        return None;
    }
    if !eligible_edge(edge) {
        return None;
    }
    let derived = string_list(edge.get("derived_from"));
    if !mentions_closed(&derived, closed) {
        return None;
    }
    let canonical = id_text(edge.get("id"));
    if canonical.is_empty() {
        return None;
    }
    Some(DependentHit {
        entity_id: id_text(edge.get("in")),
        canonical,
        derived_from: derived,
    })
}

fn setting_or_rule(rel: &str) -> bool {
    rel == "setting" || rel == "rule"
}

fn mentions_closed(derived: &[String], closed: &BTreeSet<String>) -> bool {
    derived.iter().any(|id| closed.contains(id))
}

async fn linked_edges(gm: &GraphMemory) -> Result<Vec<Value>, GraphError> {
    let mut response = gm
        .db()
        .query(
            "SELECT id, in, rel_type, status, valid_until, pending, derived_from FROM relates_to WHERE derived_from != NONE AND valid_until = NONE",
        )
        .await?;
    deserialize_take(&mut response, 0)
}

fn eligible_dependent(slot: &Value) -> bool {
    if crud::edge_already_closed(slot.get("valid_until")) || super::write::is_pending(slot) {
        return false;
    }
    if slot.get("status").and_then(Value::as_str) != Some("decided") {
        return false;
    }
    matches!(
        slot.get("predicate").and_then(Value::as_str),
        Some("setting" | "rule")
    )
}

fn eligible_edge(edge: &Value) -> bool {
    !crud::edge_already_closed(edge.get("valid_until"))
        && !edge.get("pending").is_some_and(Value::is_object)
        && edge.get("status").and_then(Value::as_str) == Some("decided")
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn push_derived_anchors(
    anchors: &mut Vec<Anchor>,
    rows: &[Row],
    dependents: &[DependentHit],
    facts: &[super::schema::Fact],
    stats: &WriteStats,
) -> (u32, u32) {
    let (kept, overflow) = capped_usable(dependents, rows, anchors, stats);
    let mut added = 0u32;
    for hit in kept {
        if let Some(anchor) = derived_anchor(hit, rows, facts, stats) {
            anchors.push(anchor);
            added += 1;
        }
    }
    (added, overflow)
}

fn capped_usable<'a>(
    dependents: &'a [DependentHit],
    rows: &[Row],
    anchors: &[Anchor],
    stats: &WriteStats,
) -> (Vec<&'a DependentHit>, u32) {
    let usable: Vec<&DependentHit> = dependents
        .iter()
        .filter(|hit| hit_usable(hit, rows, anchors, stats))
        .collect();
    let overflow = u32::try_from(usable.len().saturating_sub(D_ANCHOR_CAP)).unwrap_or(u32::MAX);
    let kept = usable.into_iter().take(D_ANCHOR_CAP).collect();
    (kept, overflow)
}

fn hit_usable(hit: &DependentHit, rows: &[Row], anchors: &[Anchor], stats: &WriteStats) -> bool {
    let Some(index) = rows.iter().position(|row| ref_of(row) == hit.canonical) else {
        return false;
    };
    if anchors.iter().any(|anchor| anchor.live == Some(index)) {
        return false;
    }
    let row = &rows[index];
    if row.pending || !setting_or_rule(&row.pred) {
        return false;
    }
    close_for(hit, stats).is_some()
}

fn close_for<'a>(
    hit: &DependentHit,
    stats: &'a WriteStats,
) -> Option<&'a super::write::CloseEvent> {
    stats.close_events.iter().find(|event| {
        writer_a1(&event.reason) && hit.derived_from.iter().any(|id| id == &event.row_id)
    })
}

fn derived_anchor(
    hit: &DependentHit,
    rows: &[Row],
    facts: &[super::schema::Fact],
    stats: &WriteStats,
) -> Option<Anchor> {
    let index = rows.iter().position(|row| ref_of(row) == hit.canonical)?;
    let row = &rows[index];
    let event = close_for(hit, stats)?;
    let (turn, quote) = facts
        .get(event.fact_index)
        .map(|fact| (fact.turn, fact.quote.clone()))
        .unwrap_or((0, String::new()));
    Some(Anchor {
        id: format!("d:{}", ref_of(row)),
        subject: row.subject.clone(),
        pred: row.pred.clone(),
        key: row.key.clone(),
        value: row.value.clone(),
        text: row.value.clone(),
        new_value: row.value.clone(),
        replaces: String::new(),
        entity: String::new(),
        reason: "derived",
        turn,
        quote,
        live: Some(index),
        a2: false,
        closed_row: None,
    })
}

struct ExtraRows<'a> {
    names: &'a mut BTreeMap<String, String>,
    owners: &'a mut Vec<(String, String)>,
    rows: &'a mut Vec<Row>,
    part_of: &'a mut Vec<(String, String)>,
}

async fn append_extra_entity(
    gm: &GraphMemory,
    session: &str,
    hit: &DependentHit,
    already: bool,
    extra: &mut ExtraRows<'_>,
) -> Result<(), GraphError> {
    let Some(entity) = load_entity(gm, &hit.entity_id).await? else {
        return Ok(());
    };
    let entity_id = entity.id_string();
    extra.names.insert(entity_id.clone(), entity.name.clone());
    push_mandatory_slot(&entity, session, hit, extra.rows);
    if already {
        return Ok(());
    }
    let Some(v2) = entity
        .attributes
        .as_ref()
        .and_then(|attributes| attributes.get("v2"))
    else {
        return Ok(());
    };
    if !parsed_kind(v2) {
        return Ok(());
    }
    extra.owners.push((entity_id.clone(), entity.name.clone()));
    admit_sibling_slots(session, &entity_id, &entity.name, v2, extra.rows);
    admit_sibling_edges(
        gm,
        session,
        &entity_id,
        extra.names,
        extra.rows,
        extra.part_of,
    )
    .await?;
    Ok(())
}

fn parsed_kind(v2: &Value) -> bool {
    v2.get("kind")
        .and_then(Value::as_str)
        .and_then(Kind::parse)
        .is_some()
}

fn push_mandatory_slot(entity: &Entity, session: &str, hit: &DependentHit, rows: &mut Vec<Row>) {
    if rows.iter().any(|row| ref_of(row) == hit.canonical) {
        return;
    }
    let Some(index) = slot_index(hit) else {
        return;
    };
    let Some(slot) = entity
        .attributes
        .as_ref()
        .and_then(|attributes| attributes.pointer("/v2/slots"))
        .and_then(Value::as_array)
        .and_then(|slots| slots.get(index))
    else {
        return;
    };
    let entity_id = entity.id_string();
    if let Some(row) = slot_row(&entity_id, &entity.name, index, slot, session) {
        rows.push(row);
    }
}

fn slot_index(hit: &DependentHit) -> Option<usize> {
    let (owner, index) = hit.canonical.rsplit_once('#')?;
    if owner != hit.entity_id {
        return None;
    }
    index.parse().ok()
}

fn slot_row(entity_id: &str, name: &str, index: usize, slot: &Value, session: &str) -> Option<Row> {
    if crud::edge_already_closed(slot.get("valid_until")) {
        return None;
    }
    if slot.get("status").and_then(Value::as_str) != Some("decided") {
        return None;
    }
    Some(Row {
        id: format!("s:{entity_id}:{index}"),
        entity: entity_id.to_string(),
        slot: Some(index),
        edge: None,
        subject: name.to_string(),
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
        pending: super::write::is_pending(slot),
    })
}

fn admit_sibling_slots(
    session: &str,
    entity_id: &str,
    name: &str,
    v2: &Value,
    rows: &mut Vec<Row>,
) {
    let Some(slots) = v2.get("slots").and_then(Value::as_array) else {
        return;
    };
    for (index, slot) in slots.iter().enumerate() {
        if rows.len() >= ROW_CAP {
            break;
        }
        if rows
            .iter()
            .any(|row| row.entity == entity_id && row.slot == Some(index))
        {
            continue;
        }
        if let Some(row) = slot_row(entity_id, name, index, slot, session) {
            rows.push(row);
        }
    }
}

async fn admit_sibling_edges(
    gm: &GraphMemory,
    session: &str,
    entity_id: &str,
    names: &BTreeMap<String, String>,
    rows: &mut Vec<Row>,
    part_of: &mut Vec<(String, String)>,
) -> Result<(), GraphError> {
    let mut response = gm
        .db()
        .query(
            "SELECT id, in, out, rel_type, status, valid_until, valid_from, evidence, pending FROM type::record($id)->(relates_to WHERE valid_until = NONE) ORDER BY id LIMIT 32",
        )
        .bind(("id", entity_id.to_string()))
        .await?;
    let edges: Vec<Value> = deserialize_take(&mut response, 0)?;
    for edge in &edges {
        if rows.len() >= ROW_CAP {
            break;
        }
        push_capped_edge(edge, session, names, rows, part_of);
    }
    Ok(())
}

fn push_capped_edge(
    edge: &Value,
    session: &str,
    names: &BTreeMap<String, String>,
    rows: &mut Vec<Row>,
    part_of: &mut Vec<(String, String)>,
) {
    if crud::edge_already_closed(edge.get("valid_until")) {
        return;
    }
    let rel = edge.get("rel_type").and_then(Value::as_str).unwrap_or("");
    let from = id_text(edge.get("in"));
    let to = id_text(edge.get("out"));
    if edge.get("status").and_then(Value::as_str) != Some("decided") {
        return;
    }
    if rel == "part_of" {
        part_of.push((from.clone(), to.clone()));
    }
    if !Predicate::parse(rel).is_some_and(Predicate::is_edge) {
        return;
    }
    let edge_id = id_text(edge.get("id"));
    if rows
        .iter()
        .any(|row| row.edge.as_deref() == Some(edge_id.as_str()))
    {
        return;
    }
    let evidence = edge.get("evidence").cloned().unwrap_or(Value::Null);
    rows.push(Row {
        id: format!("e:{edge_id}"),
        entity: from.clone(),
        slot: None,
        edge: Some(edge_id),
        subject: names.get(&from).cloned().unwrap_or_default(),
        pred: rel.to_string(),
        key: String::new(),
        value: names.get(&to).cloned().unwrap_or_default(),
        from: json_text(edge.get("valid_from")),
        here: opened_here(Some(&evidence), session),
        pending: edge.get("pending").is_some_and(Value::is_object),
        evidence,
    });
}

async fn load_named(gm: &GraphMemory, names: &[String]) -> Result<Vec<Entity>, GraphError> {
    let names: Vec<String> = names.iter().take(NEIGHBOUR_CAP).cloned().collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut response = gm
        .db()
        .query("SELECT * OMIT embedding FROM entity WHERE name INSIDE $names ORDER BY id LIMIT 48")
        .bind(("names", names))
        .await?;
    deserialize_take(&mut response, 0)
}

async fn load_entity(gm: &GraphMemory, id: &str) -> Result<Option<Entity>, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT * OMIT embedding FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?;
    let rows: Vec<Entity> = deserialize_take(&mut response, 0)?;
    Ok(rows.into_iter().next())
}

fn id_text(id: Option<&Value>) -> String {
    super::hold::id_text(id)
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
    digest_key(
        [&anchor.value, &anchor.new_value, &partner.value, evidence],
        PROMPT,
    )
}

fn digest_key(parts: [&str; 4], prompt: &str) -> String {
    let raw = format!(
        "{}\n{}\n{}\n{}\n{prompt}",
        normalise(parts[0]),
        normalise(parts[1]),
        normalise(parts[2]),
        normalise(parts[3]),
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
    // A later object is not a verdict when this one is not the schema.
    let slice = first_object(text).ok_or(())?;
    let value: Value = serde_json::from_str(slice).map_err(|_| ())?;
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

/// First balanced `{...}`. Braces inside a JSON string do not count.
fn first_object(text: &str) -> Option<&str> {
    let mut opened = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (index, ch) in text.char_indices() {
        let Some(from) = opened else {
            if ch == '{' {
                opened = Some(index);
                depth = 1;
            }
            continue;
        };
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = index + ch.len_utf8();
                    return Some(&text[from..end]);
                }
            }
            _ => {}
        }
    }
    None
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

struct ValueClasses {
    duration: Option<Regex>,
    version: Option<Regex>,
    number: Option<Regex>,
    path: Option<Regex>,
    command: Option<Regex>,
    ident: Option<Regex>,
}

fn value_classes() -> &'static ValueClasses {
    static CLASSES: OnceLock<ValueClasses> = OnceLock::new();
    CLASSES.get_or_init(|| ValueClasses {
        duration: Regex::new(r"(?i)^\d+(?:\.\d+)?\s*(?:ms|sec|secs|second|seconds|min|mins|minute|minutes|hr|hrs|hour|hours|day|days|[smhd])$").ok(),
        version: Regex::new(r"(?i)^v?\d+\.\d+(?:\.\d+)?(?:[-+][A-Za-z0-9.]+)?$").ok(),
        number: Regex::new(r"^[+-]?\d+(?:\.\d+)?$").ok(),
        path: Regex::new(r"(?i)(?:/|\\|\{\{|\{[A-Za-z0-9_]+\}|YYYY)").ok(),
        command: Regex::new(r"(?i)(?:^|\s)--?[A-Za-z]|&&|\|\||`[^`]+`").ok(),
        ident: Regex::new(r"^[A-Za-z_][A-Za-z0-9_-]*$").ok(),
    })
}

fn value_class(value: &str) -> &'static str {
    let value = value.trim();
    let classes = value_classes();
    if classes
        .duration
        .as_ref()
        .is_some_and(|re| re.is_match(value))
    {
        return "duration";
    }
    if classes
        .version
        .as_ref()
        .is_some_and(|re| re.is_match(value))
    {
        return "version";
    }
    if classes.number.as_ref().is_some_and(|re| re.is_match(value)) {
        return "number";
    }
    if classes.path.as_ref().is_some_and(|re| re.is_match(value)) {
        return "path";
    }
    if classes
        .command
        .as_ref()
        .is_some_and(|re| re.is_match(value))
    {
        return "command";
    }
    if classes.ident.as_ref().is_some_and(|re| re.is_match(value)) {
        return "identifier";
    }
    "prose"
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

/// The turn the cue check should read. Prefer one that names both values,
/// then the old value, then the new value. Turn 0 is only the fallback when
/// none of the session's turns name either value.
fn hold_turn(ctx: &WriteCtx<'_>, old: &str, new_value: &str) -> usize {
    let hit = |text: &str, value: &str| !value.is_empty() && mention(value, text);
    ctx.turns
        .iter()
        .find(|turn| hit(&turn.text, old) && hit(&turn.text, new_value))
        .or_else(|| ctx.turns.iter().find(|turn| hit(&turn.text, old)))
        .or_else(|| ctx.turns.iter().find(|turn| hit(&turn.text, new_value)))
        .map(|turn| turn.index)
        .unwrap_or(0)
}

fn attach_holds(
    ctx: &WriteCtx<'_>,
    open: &[super::hold::OpenHold],
    anchors: &mut Vec<Anchor>,
    loaded: &Loaded,
    waiting: &mut Vec<Pair>,
    report: &mut IngestionReport,
) -> u32 {
    let text = ctx
        .turns
        .iter()
        .map(|turn| turn.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut added = 0u32;
    for hold in open {
        let Some(new_index) = find_ref(&loaded.rows, &hold.new_ref, hold, &hold.new_value) else {
            report.hold.errors += 1;
            continue;
        };
        let Some(old_index) = find_ref(&loaded.rows, &hold.old_ref, hold, &hold.old_value) else {
            report.hold.errors += 1;
            continue;
        };
        let anchor = anchors.len();
        anchors.push(Anchor {
            id: format!("h:{anchor}"),
            subject: hold.subject.clone(),
            pred: hold.pred.clone(),
            key: hold.key.clone(),
            value: hold.old_value.clone(),
            text: format!("{} {}", hold.old_value, hold.new_value),
            new_value: hold.new_value.clone(),
            replaces: String::new(),
            entity: hold.subject.clone(),
            reason: "hold",
            turn: hold_turn(ctx, &hold.old_value, &hold.new_value),
            quote: String::new(),
            live: Some(new_index),
            a2: false,
            closed_row: None,
        });
        waiting.push(Pair {
            anchor,
            partner: old_index,
            score: 0,
            evidence: cut_evidence(&text, &[&hold.old_value, &hold.new_value]),
            stored: stored_session(&loaded.rows[old_index].evidence),
            hold: true,
        });
        report.hold.holds_ambiguous += 1;
        added += 1;
    }
    added
}

fn find_ref(
    rows: &[Row],
    reference: &str,
    hold: &super::hold::OpenHold,
    value: &str,
) -> Option<usize> {
    rows.iter()
        .position(|row| ref_of(row) == reference)
        .or_else(|| {
            rows.iter().position(|row| {
                values_equal(&row.value, value)
                    && normalise(&row.subject) == normalise(&hold.subject)
                    && row.pred == hold.pred
                    && (row.edge.is_some()
                        || keys_match(&row.key, &hold.key)
                        || row.key == hold.key)
            })
        })
}

fn ref_of(row: &Row) -> String {
    if let Some(edge) = &row.edge {
        return edge.clone();
    }
    format!("{}#{}", row.entity, row.slot.unwrap_or(0))
}

async fn confirm_held(
    gm: &GraphMemory,
    anchors: &[Anchor],
    loaded: &Loaded,
    pair: &Pair,
    report: &mut IngestionReport,
) -> Option<Value> {
    let Some(index) = anchors[pair.anchor].live else {
        report.hold.errors += 1;
        return None;
    };
    let reference = ref_of(&loaded.rows[index]);
    match super::hold::take_pending(gm, &reference).await {
        Ok(Some(pending)) => Some(pending),
        Ok(None) => {
            report.hold.errors += 1;
            None
        }
        Err(err) => {
            eprintln!("hold error: {err}");
            report.hold.errors += 1;
            None
        }
    }
}

fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(all(test, feature = "embedded"))]
mod tests {
    use std::collections::BTreeSet;
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

        fn message(&self) -> String {
            self.message.lock().expect("message").clone()
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
                r#"{"facts":[{"subject":"harbor","subject_type":"project","predicate":"setting","role":"none","key":"cover","object":"woven wrap","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Pack the harbor with a woven wrap.","confidence":0.9}],"bugs":[],"releases":[]}"#,
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

    async fn rewrite_slot(gm: &GraphMemory, name: &str, key: &str, from: &str, to: &str) {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities.iter().find(|entity| entity.name == name).unwrap();
        let mut attributes = entity.attributes.clone().unwrap();
        let slots = attributes["v2"]["slots"].as_array_mut().unwrap();
        let slot = slots
            .iter_mut()
            .find(|slot| slot["key"] == key && slot["value"] == from)
            .unwrap();
        slot["value"] = json!(to);
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", entity.id_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
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
        judge_turn(turn_text, &judged(&[(1, "supersedes", quote)])).await
    }

    async fn judge_turn(turn_text: &str, body: &str) -> (IngestionReport, Value) {
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
        let stub = Stub::new(Answer::Reply(body.to_string()));
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
                "---\nlog: 2\ndate: \"2026-04-02T00:00:00Z\"\nsession_id: \"s2\"\n---\n\n### User\n\nPack the harbor with a woven wrap.\n",
            )
            .unwrap();
            let gm = GraphMemory::open_embedded(&memory.join("graph"))
                .await
                .unwrap();
            ensure_v2_store(&gm).await.unwrap();
            let old = setting(
                "harbor",
                Kind::Project,
                "cloth",
                "woven cloth",
                "cloth is woven cloth",
            );
            let harbor = ProjectIdentity {
                name: "harbor".into(),
                aliases: Vec::new(),
            };
            let spoken = [turn(&old.quote)];
            let ctx = WriteCtx::new(&Quiet, &harbor, "s1", "2026-04-01", &spoken);
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
            assert!(ended(live_any(
                &slots(&gm, "harbor").await,
                "cloth",
                "woven cloth"
            ))
            .is_none());
        }
    }

    #[tokio::test]
    async fn hold_on_without_consolidate_opens_no_pending_row() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(
            memory.join(".recall-echo.toml"),
            "[graph]\nknown_match = true\nconsolidate = false\nconsolidate_hold = true\n",
        )
        .unwrap();
        std::fs::create_dir_all(memory.join("conversations")).unwrap();
        std::fs::write(
            memory.join("conversations/conversation-002.md"),
            "---\nlog: 2\ndate: \"2026-04-02T00:00:00Z\"\nsession_id: \"s2\"\n---\n\n### User\n\nStore the pages in linen.\n",
        )
        .unwrap();
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        ensure_v2_store(&gm).await.unwrap();
        let project = ProjectIdentity {
            name: "acme".into(),
            aliases: Vec::new(),
        };
        let spoken = [turn("The binder is canvas.")];
        let ctx = WriteCtx::new(&Quiet, &project, "s1", "2026-04-01", &spoken);
        write_facts(
            &gm,
            &ctx,
            &[setting(
                "acme",
                Kind::Project,
                "binder",
                "canvas",
                "The binder is canvas.",
            )],
        )
        .await
        .unwrap();
        struct Linen;
        #[async_trait]
        impl LlmProvider for Linen {
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
                    r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"binder","object":"linen","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Store the pages in linen.","confidence":0.9}],"bugs":[],"releases":[]}"#,
                ))
            }
        }
        let report = super::super::pass1::extract_session(
            &gm,
            memory,
            2,
            &IngestContext::new("s2", Some(2)),
            &Linen,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.hold.holds_opened, 0);
        assert_eq!(
            report.known_match.skipped.get("no_mention").copied(),
            Some(1)
        );
        let rows = slots(&gm, "acme").await;
        let canvas = slot_named(&rows, "binder", "canvas");
        let linen = slot_named(&rows, "binder", "linen");
        assert!(ended(canvas).is_none());
        assert!(canvas.get("pending").is_none());
        assert!(linen.get("pending").is_none());
        assert!(ended(linen).is_none());
    }

    /// The probe: a one-letter duplicate quote, and a longer one, against the
    /// only live copy. Neither closes it.
    #[tokio::test]
    async fn a_duplicate_quote_does_not_close_the_only_live_copy() {
        for quote in ["p", "plain grid layout"] {
            let (_tmp, gm) = open().await;
            let grid = setting(
                "acme",
                Kind::Project,
                "layout",
                "plain grid",
                "layout is plain grid",
            );
            let text = setting(
                "acme",
                Kind::Project,
                "format",
                "plain text",
                "format is plain text",
            );
            commit(
                &gm,
                "s1",
                "2026-03-01",
                &[turn("layout is plain grid. format is plain text")],
                &[grid, text],
            )
            .await;
            let spoken = "Leave the plain grid layout in place.";
            let mut rejected = setting("acme", Kind::Project, "format", "plain text", spoken);
            rejected.status = "rejected".into();
            let turns = [turn(spoken)];
            let stats = commit(
                &gm,
                "s2",
                "2026-03-02",
                &turns,
                std::slice::from_ref(&rejected),
            )
            .await;
            let stub = Stub::new(Answer::Reply(judged(&[(1, "duplicate", quote)])));
            let report = pass(&gm, "s2", "2026-03-02", &turns, &[rejected], &stats, &stub).await;
            assert_eq!(stub.calls(), 1, "{quote}");
            assert_eq!(report.consolidate.closes_dup, 0, "{quote}");
            assert_eq!(report.consolidate.closes_sup, 0, "{quote}");
            assert!(
                ended(live_any(&slots(&gm, "acme").await, "layout", "plain grid")).is_none(),
                "{quote}"
            );
        }
    }

    #[tokio::test]
    async fn a_one_character_quote_closes_nothing() {
        let (report, slot) = judge_layout("t", "Switch to rich text instead of plain text.").await;
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    fn dropped(report: &IngestionReport, reason: &str) -> u32 {
        report.consolidate.dropped.get(reason).copied().unwrap_or(0)
    }

    #[tokio::test]
    async fn quote_too_short_is_counted() {
        let (report, slot) =
            judge_layout("plain text", "Switch to rich text instead of plain text.").await;
        assert_eq!(dropped(&report, "quote too short"), 1);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    #[tokio::test]
    async fn quote_not_verbatim_is_counted() {
        let (report, slot) = judge_layout(
            "instead of plain text is not what was said",
            "Switch to rich text instead of plain text.",
        )
        .await;
        assert_eq!(dropped(&report, "quote not verbatim in the evidence"), 1);
        assert_eq!(dropped(&report, "quote too short"), 0);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    #[tokio::test]
    async fn missing_departure_cue_is_counted() {
        let (report, slot) = judge_layout(
            "plain text is still stored",
            "The layout is rich text while plain text is still stored.",
        )
        .await;
        assert_eq!(dropped(&report, "no departure cue"), 1);
        assert_eq!(dropped(&report, "quote too short"), 0);
        assert_eq!(dropped(&report, "quote not verbatim in the evidence"), 0);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    #[test]
    fn replacement_phrases_are_whole_phrases_only() {
        for quote in [
            "The layout is now sqlite for the index rows.",
            "The layout IS NOW sqlite for the index rows.",
            "The layout pools are now sqlite for the index rows.",
            "The layout store now uses sqlite for the index rows.",
            "New layout: sqlite for the index rows.",
            "Agreed today. New layout: sqlite for the index rows.",
        ] {
            assert!(quote_replaces(quote), "{quote}");
        }
        for quote in [
            "also now supports sqlite in production",
            "now available in addition to sqlite",
            "the layout now sqlite for the index rows",
            "We picked a new layout: sqlite for the index rows.",
            "Use sqlite instead of walnut today.",
            "dry run by default with --apply",
        ] {
            assert!(!quote_replaces(quote), "{quote}");
        }
    }

    #[tokio::test]
    async fn replacement_phrase_closes_when_the_quote_names_the_new_value() {
        for quote in [
            "The layout is now sqlite for the index rows.",
            "The layout IS NOW sqlite for the index rows.",
            "The layout pools are now sqlite for the index rows.",
            "The layout store now uses sqlite for the index rows.",
            "New layout: sqlite for the index rows.",
            "Agreed today. New layout: sqlite for the index rows.",
        ] {
            let (report, slot) =
                judge_keyed(quote, quote, "layout", "sqlite", "layout_name", "walnut").await;
            assert_eq!(report.consolidate.closes_sup, 1, "{quote}");
            assert_eq!(dropped(&report, "no departure cue"), 0, "{quote}");
            assert_eq!(ended(&slot), Some("s2:consolidated"), "{quote}");
        }
    }

    #[tokio::test]
    async fn replacement_phrase_without_the_new_value_stays_dropped() {
        for quote in [
            "The layout is now ready for the index rows.",
            "The pools are now ready for the index rows.",
            "The store now uses rocks for the index rows.",
            "New layout: rocks for the index rows.",
            "Agreed today. New default: rocks for the index rows.",
        ] {
            let turn_text = format!("{quote} sqlite stays noted here.");
            let (report, slot) = judge_keyed(
                quote,
                &turn_text,
                "layout",
                "sqlite",
                "layout_name",
                "walnut",
            )
            .await;
            assert_eq!(report.consolidate.calls, 1, "{quote}");
            assert_eq!(dropped(&report, "no departure cue"), 1, "{quote}");
            assert_eq!(report.consolidate.closes_sup, 0, "{quote}");
            assert!(ended(&slot).is_none(), "{quote}");
        }
    }

    #[tokio::test]
    async fn dry_run_wording_stays_dropped() {
        let quote = "dry run by default with --apply";
        let (report, slot) =
            judge_keyed(quote, quote, "layout", "dry run", "layout_name", "live run").await;
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(dropped(&report, "no departure cue"), 1);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    #[tokio::test]
    async fn additive_wording_stays_dropped() {
        for quote in [
            "also now supports sqlite in production",
            "now available in addition to sqlite today",
        ] {
            let (report, slot) =
                judge_keyed(quote, quote, "layout", "sqlite", "layout_name", "walnut").await;
            assert_eq!(report.consolidate.calls, 1, "{quote}");
            assert_eq!(dropped(&report, "no departure cue"), 1, "{quote}");
            assert_eq!(report.consolidate.closes_sup, 0, "{quote}");
            assert!(ended(&slot).is_none(), "{quote}");
        }
    }

    #[tokio::test]
    async fn a_list_item_or_a_second_setting_stays_dropped() {
        let listed = "New layout: cedar, oak and pine for the rows.";
        let (report, slot) =
            judge_keyed(listed, listed, "layout", "cedar", "layout_name", "walnut").await;
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(dropped(&report, "no departure cue"), 1);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());

        let second = "The layout is now rich grid for the index rows.";
        let (report, slot) = judge_keyed(
            second,
            second,
            "layout",
            "rich grid",
            "format",
            "plain grid",
        )
        .await;
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(dropped(&report, "no departure cue"), 1);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    #[tokio::test]
    async fn a_phrase_outside_the_quote_does_not_count() {
        let quote = "Keep sqlite for the index rows.";
        let turn_text = "The layout is now ready today. Keep sqlite for the index rows.";
        let (report, slot) = judge_keyed(
            quote,
            turn_text,
            "layout",
            "sqlite",
            "layout_name",
            "walnut",
        )
        .await;
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(dropped(&report, "no departure cue"), 1);
        assert_eq!(dropped(&report, "quote not verbatim in the evidence"), 0);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert!(ended(&slot).is_none());
    }

    #[tokio::test]
    async fn two_replacement_supersedes_on_one_anchor_close_none() {
        let (_tmp, gm) = open().await;
        let walnut = setting(
            "acme",
            Kind::Project,
            "layout_name",
            "walnut",
            "layout_name is walnut",
        );
        let cherry = setting(
            "acme",
            Kind::Project,
            "layout_mode",
            "cherry",
            "layout_mode is cherry",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("layout_name is walnut. layout_mode is cherry")],
            &[walnut, cherry],
        )
        .await;
        // Each quote would close on its own. Two supersedes on this anchor close none.
        let turn_text =
            "The layout is now sqlite for the index rows. New layout: sqlite for the index rows.";
        let first = "The layout is now sqlite for the index rows.";
        let second = "New layout: sqlite for the index rows.";
        let next = setting("acme", Kind::Project, "layout", "sqlite", turn_text);
        let turns = [turn(turn_text)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[
            (1, "supersedes", first),
            (2, "supersedes", second),
        ])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 1, "{}", stub.message.lock().unwrap());
        assert_eq!(report.consolidate.closes_sup, 0);
        assert_eq!(dropped(&report, "second supersede on the same anchor"), 2);
        assert!(
            report
                .consolidate
                .skips
                .get("consolidate_ambiguous")
                .copied()
                .unwrap_or(0)
                >= 2
        );
        let rows = slots(&gm, "acme").await;
        assert!(ended(live_any(&rows, "layout_name", "walnut")).is_none());
        assert!(ended(live_any(&rows, "layout_mode", "cherry")).is_none());
    }

    #[tokio::test]
    async fn a_component_with_the_same_key_accepts_the_phrase() {
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
            "layout_name",
            "walnut",
            "layout_name is walnut",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("picker is part of acme. layout_name is walnut")],
            &[link, old],
        )
        .await;
        let quote = "The layout is now sqlite for the index rows.";
        let next = setting("acme", Kind::Project, "layout", "sqlite", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 1, "{}", stub.message.lock().unwrap());
        assert_eq!(report.consolidate.closes_sup, 1);
        assert_eq!(
            ended(live_any(
                &slots(&gm, "picker").await,
                "layout_name",
                "walnut"
            )),
            Some("s2:consolidated")
        );
    }

    /// Negated, additive, questioned, hedged, listed or off-key phrasing
    /// never closes, even when the judge says supersedes.
    #[tokio::test]
    async fn hedged_or_off_key_phrasing_stays_dropped() {
        for quote in [
            "The layout is now not sqlite for the rows.",
            "No, the layout is now never sqlite here.",
            "The layout isn't now sqlite for the rows.",
            "The layout is now also sqlite for the rows.",
            "The layout now uses sqlite as well for rows.",
            "The layout sqlite is now available for rows.",
            "So the layout is now sqlite for the rows?",
            "If the layout is now sqlite, tests would break.",
            "Maybe the layout is now sqlite for the rows.",
            "I could suggest the layout is now sqlite here.",
            "New idea: sqlite for the index rows maybe.",
            "New file: sqlite.rs holds the index rows.",
            "The cache is now sqlite for the index rows.",
            "The layout is now sqlite plus redis for rows.",
            "They said the layout is now sqlite for rows.",
            "The layout is now sqlite for now.",
            "The layout is now sqlite for testing only.",
            "Until Friday the layout is now sqlite.",
            "The assistant proposes the layout is now sqlite.",
            "We tried it: the layout is now sqlite but reverted.",
            "The picker layout is now sqlite for the rows.",
            "The layout is now sqlite temporarily for rows.",
            "The layout is now sqlite, redis for the rows.",
            "The layout is sqlite now.",
            "Maybe the layout is sqlite now.",
            "The layout is sqlite now?",
            "The layout is sqlite for now.",
            "They said the layout is sqlite now.",
            "The picker layout is sqlite now.",
            "The layout is sqlite now, really.",
            "The layout is also sqlite now.",
        ] {
            let (report, slot) =
                judge_keyed(quote, quote, "layout", "sqlite", "layout_name", "walnut").await;
            assert_eq!(report.consolidate.closes_sup, 0, "{quote}");
            assert!(ended(&slot).is_none(), "{quote}");
        }
    }

    #[tokio::test]
    async fn plain_replacements_still_close() {
        for (quote, new_key, new_value, old_key, old_value) in [
            (
                "Storage is now SQLite at ~/.local/share/app.db.",
                "storage",
                "sqlite",
                "storage_name",
                "walnut",
            ),
            (
                "New layout: per-project folders under data.",
                "layout",
                "per-project folders",
                "layout_name",
                "flat folders",
            ),
            (
                "Default --jobs is now 4 instead of the CPU count.",
                "jobs",
                "4",
                "jobs_count",
                "8",
            ),
        ] {
            let (report, slot) =
                judge_keyed(quote, quote, new_key, new_value, old_key, old_value).await;
            assert_eq!(
                report.consolidate.closes_sup, 1,
                "{quote} {:?}",
                report.consolidate
            );
            assert_eq!(ended(&slot), Some("s2:consolidated"), "{quote}");
        }
    }

    #[tokio::test]
    async fn no_longer_before_the_new_value_still_closes() {
        for quote in [
            "The layout is no longer walnut; it is now sqlite.",
            "No longer walnut, the layout is now sqlite.",
        ] {
            let (report, slot) =
                judge_keyed(quote, quote, "layout", "sqlite", "layout_name", "walnut").await;
            assert_eq!(report.consolidate.closes_sup, 1, "{quote}");
            assert_eq!(ended(&slot), Some("s2:consolidated"), "{quote}");
        }
    }

    async fn judge_keyed(
        quote: &str,
        turn_text: &str,
        new_key: &str,
        new_value: &str,
        old_key: &str,
        old_value: &str,
    ) -> (IngestionReport, Value) {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            old_key,
            old_value,
            &format!("{old_key} is {old_value}"),
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let next = setting("acme", Kind::Project, new_key, new_value, turn_text);
        let turns = [turn(turn_text)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        let slot = live_any(&slots(&gm, "acme").await, old_key, old_value).clone();
        (report, slot)
    }

    #[tokio::test]
    async fn second_supersede_on_one_anchor_is_counted() {
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
            (1, "supersedes", "instead of cedar plank"),
            (2, "supersedes", "instead of cedar board"),
        ])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(dropped(&report, "second supersede on the same anchor"), 2);
        assert_eq!(report.consolidate.closes_sup, 0);
    }

    #[tokio::test]
    async fn duplicate_without_a_kept_copy_is_counted() {
        let (_tmp, gm) = open().await;
        let grid = setting(
            "acme",
            Kind::Project,
            "layout",
            "plain grid",
            "layout is plain grid",
        );
        let text = setting(
            "acme",
            Kind::Project,
            "format",
            "plain text",
            "format is plain text",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("layout is plain grid. format is plain text")],
            &[grid, text],
        )
        .await;
        let spoken = "Leave the plain grid layout in place.";
        let mut rejected = setting("acme", Kind::Project, "format", "plain text", spoken);
        rejected.status = "rejected".into();
        let turns = [turn(spoken)];
        let stats = commit(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            std::slice::from_ref(&rejected),
        )
        .await;
        let stub = Stub::new(Answer::Reply(judged(&[(
            1,
            "duplicate",
            "plain grid layout",
        )])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[rejected], &stats, &stub).await;
        assert_eq!(dropped(&report, "no kept live copy (duplicate)"), 1);
        assert_eq!(report.consolidate.closes_dup, 0);
        assert_eq!(report.consolidate.closes_sup, 0);
    }

    #[tokio::test]
    async fn a_row_opened_this_session_is_counted() {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            "layout",
            "plain grid",
            "layout is plain grid",
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let next = setting(
            "acme",
            Kind::Project,
            "grid",
            "plain grid",
            "grid is plain grid",
        );
        let turns = [turn(&next.quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Panic);
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[next], &stats, &stub).await;
        assert_eq!(stub.calls(), 0);
        assert_eq!(dropped(&report, "row opened this session"), 1);
        assert_eq!(report.consolidate.closes_dup, 0);
        let rows = slots(&gm, "acme").await;
        assert!(ended(live_any(&rows, "layout", "plain grid")).is_none());
        assert!(ended(live_any(&rows, "grid", "plain grid")).is_none());
    }

    #[tokio::test]
    async fn a_row_already_closed_is_counted() {
        let (_tmp, gm) = open().await;
        let old = setting(
            "acme",
            Kind::Project,
            "plank",
            "cedar plank",
            "plank is cedar plank",
        );
        commit(&gm, "s1", "2026-03-01", &[turn(&old.quote)], &[old]).await;
        let quote = "Use cedar beam instead of cedar plank. Use cedar post instead of cedar plank.";
        let beam = setting("acme", Kind::Project, "beam", "cedar beam", quote);
        let post = setting("acme", Kind::Project, "post", "cedar post", quote);
        let turns = [turn(quote)];
        let facts = [beam, post];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, &facts).await;
        let stub = Stub::new(Answer::Reply(judged(&[
            (1, "supersedes", "instead of cedar plank"),
            (2, "supersedes", "instead of cedar plank"),
        ])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &facts, &stats, &stub).await;
        assert_eq!(dropped(&report, "anchor or pair no longer present"), 1);
        assert_eq!(report.consolidate.closes_sup, 1);
        assert!(ended(live_any(&slots(&gm, "acme").await, "plank", "cedar plank")).is_some());
    }

    #[tokio::test]
    async fn two_supersedes_with_different_quotes_close_none() {
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
            (1, "supersedes", "instead of cedar plank"),
            (2, "supersedes", "instead of cedar board"),
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
    async fn a_changed_partner_value_is_judged_again() {
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
        pass(&gm, "s2", "2026-03-02", &turns, &[first], &stats, &stub).await;
        assert_eq!(stub.calls(), 1);
        rewrite_slot(&gm, "acme", "old", "quartz marker", "quartz signal").await;
        let again = setting("acme", Kind::Project, "newer", "quartz beacon", quote);
        let stats = commit(
            &gm,
            "s3",
            "2026-03-04",
            &turns,
            std::slice::from_ref(&again),
        )
        .await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "unrelated", "quartz beacon")])));
        let report = pass(&gm, "s3", "2026-03-04", &turns, &[again], &stats, &stub).await;
        assert_eq!(stub.calls(), 1, "{}", stub.message.lock().unwrap());
        assert_eq!(report.consolidate.closes_sup, 0);
    }

    /// Two live copies, each judged a duplicate in one pass: one stays live.
    #[tokio::test]
    async fn two_duplicate_copies_keep_one_live() {
        let (_tmp, gm) = open().await;
        let layout = setting(
            "acme",
            Kind::Project,
            "layout",
            "plain grid",
            "layout is plain grid",
        );
        let grid = setting(
            "acme",
            Kind::Project,
            "grid",
            "plain grid",
            "grid is plain grid",
        );
        let text = setting(
            "acme",
            Kind::Project,
            "format",
            "plain text",
            "format is plain text",
        );
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn(
                "layout is plain grid. grid is plain grid. format is plain text",
            )],
            &[layout, grid, text],
        )
        .await;
        let spoken = "Leave the plain grid layout in place.";
        let mut rejected = setting("acme", Kind::Project, "format", "plain text", spoken);
        rejected.status = "rejected".into();
        let turns = [turn(spoken)];
        let stats = commit(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            std::slice::from_ref(&rejected),
        )
        .await;
        let stub = Stub::new(Answer::Reply(judged(&[
            (1, "duplicate", "plain grid layout"),
            (2, "duplicate", "plain grid layout"),
        ])));
        let report = pass(&gm, "s2", "2026-03-02", &turns, &[rejected], &stats, &stub).await;
        assert_eq!(report.consolidate.pairs, 2);
        assert!(report.consolidate.closes_dup <= 1);
        let live = slots(&gm, "acme")
            .await
            .iter()
            .filter(|slot| slot["value"] == "plain grid" && ended(slot).is_none())
            .count();
        assert_eq!(live, 1);
    }

    /// A dependent past the row cap is still loaded when its owner was already seen.
    #[tokio::test]
    async fn a_selected_dependent_is_loaded_when_its_owner_is_already_seen() {
        let (_tmp, gm) = open().await;
        let quote = "delay is north pier marker";
        let mut facts = Vec::new();
        for index in 0..super::ROW_CAP {
            facts.push(setting(
                "birch",
                Kind::Project,
                &format!("k{index:04}"),
                &format!("v{index:04}"),
                quote,
            ));
        }
        facts.push(setting(
            "birch",
            Kind::Project,
            "delay",
            "north pier marker",
            quote,
        ));
        commit(&gm, "s1", "2026-04-03", &[turn(quote)], &facts).await;
        let canonical = slot_canonical(&gm, "birch", "north pier marker").await;
        let entity_id = canonical
            .rsplit_once('#')
            .expect("slot canonical")
            .0
            .to_string();
        let without = super::load_rows(&gm, "s9", &["birch".into()], &[])
            .await
            .unwrap();
        assert!(
            without
                .rows
                .iter()
                .all(|row| super::ref_of(row) != canonical),
            "the row cap kept the selected slot"
        );
        let hit = super::DependentHit {
            entity_id,
            canonical: canonical.clone(),
            derived_from: vec!["entity:harbor#0".into()],
        };
        let with = super::load_rows(&gm, "s9", &["birch".into()], &[hit])
            .await
            .unwrap();
        assert!(
            with.rows.iter().any(|row| super::ref_of(row) == canonical),
            "selected dependent missing from {:?}",
            with.rows.len()
        );
    }

    /// The part_of cap keeps the lowest edge ids, not an arbitrary 16.
    #[tokio::test]
    async fn neighbourhood_cap_keeps_the_lowest_edge_ids() {
        let (_tmp, gm) = open().await;
        let mut facts = vec![setting("acme", Kind::Project, "note", "acme note", "note")];
        for index in 0..17 {
            let name = format!("piece{index:02}");
            facts.push(edge(
                Predicate::PartOf,
                &name,
                Kind::Component,
                "acme",
                Kind::Project,
            ));
        }
        commit(&gm, "s1", "2026-03-01", &[turn("pieces")], &facts).await;
        let loaded = super::load_rows(&gm, "s9", &["acme".into()], &[])
            .await
            .unwrap();
        let project = loaded
            .owners
            .iter()
            .find(|(_, name)| name == "acme")
            .expect("acme")
            .0
            .clone();
        let mut response = gm
            .db()
            .query(
                "SELECT id, in FROM type::record($id)<-(relates_to WHERE rel_type = 'part_of' AND status = 'decided' AND valid_until = NONE) ORDER BY id",
            )
            .bind(("id", project))
            .await
            .unwrap();
        let edges: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        assert!(edges.len() >= 17, "{}", edges.len());
        let first: BTreeSet<String> = edges
            .iter()
            .take(16)
            .map(|edge| super::id_text(edge.get("in")))
            .collect();
        let loaded_ids: BTreeSet<String> = loaded
            .owners
            .iter()
            .filter(|(_, name)| name != "acme")
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(loaded_ids.len(), 16);
        assert_eq!(loaded_ids, first);
    }

    fn one(text: &str) -> (String, String) {
        let rows = parse_reply(text).expect("reply");
        assert_eq!(rows.len(), 1);
        (rows[0].1.clone(), rows[0].2.clone())
    }

    #[test]
    fn fenced_wrapped_and_first_object_only() {
        assert_ne!(PROMPT, "CONSOLIDATE_V1");
        assert_eq!(MAX_TOKENS, 400);
        let tail = SYSTEM.lines().last().expect("system");
        assert!(tail.contains("one JSON object and nothing else"));
        assert!(tail.contains("no prose") && tail.contains("no markdown fence"));
        assert!(["supersedes", "duplicate", "unrelated", "unsure"]
            .iter()
            .all(|verdict| tail.contains(&format!("\"verdict\":\"{verdict}\""))));
        let kept = judged(&[(1, "unrelated", "a { b }")]);
        assert_eq!(one(&kept), ("unrelated".into(), "a { b }".into()));
        // A lone brace and an escaped quote inside a string do not end the object.
        let lone = judged(&[(1, "unsure", "a } b \" c")]);
        assert_eq!(one(&format!("note {lone} end")).1, "a } b \" c");
        let fenced = format!("```json\n{}\n```", judged(&[(1, "duplicate", "same fact")]));
        assert_eq!(one(&fenced).0, "duplicate");
        let wrapped = format!("note\n{}\nend", judged(&[(1, "unsure", "")]));
        assert_eq!(one(&wrapped).0, "unsure");
        let later = judged(&[(1, "supersedes", "later")]);
        assert_eq!(one(&format!("{kept} {later}")).0, "unrelated");
        assert!(parse_reply(&format!("{{}} {later}")).is_err());
        assert!(parse_reply("1. unrelated {\"verdict\":\"supersedes\"}\n2. unrelated").is_err());
        assert!(parse_reply("1. unrelated\n2. unrelated\n\nboth can hold").is_err());
    }

    #[tokio::test]
    async fn fenced_wrapped_and_prose_follow_the_count() {
        let turn_text = "Switch to rich text instead of plain text.";
        let fenced = format!(
            "```json\n{}\n```",
            judged(&[(1, "supersedes", "instead of plain text")])
        );
        let (report, slot) = judge_turn(turn_text, &fenced).await;
        assert_eq!(
            (report.consolidate.errors, report.consolidate.closes_sup),
            (0, 1)
        );
        assert_eq!(ended(&slot), Some("s2:consolidated"));
        let wrapped = format!(
            "note\n{}\nthanks",
            judged(&[(1, "supersedes", "not in the evidence at all")])
        );
        let (report, slot) = judge_turn(turn_text, &wrapped).await;
        assert_eq!(
            (report.consolidate.errors, report.consolidate.closes_sup),
            (0, 0)
        );
        assert!(ended(&slot).is_none());
        let (report, slot) =
            judge_turn(turn_text, "1. unrelated\n2. unrelated\n\nboth can hold").await;
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(
            (
                report.consolidate.errors,
                report.consolidate.closes_sup,
                report.consolidate.closes_dup
            ),
            (1, 0, 0)
        );
        assert!(ended(&slot).is_none());
    }

    #[tokio::test]
    async fn a_v1_cache_row_is_not_returned() {
        let (_tmp, gm) = open().await;
        define_cache(&gm).await.unwrap();
        let parts = [
            "quartz beacon",
            "quartz beacon",
            "quartz marker",
            "Keep the quartz beacon for now.",
        ];
        let v1 = digest_key(parts, "CONSOLIDATE_V1");
        let current = digest_key(parts, PROMPT);
        assert_ne!(v1, current);
        gm.db()
            .query(format!("CREATE consolidation_cache SET key = '{v1}', verdict = 'supersedes', quote = 'instead of quartz marker', prompt = 'CONSOLIDATE_V1', at = time::now()"))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(cache_get(&gm, &current).await.unwrap().is_none());
        assert_eq!(cache_get(&gm, &v1).await.unwrap().unwrap().0, "supersedes");
    }

    async fn commit_flags(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        match_on: bool,
        hold_on: bool,
    ) -> WriteStats {
        let project = project();
        let mut ctx = WriteCtx::new(&Quiet, &project, session, date, turns);
        ctx.match_on = match_on;
        ctx.hold_on = hold_on;
        write_facts(gm, &ctx, facts).await.unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn pass_hold(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        stats: &WriteStats,
        llm: &Stub,
        hold_on: bool,
    ) -> IngestionReport {
        let project = project();
        let mut ctx = WriteCtx::new(llm, &project, session, date, turns);
        ctx.hold_on = hold_on;
        let mut report = IngestionReport::default();
        run(gm, &ctx, facts, stats, llm, &mut report).await;
        report
    }

    fn slot_named<'a>(slots: &'a [Value], key: &str, value: &str) -> &'a Value {
        slots
            .iter()
            .find(|slot| {
                slot.get("key").and_then(Value::as_str) == Some(key)
                    && slot.get("value").and_then(Value::as_str) == Some(value)
            })
            .unwrap()
    }

    async fn seed_binder(gm: &GraphMemory) {
        let turns = [turn("The binder is canvas.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "canvas",
            "The binder is canvas.",
        )];
        commit_flags(gm, "s1", "2026-04-01", &turns, &facts, false, false).await;
    }

    #[tokio::test]
    async fn hold_off_writes_no_pending_field() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let turns = [turn("Store the pages in linen.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        let stats = commit_flags(&gm, "s2", "2026-04-02", &turns, &facts, true, false).await;
        assert_eq!(stats.holds_opened, 0);
        assert_eq!(stats.match_skips.get("no_mention").copied(), Some(1));
        let rows = slots(&gm, "acme").await;
        let canvas = slot_named(&rows, "binder", "canvas");
        let linen = slot_named(&rows, "binder", "linen");
        assert!(canvas.get("pending").is_none());
        assert!(linen.get("pending").is_none());
        assert!(ended(canvas).is_none());
        assert!(ended(linen).is_none());
    }

    #[tokio::test]
    async fn hold_stores_the_new_value_pending_and_keeps_the_old_live() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let before = slots(&gm, "acme").await;
        let alpha = slot_named(&before, "binder", "canvas")["alpha"].clone();
        let turns = [turn("Store the pages in linen.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        let stats = commit_flags(&gm, "s2", "2026-04-02", &turns, &facts, true, true).await;
        assert_eq!(stats.holds_opened, 1);
        assert!(stats.match_skips.is_empty(), "{:?}", stats.match_skips);
        assert!(stats.retirements.is_empty());
        let rows = slots(&gm, "acme").await;
        let canvas = slot_named(&rows, "binder", "canvas");
        let linen = slot_named(&rows, "binder", "linen");
        assert!(ended(canvas).is_none());
        assert_eq!(canvas["alpha"], alpha);
        let pending = linen.get("pending").expect("pending");
        assert_eq!(pending["since"], "s2");
        assert!(pending["conflicts"].as_str().unwrap().ends_with("#0"));
        let project = project();
        let live = super::super::known::read_live(&gm, &project, "binder canvas linen").await;
        assert!(live.iter().any(|row| row.value == "canvas"));
        assert!(live.iter().all(|row| row.value != "linen"));
        let block = super::super::known::render_known(&live);
        assert!(block.contains("canvas"));
        assert!(!block.contains("linen"));
    }

    #[tokio::test]
    async fn a_second_conflict_is_counted_as_a_skip() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .unwrap();
        let mut attributes = entity.attributes.clone().unwrap();
        let stored = attributes["v2"]["slots"].as_array_mut().unwrap();
        let mut silk = stored[0].clone();
        silk["value"] = json!("silk");
        stored.push(silk);
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", entity.id_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
        let turns = [turn("Store the pages in linen.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        let stats = commit_flags(&gm, "s2", "2026-04-02", &turns, &facts, true, true).await;
        assert_eq!(stats.holds_opened, 1);
        assert_eq!(stats.match_skips.get("no_mention").copied(), Some(1));
        let rows = slots(&gm, "acme").await;
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
        assert!(ended(slot_named(&rows, "binder", "silk")).is_none());
        let linen = slot_named(&rows, "binder", "linen");
        assert!(linen.get("pending").is_some());
    }

    #[tokio::test]
    async fn a_restatement_keeps_the_pending_value_and_records_evidence() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let quote = "Keep using linen.";
        let turns = [turn(quote)];
        let facts = [setting("acme", Kind::Project, "binder", "linen", quote)];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        assert_eq!(stats.holds_confirmed, 0);
        assert_eq!(stats.holds_opened, 0);
        assert_eq!(stats.already_seen, 0);
        assert_eq!(stats.reinforced, 1);
        let rows = slots(&gm, "acme").await;
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
        let linen = slot_named(&rows, "binder", "linen");
        assert!(linen.get("pending").is_some());
        assert!(ended(linen).is_none());
        let evidence = linen["evidence"].as_array().unwrap();
        assert!(evidence.iter().any(|item| {
            item["session"] == "s3" && item["quote"] == quote && item["turn"] == 0
        }));
    }

    #[tokio::test]
    async fn a_mismatched_hold_reference_is_not_written() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .unwrap();
        let mut attributes = entity.attributes.clone().unwrap();
        let linen = attributes["v2"]["slots"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|slot| slot["value"] == "linen")
            .unwrap();
        linen["pending"]["conflicts"] = json!("user:other");
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", entity.id_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
        let turns = [turn("Switched canvas for linen.")];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &[], true, true).await;
        let llm = Stub::new(Answer::Panic);
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &[], &stats, &llm, true).await;
        assert!(report.hold.errors >= 1);
        assert_eq!(report.hold.holds_confirmed, 0);
        let rows = slots(&gm, "acme").await;
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
    }

    #[tokio::test]
    async fn a_later_turn_cue_confirms_the_hold() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let quote = "Moved off canvas yesterday.";
        let turns = [
            turn("The label is north."),
            Turn {
                index: 1,
                role: TurnRole::User,
                text: quote.into(),
            },
        ];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &[], &stats, &llm, true).await;
        assert_eq!(report.hold.errors, 0, "{:?}", report.consolidate.dropped);
        assert_eq!(report.hold.holds_confirmed, 1);
        let rows = slots(&gm, "acme").await;
        assert_eq!(
            ended(slot_named(&rows, "binder", "canvas")),
            Some("s3:consolidated")
        );
        assert!(slot_named(&rows, "binder", "linen")
            .get("pending")
            .is_none());
    }

    #[tokio::test]
    async fn a_later_departure_confirms_the_hold() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [
            turn("Switched canvas for linen."),
            Turn {
                index: 1,
                role: TurnRole::User,
                text: "The label is north.".into(),
            },
        ];
        let mut label = setting(
            "acme",
            Kind::Project,
            "label",
            "north",
            "The label is north.",
        );
        label.turn = 1;
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &[label], true, true).await;
        let llm = Stub::new(Answer::Panic);
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &[], &stats, &llm, true).await;
        assert_eq!(llm.calls(), 0);
        assert_eq!(report.hold.holds_confirmed, 1);
        let rows = slots(&gm, "acme").await;
        assert_eq!(
            ended(slot_named(&rows, "binder", "canvas")),
            Some("s3:superseded")
        );
        let linen = slot_named(&rows, "binder", "linen");
        assert!(linen.get("pending").is_none());
        assert!(ended(linen).is_none());
    }

    #[tokio::test]
    async fn a_later_reassertion_drops_the_pending_row() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("The binder stays canvas.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "canvas",
            "The binder stays canvas.",
        )];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        let llm = Stub::new(Answer::Panic);
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &facts, &stats, &llm, true).await;
        assert_eq!(report.hold.holds_dropped, 1);
        let rows = slots(&gm, "acme").await;
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
        assert_eq!(
            ended(slot_named(&rows, "binder", "linen")),
            Some("s3:hold_dropped")
        );
    }

    #[tokio::test]
    async fn ambiguous_evidence_keeps_the_hold_in_the_consolidation_batch() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("The harbor bell is brass.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "bell",
            "brass",
            "The harbor bell is brass.",
        )];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        let llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &facts, &stats, &llm, true).await;
        assert_eq!(llm.calls(), 1);
        let message = llm.message();
        assert!(message.contains("canvas"), "{message}");
        assert!(message.contains("linen"), "{message}");
        assert_eq!(report.hold.holds_ambiguous, 1);
        let rows = slots(&gm, "acme").await;
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
        assert!(slot_named(&rows, "binder", "linen")
            .get("pending")
            .is_some());
    }

    #[tokio::test]
    async fn a_restated_replace_confirms_the_hold() {
        let (tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        std::fs::write(
            tmp.path().join(".recall-echo.toml"),
            "[graph]\nknown_match = true\nconsolidate = true\nconsolidate_hold = true\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("conversations")).unwrap();
        std::fs::write(
            tmp.path().join("conversations/conversation-001.md"),
            "---\nlog: 1\ndate: \"2026-04-03T00:00:00Z\"\nsession_id: \"s3\"\n---\n\n\
             ### User\n\nSwitched canvas for linen.\n",
        )
        .unwrap();
        struct Confirm;
        #[async_trait]
        impl LlmProvider for Confirm {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Ok(r#"{"pairs":[]}"#.to_string())
            }

            async fn complete_structured_chat(
                &self,
                _: &str,
                _: &[crate::graph::llm::Message],
                _: &Value,
                _: u32,
            ) -> Result<Completion, GraphError> {
                Ok(Completion::unmeasured(
                    r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"binder","object":"linen","object_type":"literal","status":"decided","replaces":"canvas","turn":0,"quote":"Switched canvas for linen.","confidence":0.9}],"bugs":[],"releases":[]}"#,
                ))
            }
        }
        let report = super::super::pass1::extract_session(
            &gm,
            tmp.path(),
            1,
            &IngestContext::new("s3", Some(1)),
            &Confirm,
        )
        .await
        .unwrap();
        assert!(report.hold.holds_confirmed >= 1, "{:?}", report.hold);
        assert_eq!(report.hold.errors, 0);
        let rows = slots(&gm, "acme").await;
        let canvas = slot_named(&rows, "binder", "canvas");
        let linen = slot_named(&rows, "binder", "linen");
        assert!(ended(canvas).is_some(), "canvas stayed live");
        assert!(linen.get("pending").is_none(), "linen still pending");
        assert!(ended(linen).is_none());
        let live: Vec<_> = rows
            .iter()
            .filter(|slot| {
                slot.get("valid_until").is_none_or(Value::is_null) && slot.get("pending").is_none()
            })
            .filter_map(|slot| slot.get("value").and_then(Value::as_str))
            .collect();
        assert_eq!(live, vec!["linen"]);
    }

    #[tokio::test]
    async fn a_third_value_does_not_promote_the_pending_rival() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("Switched canvas for silk.")];
        let mut fact = setting(
            "acme",
            Kind::Project,
            "binder",
            "silk",
            "Switched canvas for silk.",
        );
        fact.replaces = "canvas".into();
        let facts = [fact];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        let llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let _report = pass_hold(&gm, "s3", "2026-04-03", &turns, &facts, &stats, &llm, true).await;
        let rows = slots(&gm, "acme").await;
        let silk = slot_named(&rows, "binder", "silk");
        let linen = slot_named(&rows, "binder", "linen");
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_some());
        assert!(silk.get("pending").is_none());
        assert!(ended(silk).is_none());
        assert!(
            linen.get("pending").is_some(),
            "linen was promoted beside silk"
        );
    }

    async fn uses_edges(gm: &GraphMemory) -> Vec<Value> {
        let mut response = gm
            .db()
            .query(
                "SELECT id, pending, valid_until, ended_by, description, out.name AS object FROM relates_to WHERE rel_type = 'uses'",
            )
            .await
            .unwrap();
        deserialize_take(&mut response, 0).unwrap()
    }

    fn edge_named<'a>(edges: &'a [Value], object: &str) -> &'a Value {
        edges
            .iter()
            .find(|edge| edge.get("object").and_then(Value::as_str) == Some(object))
            .unwrap_or_else(|| panic!("missing uses {object}"))
    }

    fn live_values(rows: &[Value]) -> Vec<String> {
        rows.iter()
            .filter(|slot| {
                slot.get("valid_until").is_none_or(Value::is_null) && slot.get("pending").is_none()
            })
            .filter_map(|slot| {
                slot.get("value")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_shared_short_value_does_not_bind_another_subject() {
        let (_tmp, gm) = open().await;
        let on = [turn("The flag is on.")];
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &on,
            &[setting(
                "acme",
                Kind::Project,
                "flag",
                "on",
                "The flag is on.",
            )],
            false,
            false,
        )
        .await;
        commit_flags(
            &gm,
            "s1b",
            "2026-04-01",
            &on,
            &[setting(
                "bravo",
                Kind::Project,
                "flag",
                "on",
                "The flag is on.",
            )],
            false,
            false,
        )
        .await;
        let off = [turn("Store the pages in off.")];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &off,
            &[setting(
                "acme",
                Kind::Project,
                "flag",
                "off",
                "Store the pages in off.",
            )],
            true,
            true,
        )
        .await;
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .unwrap();
        let mut attributes = entity.attributes.clone().unwrap();
        attributes["v2"]["slots"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|slot| slot.get("value").and_then(Value::as_str) == Some("on"))
            .unwrap()
            .as_object_mut()
            .unwrap()
            .extend([
                ("valid_until".into(), json!("2026-04-02T00:00:00Z")),
                ("ended_by".into(), json!("s2:consolidated")),
            ]);
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", entity.id_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
        let turns = [turn("The harbor bell rang twice today.")];
        let project = ProjectIdentity {
            name: "acme".into(),
            aliases: vec!["bravo".into()],
        };
        let llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let mut ctx = WriteCtx::new(&llm, &project, "s3", "2026-04-03", &turns);
        ctx.hold_on = true;
        let stats = WriteStats::default();
        let mut report = IngestionReport::default();
        run(&gm, &ctx, &[], &stats, &llm, &mut report).await;
        let bravo = slots(&gm, "bravo").await;
        assert!(ended(slot_named(&bravo, "flag", "on")).is_none());
        assert!(report.hold.errors >= 1, "{:?}", report.hold);
    }

    #[tokio::test]
    async fn a_cue_without_a_close_leaves_the_row_pending() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("Switched canvas for linen.")];
        let mut fact = setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Switched canvas for linen.",
        );
        fact.replaces = "canvas".into();
        commit_flags(&gm, "s3", "not-a-day", &turns, &[fact], true, true).await;
        let rows = slots(&gm, "acme").await;
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
        assert!(!field_absent(
            slot_named(&rows, "binder", "linen").get("pending")
        ));
    }

    #[tokio::test]
    async fn an_unrelated_replaces_name_does_not_release_an_edge_hold() {
        let (_tmp, gm) = open().await;
        let turns = [turn("acme links canvas")];
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &turns,
            &[edge(
                Predicate::Uses,
                "acme",
                Kind::Project,
                "canvas",
                Kind::Technology,
            )],
            false,
            false,
        )
        .await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &[turn("Keep canvas and linen.")],
            &[linen],
            true,
            true,
        )
        .await;
        let mut again = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        again.replaces = "jute".into();
        again.quote = "Replaced jute with linen.".into();
        commit_flags(
            &gm,
            "s3",
            "2026-04-03",
            &[turn("Replaced jute with linen.")],
            &[again],
            true,
            true,
        )
        .await;
        let edges = uses_edges(&gm).await;
        assert!(ended_edge(edge_named(&edges, "canvas")).is_none());
        assert!(!field_absent(edge_named(&edges, "linen").get("pending")));
    }

    #[tokio::test]
    async fn a_live_copy_drops_the_pending_duplicate() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .unwrap();
        let mut attributes = entity.attributes.clone().unwrap();
        let stored = attributes["v2"]["slots"].as_array_mut().unwrap();
        let mut live = stored
            .iter()
            .find(|slot| slot.get("value").and_then(Value::as_str) == Some("linen"))
            .unwrap()
            .clone();
        live.as_object_mut().unwrap().remove("pending");
        stored.push(live);
        stored
            .iter_mut()
            .find(|slot| slot.get("value").and_then(Value::as_str) == Some("canvas"))
            .unwrap()
            .as_object_mut()
            .unwrap()
            .extend([
                ("valid_until".into(), json!("2026-04-02T00:00:00Z")),
                ("ended_by".into(), json!("s2:consolidated")),
            ]);
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", entity.id_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
        let turns = [turn("Switched canvas for linen.")];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &[], &stats, &llm, true).await;
        let rows = slots(&gm, "acme").await;
        assert_eq!(live_values(&rows), vec!["linen".to_string()]);
        assert_eq!(report.hold.holds_confirmed, 0);
        assert!(report.hold.holds_dropped >= 1, "{:?}", report.hold);
    }

    #[tokio::test]
    async fn a_pending_uses_edge_stays_out_of_live_reads() {
        let (_tmp, gm) = open().await;
        let turns = [turn("acme links canvas")];
        let canvas = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "canvas",
            Kind::Technology,
        );
        commit_flags(&gm, "s1", "2026-04-01", &turns, &[canvas], false, false).await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        let hold_turns = [turn("Keep canvas and linen.")];
        let stats = commit_flags(&gm, "s2", "2026-04-02", &hold_turns, &[linen], true, true).await;
        assert_eq!(stats.holds_opened, 1);
        let edges = uses_edges(&gm).await;
        assert!(ended_edge(edge_named(&edges, "canvas")).is_none());
        assert!(!field_absent(edge_named(&edges, "linen").get("pending")));
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let acme = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .unwrap();
        let live = crate::graph::edge_view::live_edges_of(gm.db(), &acme.id_string())
            .await
            .unwrap();
        assert_eq!(live.len(), 1, "{live:?}");
    }

    #[tokio::test]
    async fn an_unrelated_uses_edge_does_not_drop_a_roleless_hold() {
        let (_tmp, gm) = open().await;
        let turns = [turn("acme links canvas and rust")];
        let facts = [
            edge(
                Predicate::Uses,
                "acme",
                Kind::Project,
                "canvas",
                Kind::Technology,
            ),
            edge(
                Predicate::Uses,
                "acme",
                Kind::Project,
                "rust",
                Kind::Technology,
            ),
        ];
        commit_flags(&gm, "s1", "2026-04-01", &turns, &facts, false, false).await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        let hold_turns = [turn("Keep canvas and linen.")];
        commit_flags(&gm, "s2", "2026-04-02", &hold_turns, &[linen], true, true).await;
        let before = uses_edges(&gm).await;
        assert!(
            !field_absent(edge_named(&before, "linen").get("pending")),
            "linen was not stored pending: {}",
            edge_named(&before, "linen")
        );
        let canvas_id = id_text(edge_named(&before, "canvas").get("id"));
        gm.db()
            .query(
                "UPDATE type::record($id) SET valid_until = <datetime>'2026-04-02T00:00:00Z', ended_by = 's2:consolidated'",
            )
            .bind(("id", canvas_id))
            .await
            .unwrap()
            .check()
            .unwrap();
        let later = [turn("We moved off canvas. Linen is in.")];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &later, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let report = pass_hold(&gm, "s3", "2026-04-03", &later, &[], &stats, &llm, true).await;
        let edges = uses_edges(&gm).await;
        let linen_edge = edge_named(&edges, "linen");
        let rust_edge = edge_named(&edges, "rust");
        assert!(field_absent(linen_edge.get("pending")), "{linen_edge}");
        assert!(field_absent(linen_edge.get("valid_until")), "{linen_edge}");
        assert!(ended_edge(linen_edge).is_none(), "{linen_edge}");
        assert!(field_absent(rust_edge.get("valid_until")), "{rust_edge}");
        assert!(ended_edge(rust_edge).is_none(), "{rust_edge}");
        assert_eq!(report.hold.holds_dropped, 0);
        assert!(report.hold.holds_confirmed >= 1, "{:?}", report.hold);
    }

    #[tokio::test]
    async fn a_same_role_uses_edge_drops_the_pending_hold() {
        let (_tmp, gm) = open().await;
        let mut canvas = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "canvas",
            Kind::Technology,
        );
        let mut rust_edge = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "rust",
            Kind::Technology,
        );
        canvas.role = Some(super::super::schema::Role::Database);
        rust_edge.role = Some(super::super::schema::Role::Database);
        let turns = [turn("acme links canvas and rust")];
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &turns,
            &[canvas, rust_edge],
            false,
            false,
        )
        .await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.role = Some(super::super::schema::Role::Database);
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        let hold_turns = [turn("Keep canvas and linen.")];
        commit_flags(&gm, "s2", "2026-04-02", &hold_turns, &[linen], true, true).await;
        let before = uses_edges(&gm).await;
        assert_eq!(
            edge_named(&before, "linen")
                .get("description")
                .and_then(Value::as_str),
            Some("role=database")
        );
        let canvas_id = id_text(edge_named(&before, "canvas").get("id"));
        gm.db()
            .query(
                "UPDATE type::record($id) SET valid_until = <datetime>'2026-04-02T00:00:00Z', ended_by = 's2:consolidated'",
            )
            .bind(("id", canvas_id))
            .await
            .unwrap()
            .check()
            .unwrap();
        let later = [turn("We moved off canvas. Linen is in.")];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &later, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let report = pass_hold(&gm, "s3", "2026-04-03", &later, &[], &stats, &llm, true).await;
        let edges = uses_edges(&gm).await;
        let linen_edge = edge_named(&edges, "linen");
        assert_eq!(
            ended_edge(linen_edge),
            Some("s3:hold_dropped"),
            "{linen_edge}"
        );
        assert!(ended_edge(edge_named(&edges, "rust")).is_none());
        assert!(report.hold.holds_dropped >= 1, "{:?}", report.hold);

        // A live edge of another role does not occupy this one, so the hold promotes.
        let (_tmp, gm) = open().await;
        let mut canvas = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "canvas",
            Kind::Technology,
        );
        let mut rust_edge = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "rust",
            Kind::Technology,
        );
        canvas.role = Some(super::super::schema::Role::Database);
        rust_edge.role = Some(super::super::schema::Role::Language);
        let turns = [turn("acme links canvas and rust")];
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &turns,
            &[canvas, rust_edge],
            false,
            false,
        )
        .await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.role = Some(super::super::schema::Role::Database);
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        let hold_turns = [turn("Keep canvas and linen.")];
        commit_flags(&gm, "s2", "2026-04-02", &hold_turns, &[linen], true, true).await;
        let before = uses_edges(&gm).await;
        let canvas_id = id_text(edge_named(&before, "canvas").get("id"));
        gm.db()
            .query(
                "UPDATE type::record($id) SET valid_until = <datetime>'2026-04-02T00:00:00Z', ended_by = 's2:consolidated'",
            )
            .bind(("id", canvas_id))
            .await
            .unwrap()
            .check()
            .unwrap();
        let later = [turn("We moved off canvas. Linen is in.")];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &later, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let report = pass_hold(&gm, "s3", "2026-04-03", &later, &[], &stats, &llm, true).await;
        let edges = uses_edges(&gm).await;
        let linen_edge = edge_named(&edges, "linen");
        assert!(field_absent(linen_edge.get("pending")), "{linen_edge}");
        assert!(ended_edge(linen_edge).is_none(), "{linen_edge}");
        assert!(field_absent(linen_edge.get("valid_until")), "{linen_edge}");
        assert!(ended_edge(edge_named(&edges, "rust")).is_none());
        assert_eq!(report.hold.holds_dropped, 0, "{:?}", report.hold);
        assert!(report.hold.holds_confirmed >= 1, "{:?}", report.hold);
    }

    #[tokio::test]
    async fn a_closed_conflict_releases_a_roleless_edge_hold() {
        let (_tmp, gm) = open().await;
        let canvas = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "canvas",
            Kind::Technology,
        );
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &[turn("acme links canvas")],
            &[canvas],
            false,
            false,
        )
        .await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &[turn("Keep canvas and linen.")],
            &[linen],
            true,
            true,
        )
        .await;
        let before = uses_edges(&gm).await;
        assert!(
            field_absent(edge_named(&before, "linen").get("description")),
            "the pending edge should have no role"
        );
        let canvas_id = id_text(edge_named(&before, "canvas").get("id"));
        gm.db()
            .query(
                "UPDATE type::record($id) SET valid_until = <datetime>'2026-04-02T00:00:00Z', ended_by = 's2:consolidated'",
            )
            .bind(("id", canvas_id))
            .await
            .unwrap()
            .check()
            .unwrap();
        let mut again = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        again.quote = "acme links linen.".into();
        let stats = commit_flags(
            &gm,
            "s3",
            "2026-04-03",
            &[turn("acme links linen.")],
            &[again],
            true,
            true,
        )
        .await;
        assert_eq!(stats.holds_confirmed, 1, "{stats:?}");
        let edges = uses_edges(&gm).await;
        let linen_edge = edge_named(&edges, "linen");
        assert!(field_absent(linen_edge.get("pending")), "{linen_edge}");
        assert!(ended_edge(linen_edge).is_none(), "{linen_edge}");
        assert!(field_absent(linen_edge.get("valid_until")), "{linen_edge}");
    }

    #[tokio::test]
    async fn a_same_role_live_peer_does_not_release_an_edge_hold() {
        let (_tmp, gm) = open().await;
        let mut canvas = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "canvas",
            Kind::Technology,
        );
        let mut rust_edge = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "rust",
            Kind::Technology,
        );
        canvas.role = Some(super::super::schema::Role::Database);
        rust_edge.role = Some(super::super::schema::Role::Database);
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &[turn("acme links canvas and rust")],
            &[canvas, rust_edge],
            false,
            false,
        )
        .await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.role = Some(super::super::schema::Role::Database);
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &[turn("Keep canvas and linen.")],
            &[linen],
            true,
            true,
        )
        .await;
        let before = uses_edges(&gm).await;
        assert_eq!(
            edge_named(&before, "linen")
                .get("description")
                .and_then(Value::as_str),
            Some("role=database")
        );
        let canvas_id = id_text(edge_named(&before, "canvas").get("id"));
        gm.db()
            .query(
                "UPDATE type::record($id) SET valid_until = <datetime>'2026-04-02T00:00:00Z', ended_by = 's2:consolidated'",
            )
            .bind(("id", canvas_id))
            .await
            .unwrap()
            .check()
            .unwrap();
        let mut again = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        again.role = Some(super::super::schema::Role::Database);
        again.quote = "acme links linen.".into();
        let stats = commit_flags(
            &gm,
            "s3",
            "2026-04-03",
            &[turn("acme links linen.")],
            &[again],
            true,
            true,
        )
        .await;
        assert_eq!(stats.holds_confirmed, 0, "{stats:?}");
        let edges = uses_edges(&gm).await;
        assert!(
            !field_absent(edge_named(&edges, "linen").get("pending")),
            "{}",
            edge_named(&edges, "linen")
        );
        assert!(ended_edge(edge_named(&edges, "rust")).is_none());

        // A live edge of another role does not occupy this one, so the write releases it.
        let (_tmp, gm) = open().await;
        let mut canvas = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "canvas",
            Kind::Technology,
        );
        let mut rust_edge = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "rust",
            Kind::Technology,
        );
        canvas.role = Some(super::super::schema::Role::Database);
        rust_edge.role = Some(super::super::schema::Role::Language);
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &[turn("acme links canvas and rust")],
            &[canvas, rust_edge],
            false,
            false,
        )
        .await;
        let mut linen = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        linen.role = Some(super::super::schema::Role::Database);
        linen.replaces = "canvas".into();
        linen.quote = "Keep canvas and linen.".into();
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &[turn("Keep canvas and linen.")],
            &[linen],
            true,
            true,
        )
        .await;
        let before = uses_edges(&gm).await;
        let canvas_id = id_text(edge_named(&before, "canvas").get("id"));
        gm.db()
            .query(
                "UPDATE type::record($id) SET valid_until = <datetime>'2026-04-02T00:00:00Z', ended_by = 's2:consolidated'",
            )
            .bind(("id", canvas_id))
            .await
            .unwrap()
            .check()
            .unwrap();
        let mut again = edge(
            Predicate::Uses,
            "acme",
            Kind::Project,
            "linen",
            Kind::Technology,
        );
        again.role = Some(super::super::schema::Role::Database);
        again.quote = "acme links linen.".into();
        let stats = commit_flags(
            &gm,
            "s3",
            "2026-04-03",
            &[turn("acme links linen.")],
            &[again],
            true,
            true,
        )
        .await;
        assert_eq!(stats.holds_confirmed, 1, "{stats:?}");
        let edges = uses_edges(&gm).await;
        let linen_edge = edge_named(&edges, "linen");
        assert!(field_absent(linen_edge.get("pending")), "{linen_edge}");
        assert!(ended_edge(linen_edge).is_none(), "{linen_edge}");
        assert!(field_absent(linen_edge.get("valid_until")), "{linen_edge}");
        assert!(ended_edge(edge_named(&edges, "rust")).is_none());
    }

    fn ended_edge(edge: &Value) -> Option<&str> {
        edge.get("ended_by").and_then(Value::as_str)
    }

    fn field_absent(value: Option<&Value>) -> bool {
        value.is_none_or(Value::is_null)
    }

    #[tokio::test]
    async fn a_mentioned_rival_is_dropped_beside_the_new_value() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("Switched canvas for silk. The linen samples go back.")];
        let mut fact = setting(
            "acme",
            Kind::Project,
            "binder",
            "silk",
            "Switched canvas for silk.",
        );
        fact.replaces = "canvas".into();
        let facts = [fact];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        let llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &facts, &stats, &llm, true).await;
        let rows = slots(&gm, "acme").await;
        assert_eq!(live_values(&rows), vec!["silk".to_string()]);
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_some());
        let linen = slot_named(&rows, "binder", "linen");
        assert!(ended(linen).is_some_and(|reason| reason.ends_with("hold_dropped")));
        assert_eq!(report.hold.holds_confirmed, 0);
        assert!(report.hold.holds_dropped >= 1);
    }

    #[tokio::test]
    async fn clearing_an_edge_without_pending_is_not_success() {
        let (_tmp, gm) = open().await;
        commit_flags(
            &gm,
            "s1",
            "2026-04-01",
            &[turn("acme links canvas")],
            &[edge(
                Predicate::Uses,
                "acme",
                Kind::Project,
                "canvas",
                Kind::Technology,
            )],
            false,
            false,
        )
        .await;
        let edges = uses_edges(&gm).await;
        let id = id_text(edge_named(&edges, "canvas").get("id"));
        assert!(super::super::hold::take_pending(&gm, &id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn an_empty_hold_does_not_load_every_entity() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        super::super::hold::reset_full_loads();
        let turns = [turn("The harbor bell rang twice today.")];
        let stats = WriteStats::default();
        let llm = Stub::new(Answer::Panic);
        let report = pass_hold(&gm, "s2", "2026-04-02", &turns, &[], &stats, &llm, true).await;
        assert_eq!(super::super::hold::full_loads(), 0);
        assert_eq!(report.hold.errors, 0);
    }

    #[tokio::test]
    async fn a_split_turn_does_not_confirm_a_hold() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &[turn("Store the pages in linen.")],
            &[setting(
                "acme",
                Kind::Project,
                "binder",
                "linen",
                "Store the pages in linen.",
            )],
            true,
            true,
        )
        .await;
        let turns = [turn("Switched canvas for silk.")];
        let mut fact = setting(
            "acme",
            Kind::Project,
            "binder",
            "silk",
            "Switched canvas for silk.",
        );
        fact.replaces = "canvas".into();
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &[fact], true, true).await;
        let llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let _ = pass_hold(&gm, "s3", "2026-04-03", &turns, &[], &stats, &llm, true).await;
        let later = [
            turn("We moved off canvas last month."),
            Turn {
                index: 1,
                role: TurnRole::User,
                text: "The linen swatches arrived today.".into(),
            },
        ];
        let later_stats = commit_flags(&gm, "s4", "2026-04-04", &later, &[], true, true).await;
        let later_llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let report = pass_hold(
            &gm,
            "s4",
            "2026-04-04",
            &later,
            &[],
            &later_stats,
            &later_llm,
            true,
        )
        .await;
        let rows = slots(&gm, "acme").await;
        assert!(!field_absent(
            slot_named(&rows, "binder", "linen").get("pending")
        ));
        assert_eq!(report.hold.holds_confirmed, 0);
        assert_eq!(report.hold.holds_dropped, 0);
    }

    #[tokio::test]
    async fn a_later_mention_drops_the_rival_beside_the_live_value() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("Switched canvas for silk.")];
        let mut fact = setting(
            "acme",
            Kind::Project,
            "binder",
            "silk",
            "Switched canvas for silk.",
        );
        fact.replaces = "canvas".into();
        let facts = [fact];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        let llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let _ = pass_hold(&gm, "s3", "2026-04-03", &turns, &facts, &stats, &llm, true).await;
        let later = [turn(
            "We moved off canvas last month. The linen swatches arrived today.",
        )];
        let later_stats = commit_flags(&gm, "s4", "2026-04-04", &later, &[], true, true).await;
        let later_llm = Stub::new(Answer::Reply(r#"{"pairs":[]}"#.into()));
        let report = pass_hold(
            &gm,
            "s4",
            "2026-04-04",
            &later,
            &[],
            &later_stats,
            &later_llm,
            true,
        )
        .await;
        let rows = slots(&gm, "acme").await;
        assert_eq!(live_values(&rows), vec!["silk".to_string()]);
        let linen = slot_named(&rows, "binder", "linen");
        assert!(ended(linen).is_some_and(|reason| reason.ends_with("hold_dropped")));
        assert_eq!(report.hold.holds_confirmed, 0);
        assert!(report.hold.holds_dropped >= 1);
    }

    #[tokio::test]
    async fn an_ambiguous_hold_reaches_the_judge_without_session_anchors() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let turns = [turn("The harbor bell rang twice today.")];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let report = pass_hold(&gm, "s3", "2026-04-03", &turns, &[], &stats, &llm, true).await;
        assert!(llm.calls() >= 1, "hold pair never reached the judge");
        assert!(report.consolidate.pairs >= 1 || llm.calls() >= 1);
        let rows = slots(&gm, "acme").await;
        assert!(slot_named(&rows, "binder", "linen")
            .get("pending")
            .is_some());
        assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
    }

    #[tokio::test]
    async fn a_failed_consolidation_batch_leaves_the_hold() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        commit_flags(
            &gm,
            "s2",
            "2026-04-02",
            &open_turns,
            &open_facts,
            true,
            true,
        )
        .await;
        let before = slots(&gm, "acme").await;
        let pending = slot_named(&before, "binder", "linen")["pending"].clone();
        let turns = [turn("The harbor bell is brass.")];
        let facts = [setting(
            "acme",
            Kind::Project,
            "bell",
            "brass",
            "The harbor bell is brass.",
        )];
        let stats = commit_flags(&gm, "s3", "2026-04-03", &turns, &facts, true, true).await;
        for answer in [Answer::Err, Answer::Bad] {
            let llm = Stub::new(answer);
            let report =
                pass_hold(&gm, "s3", "2026-04-03", &turns, &facts, &stats, &llm, true).await;
            assert!(report.consolidate.errors >= 1);
            assert_eq!(report.hold.holds_confirmed, 0);
            assert_eq!(report.hold.holds_dropped, 0);
            let rows = slots(&gm, "acme").await;
            assert_eq!(slot_named(&rows, "binder", "linen")["pending"], pending);
            assert!(ended(slot_named(&rows, "binder", "canvas")).is_none());
        }
    }

    #[tokio::test]
    async fn a_failed_old_stamp_recovers_through_settle() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let quote = "Store the pages in linen.";
        let turns = [turn(quote)];
        let facts = [setting("acme", Kind::Project, "binder", "linen", quote)];
        commit_flags(&gm, "s2", "2026-04-02", &turns, &facts, true, true).await;
        let rows = slots(&gm, "acme").await;
        let index = rows
            .iter()
            .position(|slot| slot["value"] == "linen")
            .unwrap();
        let pending = slot_named(&rows, "binder", "linen")["pending"].clone();
        let old_ref = pending["conflicts"].as_str().unwrap().to_string();
        let entity = old_ref.rsplit_once('#').unwrap().0;
        let new_ref = format!("{entity}#{index}");
        // Loaded value was canvas. The stored value no longer matches, so the
        // old-row stamp refuses while that row is still live.
        rewrite_slot(&gm, "acme", "binder", "canvas", "jute").await;
        let project = project();
        let confirm = [turn("Switched canvas for linen.")];
        let ctx = WriteCtx::new(&Quiet, &project, "s3", "2026-04-03", &confirm);
        let mut report = IngestionReport::default();
        let mut open = Vec::new();
        super::super::hold::consider(
            &gm,
            &ctx,
            &[],
            &mut report,
            &mut open,
            &super::super::hold::OpenHold {
                subject: "acme".into(),
                pred: "setting".into(),
                key: "binder".into(),
                new_value: "linen".into(),
                old_value: "canvas".into(),
                new_ref,
                old_ref,
            },
        )
        .await;
        let rows = slots(&gm, "acme").await;
        assert_eq!(live_values(&rows), vec!["jute".to_string()]);
        assert!(ended(slot_named(&rows, "binder", "jute")).is_none());
        let linen = slot_named(&rows, "binder", "linen");
        assert_eq!(linen["pending"], pending);
        assert!(ended(linen).is_none());
        assert!(report.hold.errors >= 1, "{:?}", report.hold);
        assert!(open.is_empty());
    }

    #[tokio::test]
    async fn a_failed_partner_close_recovers_through_the_judge() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let quote = "Store the pages in linen.";
        let turns = [turn(quote)];
        let facts = [setting("acme", Kind::Project, "binder", "linen", quote)];
        commit_flags(&gm, "s2", "2026-04-02", &turns, &facts, true, true).await;
        let pending = slot_named(&slots(&gm, "acme").await, "binder", "linen")["pending"].clone();
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .unwrap()
            .id_string();
        struct RewriteOnJudge {
            db: surrealdb::Surreal<crate::graph::store::Db>,
            entity: String,
        }
        #[async_trait]
        impl LlmProvider for RewriteOnJudge {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                panic!("plain complete was called");
            }

            async fn complete_structured(
                &self,
                _: &str,
                _: &str,
                _: &Value,
                _: u32,
            ) -> Result<Completion, GraphError> {
                let mut response = self
                    .db
                    .query("SELECT attributes FROM type::record($id)")
                    .bind(("id", self.entity.clone()))
                    .await
                    .unwrap();
                let rows: Vec<Value> = deserialize_take(&mut response, 0).unwrap();
                let mut attributes = rows[0]["attributes"].clone();
                let slots = attributes["v2"]["slots"].as_array_mut().unwrap();
                let canvas = slots
                    .iter_mut()
                    .find(|slot| slot["key"] == "binder" && slot["value"] == "canvas")
                    .unwrap();
                canvas["value"] = json!("jute");
                self.db
                    .query("UPDATE type::record($id) SET attributes = $attributes")
                    .bind(("id", self.entity.clone()))
                    .bind(("attributes", attributes))
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
                Ok(Completion::unmeasured(
                    r#"{"pairs":[{"i":1,"verdict":"supersedes","quote":"We moved off canvas."}]}"#,
                ))
            }
        }
        let judge = RewriteOnJudge {
            db: gm.db().clone(),
            entity,
        };
        let later = [turn("We moved off canvas.")];
        let project = project();
        let mut ctx = WriteCtx::new(&judge, &project, "s3", "2026-04-03", &later);
        ctx.hold_on = true;
        let mut report = IngestionReport::default();
        run(&gm, &ctx, &[], &WriteStats::default(), &judge, &mut report).await;
        let rows = slots(&gm, "acme").await;
        assert_eq!(live_values(&rows), vec!["jute".to_string()]);
        assert!(ended(slot_named(&rows, "binder", "jute")).is_none());
        let linen = slot_named(&rows, "binder", "linen");
        assert_eq!(linen["pending"], pending, "linen was left live: {linen}");
        assert!(ended(linen).is_none());
        assert!(report.hold.errors >= 1, "{:?}", report.hold);
    }

    #[tokio::test]
    async fn a_failed_pending_restore_does_not_leave_two_live_values() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let quote = "Store the pages in linen.";
        let turns = [turn(quote)];
        let facts = [setting("acme", Kind::Project, "binder", "linen", quote)];
        commit_flags(&gm, "s2", "2026-04-02", &turns, &facts, true, true).await;
        let rows = slots(&gm, "acme").await;
        let index = rows
            .iter()
            .position(|slot| slot["value"] == "linen")
            .unwrap();
        let conflicts = slot_named(&rows, "binder", "linen")["pending"]["conflicts"]
            .as_str()
            .unwrap();
        let entity = conflicts.rsplit_once('#').unwrap().0;
        let new_ref = format!("{entity}#{index}");
        let taken = super::super::hold::take_pending(&gm, &new_ref)
            .await
            .unwrap()
            .expect("pending object");
        assert!(taken.is_object());
        let taken_live = live_values(&slots(&gm, "acme").await);
        assert!(
            taken_live.iter().any(|value| value == "canvas")
                && taken_live.iter().any(|value| value == "linen"),
            "take left one value: {taken_live:?}"
        );
        // A non-object is not a pending payload, so the restore returns Ok(false).
        let outcome = super::super::hold::recover_pending(
            &gm,
            &super::super::hold::PendingTarget {
                new_ref: &new_ref,
                pred: "setting",
                key: "binder",
                new_value: "linen",
            },
            &json!("not-an-object"),
            "2026-04-03T00:00:00Z",
            "s3:hold_dropped",
        )
        .await
        .unwrap();
        assert_eq!(outcome, super::super::hold::PendingRecover::Dropped);
        let rows = slots(&gm, "acme").await;
        assert_eq!(live_values(&rows), vec!["canvas".to_string()]);
        assert_eq!(
            ended(slot_named(&rows, "binder", "linen")),
            Some("s3:hold_dropped")
        );
    }

    #[tokio::test]
    async fn two_supersedes_on_one_old_row_promote_one_hold() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        for (session, date, value) in [("s2", "2026-04-02", "linen"), ("s3", "2026-04-03", "silk")]
        {
            let quote = format!("Store the pages in {value}.");
            let turns = [turn(&quote)];
            let facts = [setting("acme", Kind::Project, "binder", value, &quote)];
            commit_flags(&gm, session, date, &turns, &facts, true, true).await;
        }
        let turns = [turn("We moved off canvas.")];
        let stats = commit_flags(&gm, "s4", "2026-04-04", &turns, &[], true, true).await;
        let llm = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"supersedes","quote":"We moved off canvas."},{"i":2,"verdict":"supersedes","quote":"We moved off canvas."}]}"#.into(),
        ));
        let report = pass_hold(&gm, "s4", "2026-04-04", &turns, &[], &stats, &llm, true).await;
        let rows = slots(&gm, "acme").await;
        let live = live_values(&rows);
        assert!(live.len() <= 1, "two live binder values: {live:?}");
        let promoted = ["linen", "silk"]
            .into_iter()
            .filter(|value| live.iter().any(|row| row == value))
            .count();
        assert_eq!(promoted, 1, "live {live:?} hold {:?}", report.hold);
        for value in ["linen", "silk"] {
            let slot = slot_named(&rows, "binder", value);
            let is_live = slot.get("pending").is_none() && ended(slot).is_none();
            if !is_live {
                assert!(
                    slot.get("pending").is_some()
                        || ended(slot).is_some_and(|reason| reason.ends_with("hold_dropped")),
                    "{value} became a second live value: {slot}"
                );
            }
        }
    }

    async fn commit_linked(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
    ) -> WriteStats {
        let project = project();
        let mut ctx = WriteCtx::new(&Quiet, &project, session, date, turns);
        ctx.links_on = true;
        write_facts(gm, &ctx, facts).await.unwrap()
    }

    async fn pass_linked(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        stats: &WriteStats,
        llm: &Stub,
    ) -> IngestionReport {
        let project = project();
        let mut ctx = WriteCtx::new(llm, &project, session, date, turns);
        ctx.links_on = true;
        let mut report = IngestionReport {
            links: stats.links,
            ..IngestionReport::default()
        };
        run(gm, &ctx, facts, stats, llm, &mut report).await;
        report
    }

    fn rule(subject: &str, kind: Kind, text: &str, quote: &str) -> Fact {
        let mut fact = setting(subject, kind, "", text, quote);
        fact.predicate = Predicate::Rule;
        fact
    }

    fn uses(subject: &str, object: &str, quote: &str) -> Fact {
        let mut fact = edge(
            Predicate::Uses,
            subject,
            Kind::Project,
            object,
            Kind::Technology,
        );
        fact.quote = quote.into();
        fact
    }

    async fn slot_canonical(gm: &GraphMemory, name: &str, value: &str) -> String {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities.iter().find(|entity| entity.name == name).unwrap();
        let slots = entity.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap();
        let index = slots
            .iter()
            .position(|slot| slot["value"] == value)
            .unwrap_or_else(|| panic!("missing {value} on {name}"));
        format!("{}#{index}", entity.id_string())
    }

    fn derived_marker(value: &str) -> String {
        format!("NEW=\"{value}\" (s2, reason: derived)")
    }

    #[tokio::test]
    async fn a_rule_opened_this_session_keeps_its_new_anchor_only() {
        let (_tmp, gm) = open().await;
        let premise = uses("harbor", "queue", "harbor uses queue");
        let partner = rule(
            "birch",
            Kind::Project,
            "keep one spool ready for the workers",
            "keep one spool ready for the workers",
        );
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("harbor uses queue")],
            &[premise, partner],
        )
        .await;
        let fresh = "keep one queue ready for the workers";
        let mut created = rule("birch", Kind::Project, fresh, fresh);
        created.turn = 0;
        let quote = "queue is retired from the dock";
        let mut rejection = uses("harbor", "queue", quote);
        rejection.status = "rejected".into();
        rejection.turn = 1;
        let turns = [
            turn(fresh),
            Turn {
                index: 1,
                role: TurnRole::User,
                text: quote.into(),
            },
        ];
        let stats = commit_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            &[created.clone(), rejection.clone()],
        )
        .await;
        assert_eq!(stats.close_events.len(), 1, "{:?}", stats.close_events);
        let birch = slots(&gm, "birch").await;
        let linked = slot_of(&birch, fresh);
        assert!(
            linked
                .get("derived_from")
                .and_then(Value::as_array)
                .is_some_and(|ids| !ids.is_empty()),
            "{linked}"
        );
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unsure","quote":""}]}"#.into(),
        ));
        let report = pass_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            &[created, rejection],
            &stats,
            &stub,
        )
        .await;
        assert_eq!(report.links.d_anchors_added, 0);
        assert_eq!(report.links.d_anchor_overflow, 0);
        let message = stub.message();
        let lines: Vec<_> = message
            .lines()
            .filter(|line| line.contains(fresh))
            .collect();
        assert_eq!(lines.len(), 1, "{message}");
        assert!(lines[0].contains("reason: new"), "{message}");
        assert!(!message.contains("reason: derived"), "{message}");
        let birch = slots(&gm, "birch").await;
        assert!(ended(slot_of(&birch, fresh)).is_none());
    }

    #[tokio::test]
    async fn a_rejected_premise_adds_a_live_dependent_anchor() {
        let (_tmp, gm) = open().await;
        let premise = uses("harbor", "queue", "harbor uses queue");
        let dependent = rule(
            "harbor",
            Kind::Project,
            "keep one queue ready for the workers",
            "keep one queue ready for the workers",
        );
        let partner = rule(
            "harbor",
            Kind::Project,
            "keep one spool ready for the workers",
            "keep one spool ready for the workers",
        );
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("harbor uses queue")],
            &[premise, dependent.clone(), partner],
        )
        .await;
        let quote = "queue is retired from the dock";
        let mut rejection = uses("harbor", "queue", quote);
        rejection.status = "rejected".into();
        let turns = [turn(quote)];
        let stats = commit_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            std::slice::from_ref(&rejection),
        )
        .await;
        assert_eq!(stats.retirements.len(), 1, "{:?}", stats.retirements);
        assert_eq!(stats.retirements[0].reason, "rejected");
        assert_eq!(stats.close_events.len(), 1, "{:?}", stats.close_events);
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unsure","quote":""}]}"#.into(),
        ));
        let report =
            pass_linked(&gm, "s2", "2026-04-11", &turns, &[rejection], &stats, &stub).await;
        assert_eq!(report.links.d_anchors_added, 1);
        assert_eq!(report.links.d_anchor_overflow, 0);
        assert!(report.consolidate.anchors >= 2, "{:?}", report.consolidate);
        assert!(report.consolidate.pairs <= 6, "{:?}", report.consolidate);
        assert_eq!(report.consolidate.calls, 1);
        assert_eq!(stub.calls(), 1);
        let message = stub.message();
        assert!(
            message.contains(&derived_marker("keep one queue ready for the workers")),
            "{message}"
        );
        assert!(
            message.contains("keep one spool ready for the workers"),
            "{message}"
        );
        let rows = slots(&gm, "harbor").await;
        assert!(ended(slot_of(&rows, "keep one queue ready for the workers")).is_none());
        assert!(ended(slot_of(&rows, "keep one spool ready for the workers")).is_none());
    }

    #[tokio::test]
    async fn a_superseded_premise_adds_a_live_dependent_anchor() {
        let (_tmp, gm) = open().await;
        let original = setting(
            "maple",
            Kind::Project,
            "delay",
            "north pier marker",
            "delay is north pier marker",
        );
        let dependent = setting(
            "birch",
            Kind::Project,
            "delay",
            "north pier marker",
            "birch delay is north pier marker",
        );
        let partner = setting(
            "birch",
            Kind::Project,
            "note",
            "north pier beacon",
            "birch note is north pier beacon",
        );
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("delay is north pier marker")],
            &[original, dependent, partner],
        )
        .await;
        let quote = "delay is now south pier marker";
        let next = setting("maple", Kind::Project, "delay", "south pier marker", quote);
        let turns = [turn(quote)];
        let stats =
            commit_linked(&gm, "s2", "2026-04-11", &turns, std::slice::from_ref(&next)).await;
        assert!(
            stats
                .retirements
                .iter()
                .any(|item| item.reason == "superseded"),
            "{:?}",
            stats.retirements
        );
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unsure","quote":""}]}"#.into(),
        ));
        let report = pass_linked(&gm, "s2", "2026-04-11", &turns, &[next], &stats, &stub).await;
        assert_eq!(report.links.d_anchors_added, 1);
        assert_eq!(stub.calls(), 1);
        assert!(report.consolidate.pairs <= 6);
        assert!(report.consolidate.calls <= 1);
        let message = stub.message();
        assert!(
            message.contains(&derived_marker("north pier marker")),
            "{message}"
        );
        let birch = slots(&gm, "birch").await;
        assert!(ended(slot_named(&birch, "delay", "north pier marker")).is_none());
        assert!(ended(slot_named(&birch, "note", "north pier beacon")).is_none());
    }

    #[tokio::test]
    async fn a_replaced_premise_adds_a_live_dependent_anchor() {
        let (_tmp, gm) = open().await;
        let original = setting(
            "maple",
            Kind::Project,
            "delay",
            "north pier marker",
            "delay is north pier marker",
        );
        let dependent = setting(
            "birch",
            Kind::Project,
            "wait",
            "north pier marker",
            "wait is north pier marker",
        );
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("delay is north pier marker")],
            &[original, dependent],
        )
        .await;
        let quote = "Use south pier marker instead of north pier marker";
        let mut next = setting("maple", Kind::Project, "delay", "south pier marker", quote);
        next.replaces = "north pier marker".into();
        let turns = [turn(quote)];
        let stats =
            commit_linked(&gm, "s2", "2026-04-11", &turns, std::slice::from_ref(&next)).await;
        assert!(
            stats
                .retirements
                .iter()
                .any(|item| item.reason == "replaced"),
            "{:?}",
            stats.retirements
        );
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let report = pass_linked(&gm, "s2", "2026-04-11", &turns, &[next], &stats, &stub).await;
        assert_eq!(report.links.d_anchors_added, 1);
        assert!(report.consolidate.pairs <= 6);
        assert!(report.consolidate.calls <= 1);
        let birch = slots(&gm, "birch").await;
        assert!(ended(slot_named(&birch, "wait", "north pier marker")).is_none());
    }

    #[tokio::test]
    async fn a_dependent_without_a_partner_stays_live() {
        let (_tmp, gm) = open().await;
        let premise = uses("harbor", "queue", "harbor uses queue");
        let dependent = rule(
            "harbor",
            Kind::Project,
            "keep one queue ready for the workers",
            "keep one queue ready for the workers",
        );
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("harbor uses queue")],
            &[premise, dependent],
        )
        .await;
        let quote = "queue is retired from the dock";
        let mut rejection = uses("harbor", "queue", quote);
        rejection.status = "rejected".into();
        let turns = [turn(quote)];
        let stats = commit_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            std::slice::from_ref(&rejection),
        )
        .await;
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#.into(),
        ));
        let report =
            pass_linked(&gm, "s2", "2026-04-11", &turns, &[rejection], &stats, &stub).await;
        assert_eq!(report.links.d_anchors_added, 1);
        assert!(
            !stub.message().contains("reason: derived"),
            "{}",
            stub.message()
        );
        assert!(stub.calls() <= 1);
        let rows = slots(&gm, "harbor").await;
        assert!(ended(slot_of(&rows, "keep one queue ready for the workers")).is_none());
    }

    #[tokio::test]
    async fn promoted_and_consolidation_closes_add_no_derived_anchor() {
        let (_tmp, gm) = open().await;
        let mut considered = uses("harbor", "queue", "harbor might use queue");
        considered.status = "considered".into();
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("harbor might use queue")],
            &[considered],
        )
        .await;
        let decided = uses("harbor", "queue", "harbor uses queue");
        let promoted = commit_linked(
            &gm,
            "s2",
            "2026-04-04",
            &[turn("harbor uses queue")],
            std::slice::from_ref(&decided),
        )
        .await;
        assert!(
            promoted
                .retirements
                .iter()
                .any(|item| item.reason == "promoted"),
            "{:?}",
            promoted.retirements
        );
        assert!(
            promoted.close_events.is_empty(),
            "{:?}",
            promoted.close_events
        );
        let stub = Stub::new(Answer::Panic);
        let report = pass_linked(
            &gm,
            "s2",
            "2026-04-04",
            &[turn("harbor uses queue")],
            &[decided],
            &promoted,
            &stub,
        )
        .await;
        assert_eq!(report.links.d_anchors_added, 0);
        assert_eq!(stub.calls(), 0);

        let old = setting(
            "acme",
            Kind::Project,
            "pool",
            "logical cores",
            "pool is logical cores",
        );
        let linked = rule(
            "birch",
            Kind::Project,
            "keep logical cores available for the pool workers",
            "keep logical cores available for the pool workers",
        );
        commit_linked(
            &gm,
            "s3",
            "2026-04-05",
            &[turn("pool is logical cores")],
            &[old, linked],
        )
        .await;
        let quote = "Use 4 workers instead of the core count.";
        let mut next = setting("acme", Kind::Project, "workers", "4", quote);
        next.replaces = "core count".into();
        let turns = [turn(quote)];
        let stats =
            commit_linked(&gm, "s4", "2026-04-06", &turns, std::slice::from_ref(&next)).await;
        assert!(
            stats.replaces_unconsumed.contains(&0),
            "{:?}",
            stats.retirements
        );
        let stub = Stub::new(Answer::Panic);
        let report = pass_linked(&gm, "s4", "2026-04-06", &turns, &[next], &stats, &stub).await;
        assert_eq!(report.consolidate.closes_sup, 1);
        assert_eq!(report.links.d_anchors_added, 0);
        assert_eq!(stub.calls(), 0);
        let rows = slots(&gm, "birch").await;
        assert!(ended(slot_of(
            &rows,
            "keep logical cores available for the pool workers"
        ))
        .is_none());
        assert_eq!(
            ended(live_any(&slots(&gm, "acme").await, "pool", "logical cores")),
            Some("s4:consolidated")
        );
    }

    #[tokio::test]
    async fn derived_anchors_keep_the_first_four_ids_and_share_the_pair_cap() {
        let (_tmp, gm) = open().await;
        let names = ["maple", "birch", "holly", "ivory", "ochre"];
        let mut facts = vec![uses("harbor", "queue", "harbor uses queue")];
        for name in names {
            facts.push(rule(
                name,
                Kind::Project,
                &format!("keep one queue ready for {name} workers"),
                &format!("keep one queue ready for {name} workers"),
            ));
            facts.push(rule(
                name,
                Kind::Project,
                &format!("keep one spool ready for {name} workers"),
                &format!("keep one spool ready for {name} workers"),
            ));
        }
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("harbor uses queue")],
            &facts,
        )
        .await;
        let mut ranked = Vec::new();
        for name in names {
            let value = format!("keep one queue ready for {name} workers");
            ranked.push((slot_canonical(&gm, name, &value).await, value));
        }
        ranked.sort();
        let kept: Vec<_> = ranked
            .iter()
            .take(4)
            .map(|(_, value)| value.clone())
            .collect();
        let excluded = ranked[4].1.clone();
        let quote = "queue is retired from the dock";
        let mut rejection = uses("harbor", "queue", quote);
        rejection.status = "rejected".into();
        let turns = [turn(quote)];
        let stats = commit_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            std::slice::from_ref(&rejection),
        )
        .await;
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unsure","quote":""}]}"#.into(),
        ));
        let report =
            pass_linked(&gm, "s2", "2026-04-11", &turns, &[rejection], &stats, &stub).await;
        assert_eq!(report.links.d_anchors_added, 4);
        assert_eq!(report.links.d_anchor_overflow, 1);
        assert!(report.consolidate.pairs <= 6, "{:?}", report.consolidate);
        assert!(report.consolidate.calls <= 1);
        assert_eq!(stub.calls(), 1);
        let message = stub.message();
        for value in &kept {
            assert!(
                message.contains(&derived_marker(value)),
                "{value} missing in {message}"
            );
        }
        assert!(
            !message.contains(&derived_marker(&excluded)),
            "{excluded} was not dropped: {message}"
        );
        for name in names {
            let rows = slots(&gm, name).await;
            assert!(ended(slot_of(
                &rows,
                &format!("keep one queue ready for {name} workers")
            ))
            .is_none());
        }
    }

    #[tokio::test]
    async fn the_earliest_writer_close_supplies_turn_and_quote() {
        let (_tmp, gm) = open().await;
        let queue = uses("harbor", "queue", "harbor uses queue");
        let spool = uses("harbor", "spool", "harbor uses spool");
        let dependent = rule(
            "birch",
            Kind::Project,
            "keep one queue and one spool ready for the workers",
            "keep one queue and one spool ready for the workers",
        );
        let partner = rule(
            "birch",
            Kind::Project,
            "keep one latch and one valve ready for the workers",
            "keep one latch and one valve ready for the workers",
        );
        commit_linked(
            &gm,
            "s1",
            "2026-04-03",
            &[turn("harbor uses queue and spool")],
            &[queue, spool, dependent, partner],
        )
        .await;
        let first = "spool retired from the dock today";
        let second = "queue retired from the dock today";
        let mut drop_spool = uses("harbor", "spool", first);
        drop_spool.status = "rejected".into();
        drop_spool.turn = 0;
        let mut drop_queue = uses("harbor", "queue", second);
        drop_queue.status = "rejected".into();
        drop_queue.turn = 1;
        let turns = [
            turn(first),
            Turn {
                index: 1,
                role: TurnRole::User,
                text: second.into(),
            },
        ];
        let stats = commit_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            &[drop_spool.clone(), drop_queue.clone()],
        )
        .await;
        assert_eq!(stats.close_events.len(), 2, "{:?}", stats.close_events);
        let stub = Stub::new(Answer::Reply(
            r#"{"pairs":[{"i":1,"verdict":"unsure","quote":""}]}"#.into(),
        ));
        let report = pass_linked(
            &gm,
            "s2",
            "2026-04-11",
            &turns,
            &[drop_spool, drop_queue],
            &stats,
            &stub,
        )
        .await;
        assert_eq!(report.links.d_anchors_added, 1);
        let message = stub.message();
        let derived = message
            .lines()
            .find(|line| line.contains("reason: derived"))
            .unwrap_or_else(|| panic!("{message}"));
        assert!(derived.contains(first), "{derived}");
        assert!(!derived.contains(second), "{derived}");
        let rows = slots(&gm, "birch").await;
        assert!(ended(slot_of(
            &rows,
            "keep one queue and one spool ready for the workers"
        ))
        .is_none());
    }

    #[tokio::test]
    async fn hold_confirmation_still_runs_when_links_are_on() {
        let (_tmp, gm) = open().await;
        seed_binder(&gm).await;
        let linked = rule(
            "birch",
            Kind::Project,
            "keep the canvas cover on the binder",
            "keep the canvas cover on the binder",
        );
        commit_linked(
            &gm,
            "s1b",
            "2026-04-01",
            &[turn("keep the canvas cover on the binder")],
            &[linked],
        )
        .await;
        let open_turns = [turn("Store the pages in linen.")];
        let open_facts = [setting(
            "acme",
            Kind::Project,
            "binder",
            "linen",
            "Store the pages in linen.",
        )];
        let identity = project();
        let mut ctx = WriteCtx::new(&Quiet, &identity, "s2", "2026-04-02", &open_turns);
        ctx.match_on = true;
        ctx.hold_on = true;
        ctx.links_on = true;
        write_facts(&gm, &ctx, &open_facts).await.unwrap();
        let quote = "Moved off canvas yesterday.";
        let turns = [
            turn("The label is north."),
            Turn {
                index: 1,
                role: TurnRole::User,
                text: quote.into(),
            },
        ];
        let identity = project();
        let mut ctx = WriteCtx::new(&Quiet, &identity, "s3", "2026-04-03", &turns);
        ctx.match_on = true;
        ctx.hold_on = true;
        ctx.links_on = true;
        let stats = write_facts(&gm, &ctx, &[]).await.unwrap();
        let llm = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let mut pass_ctx = WriteCtx::new(&llm, &identity, "s3", "2026-04-03", &turns);
        pass_ctx.hold_on = true;
        pass_ctx.links_on = true;
        let mut report = IngestionReport {
            links: stats.links,
            ..IngestionReport::default()
        };
        run(&gm, &pass_ctx, &[], &stats, &llm, &mut report).await;
        assert_eq!(report.hold.holds_confirmed, 1, "{:?}", report.hold);
        assert_eq!(report.links.d_anchors_added, 0);
        let rows = slots(&gm, "acme").await;
        assert_eq!(
            ended(slot_named(&rows, "binder", "canvas")),
            Some("s3:consolidated")
        );
        assert!(slot_named(&rows, "binder", "linen")
            .get("pending")
            .is_none());
        let birch = slots(&gm, "birch").await;
        assert!(ended(slot_of(&birch, "keep the canvas cover on the binder")).is_none());
    }

    fn slot_of<'a>(rows: &'a [Value], value: &str) -> &'a Value {
        rows.iter()
            .find(|slot| slot["value"] == value)
            .unwrap_or_else(|| panic!("missing {value} in {rows:?}"))
    }
    struct PassFlags {
        links_on: bool,
        components_on: bool,
        hold_on: bool,
    }

    async fn commit_mode(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        links_on: bool,
    ) -> WriteStats {
        let project = project();
        let mut ctx = WriteCtx::new(&Quiet, &project, session, date, turns);
        ctx.links_on = links_on;
        write_facts(gm, &ctx, facts).await.unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn pass_switches(
        gm: &GraphMemory,
        session: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        stats: &WriteStats,
        llm: &Stub,
        flags: PassFlags,
    ) -> IngestionReport {
        let project = project();
        let mut ctx = WriteCtx::new(llm, &project, session, date, turns);
        ctx.links_on = flags.links_on;
        ctx.components_on = flags.components_on;
        ctx.hold_on = flags.hold_on;
        let mut report = IngestionReport {
            links: stats.links,
            ..IngestionReport::default()
        };
        run(gm, &ctx, facts, stats, llm, &mut report).await;
        report
    }

    fn flags(links_on: bool, components_on: bool) -> PassFlags {
        PassFlags {
            links_on,
            components_on,
            hold_on: false,
        }
    }

    struct Judged {
        report: IngestionReport,
        message: String,
        calls: u32,
        ended: Option<String>,
    }

    async fn sibling_port(components_on: bool) -> (Judged, Option<String>) {
        sibling_verdict(components_on, "supersedes").await
    }

    async fn sibling_verdict(components_on: bool, verdict: &str) -> (Judged, Option<String>) {
        let (_tmp, gm) = open().await;
        let widget = edge(
            Predicate::PartOf,
            "widget",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let serve = edge(
            Predicate::PartOf,
            "acme widget",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let old = setting("widget", Kind::Component, "port", "8080", "port is 8080");
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("widget is part of acme. port is 8080")],
            &[widget, serve, old],
        )
        .await;
        let quote = "Default port is now 8765.";
        let next = setting("acme widget", Kind::Component, "port", "8765", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, verdict, quote)])));
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next],
            &stats,
            &stub,
            flags(false, components_on),
        )
        .await;
        let ended =
            ended(live_any(&slots(&gm, "widget").await, "port", "8080")).map(str::to_string);
        (
            Judged {
                report,
                message: stub.message(),
                calls: stub.calls(),
                ended: None,
            },
            ended,
        )
    }

    #[tokio::test]
    async fn a_sibling_under_the_project_closes_when_components_are_on() {
        let off = sibling_port(false).await;
        let on = sibling_port(true).await;
        assert_eq!(off.0.calls, 1);
        assert_eq!(on.0.calls, off.0.calls);
        assert_eq!(on.0.message, off.0.message);
        assert_eq!(dropped(&off.0.report, "no departure cue"), 1);
        assert_eq!(off.0.report.consolidate.closes_sup, 0);
        assert!(off.1.is_none());
        assert!(off.0.report.consolidate.link_sibling.is_none());
        assert!(off.0.report.consolidate.link_derived.is_none());
        assert!(off.0.report.consolidate.closes_component.is_none());
        let omitted = serde_json::to_value(&off.0.report.consolidate).unwrap();
        assert!(omitted.get("link_sibling").is_none());
        assert_eq!(on.1.as_deref(), Some("s2:consolidated"));
        assert_eq!(on.0.report.consolidate.closes_sup, 1);
        assert_eq!(on.0.report.consolidate.link_sibling, Some(1));
        assert_eq!(on.0.report.consolidate.link_derived, Some(0));
        assert_eq!(on.0.report.consolidate.closes_component, Some(1));
    }

    async fn derived_storage(links_on: bool, components_on: bool, ambiguous: bool) -> Judged {
        let (_tmp, gm) = open().await;
        let project_row = setting(
            "acme",
            Kind::Project,
            "storage_path",
            "walnut marker",
            "storage_path is walnut marker",
        );
        let copy = setting(
            "store",
            Kind::Component,
            "storage_path",
            "walnut marker",
            "store storage_path is walnut marker",
        );
        commit_mode(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("storage_path is walnut marker")],
            &[project_row, copy],
            true,
        )
        .await;
        let quote = "Storage is now sqlite index for the rows.";
        let next = setting("acme", Kind::Project, "storage_path", "sqlite index", quote);
        let label = setting(
            "store",
            Kind::Component,
            "label",
            "blue ribbon",
            "The store label is blue ribbon.",
        );
        let turns = [turn(
            "Storage is now sqlite index for the rows. The store label is blue ribbon.",
        )];
        let mut stats = commit_mode(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next.clone(), label.clone()],
            true,
        )
        .await;
        assert_eq!(stats.close_events.len(), 1, "{:?}", stats.close_events);
        let closed = stats.close_events[0].row_id.clone();
        let stored = slots(&gm, "store").await;
        let derived = stored
            .iter()
            .find(|slot| slot["value"] == "walnut marker")
            .expect("stored copy");
        let ids = derived["derived_from"].as_array().expect("derived_from");
        assert!(
            ids.iter().any(|id| id.as_str() == Some(closed.as_str())),
            "{derived}"
        );
        if ambiguous {
            stats.close_events.push(super::super::write::CloseEvent {
                row_id: format!("{closed}-other"),
                reason: stats.close_events[0].reason.clone(),
                fact_index: stats.close_events[0].fact_index,
            });
        }
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next, label],
            &stats,
            &stub,
            flags(links_on, components_on),
        )
        .await;
        let ended = ended(live_any(
            &slots(&gm, "store").await,
            "storage_path",
            "walnut marker",
        ))
        .map(str::to_string);
        Judged {
            report,
            message: stub.message(),
            calls: stub.calls(),
            ended,
        }
    }

    #[tokio::test]
    async fn a_derived_from_partner_closes_only_when_links_and_components_are_on() {
        let off = derived_storage(true, false, false).await;
        let on = derived_storage(true, true, false).await;
        let unlinked = derived_storage(false, true, false).await;
        assert_eq!(off.calls, 1);
        assert_eq!(on.calls, off.calls);
        assert_eq!(on.message, off.message);
        assert_eq!(unlinked.calls, 1);
        assert_eq!(dropped(&off.report, "no departure cue"), 1);
        assert_eq!(off.report.consolidate.closes_sup, 0);
        assert!(off.report.consolidate.link_derived.is_none());
        assert_eq!(dropped(&unlinked.report, "no departure cue"), 1);
        assert_eq!(unlinked.report.consolidate.closes_sup, 0);
        assert_eq!(unlinked.report.consolidate.link_derived, Some(0));
        assert_eq!(on.report.consolidate.closes_sup, 1);
        assert_eq!(on.ended.as_deref(), Some("s2:consolidated"));
        assert_eq!(off.ended, None);
        assert_eq!(unlinked.ended, None);
        assert_eq!(on.report.consolidate.link_derived, Some(1));
        assert_eq!(on.report.consolidate.link_sibling, Some(0));
        assert_eq!(on.report.consolidate.closes_component, Some(1));
    }

    #[tokio::test]
    async fn an_ambiguous_close_event_makes_no_derived_link() {
        let judged = derived_storage(true, true, true).await;
        assert_eq!(judged.calls, 1);
        assert_eq!(dropped(&judged.report, "no departure cue"), 1);
        assert_eq!(judged.report.consolidate.closes_sup, 0);
        assert_eq!(judged.report.consolidate.link_derived, Some(0));
    }

    async fn loose_parent(kind: &str) -> Judged {
        let (_tmp, gm) = open().await;
        let parent = if kind == "panel" { "panel" } else { "acme" };
        let parent_kind = if kind == "panel" {
            Kind::Component
        } else {
            Kind::Project
        };
        let widget = edge(
            Predicate::PartOf,
            "widget",
            Kind::Component,
            parent,
            parent_kind,
        );
        let serve = edge(
            Predicate::PartOf,
            "acme widget",
            Kind::Component,
            parent,
            parent_kind,
        );
        let old = setting("widget", Kind::Component, "port", "8080", "port is 8080");
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("widget links its parent. port is 8080")],
            &[widget, serve, old],
        )
        .await;
        match kind {
            "considered" => set_part_of(&gm, "considered", None).await,
            "rejected" => set_part_of(&gm, "rejected", None).await,
            "closed" => set_part_of(&gm, "decided", Some("2026-03-01T00:00:00Z")).await,
            _ => {}
        }
        let quote = "Default port is now 8765.";
        let next = setting("acme widget", Kind::Component, "port", "8765", quote);
        let label = setting(
            "widget",
            Kind::Component,
            "label",
            "blue ribbon",
            "The widget label is blue ribbon.",
        );
        let turns = [turn(
            "Default port is now 8765. The widget label is blue ribbon.",
        )];
        let stats = commit(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next.clone(), label.clone()],
        )
        .await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next, label],
            &stats,
            &stub,
            flags(false, true),
        )
        .await;
        let ended =
            ended(live_any(&slots(&gm, "widget").await, "port", "8080")).map(str::to_string);
        Judged {
            report,
            message: stub.message(),
            calls: stub.calls(),
            ended,
        }
    }

    async fn set_part_of(gm: &GraphMemory, status: &str, until: Option<&str>) {
        if let Some(until) = until {
            gm.db()
                .query(
                    "UPDATE relates_to SET status = $status, valid_until = type::datetime($until) WHERE rel_type = 'part_of'",
                )
                .bind(("status", status.to_string()))
                .bind(("until", until.to_string()))
                .await
                .unwrap()
                .check()
                .unwrap();
            return;
        }
        gm.db()
            .query("UPDATE relates_to SET status = $status WHERE rel_type = 'part_of'")
            .bind(("status", status.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    #[tokio::test]
    async fn a_non_project_or_inactive_parent_is_not_a_sibling() {
        for kind in ["panel", "considered", "rejected", "closed"] {
            let judged = loose_parent(kind).await;
            assert_eq!(judged.calls, 1, "{kind} {}", judged.message);
            assert_eq!(dropped(&judged.report, "no departure cue"), 1, "{kind}");
            assert_eq!(judged.report.consolidate.closes_sup, 0, "{kind}");
            assert_eq!(judged.ended, None, "{kind}");
            assert_eq!(judged.report.consolidate.link_sibling, Some(0), "{kind}");
        }
    }

    #[tokio::test]
    async fn two_sibling_supersedes_on_one_anchor_close_none() {
        let (_tmp, gm) = open().await;
        let mut facts = Vec::new();
        for (name, value) in [("widget", "8080"), ("panel", "9090")] {
            facts.push(edge(
                Predicate::PartOf,
                name,
                Kind::Component,
                "acme",
                Kind::Project,
            ));
            facts.push(setting(
                name,
                Kind::Component,
                "port",
                value,
                &format!("port is {value}"),
            ));
        }
        facts.push(edge(
            Predicate::PartOf,
            "acme widget",
            Kind::Component,
            "acme",
            Kind::Project,
        ));
        commit(&gm, "s1", "2026-03-01", &[turn("ports stay")], &facts).await;
        let turn_text = "Default port is now 8765. The port is now 8765.";
        let next = setting("acme widget", Kind::Component, "port", "8765", turn_text);
        let turns = [turn(turn_text)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let body = judged(&[
            (1, "supersedes", "Default port is now 8765."),
            (2, "supersedes", "The port is now 8765."),
        ]);
        let stub = Stub::new(Answer::Reply(body));
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next],
            &stats,
            &stub,
            flags(false, true),
        )
        .await;
        assert_eq!(stub.calls(), 1, "{}", stub.message());
        assert_eq!(report.consolidate.closes_sup, 0);
        assert_eq!(report.consolidate.link_sibling, Some(0));
        assert_eq!(report.consolidate.quote_trailing, Some(0));
        assert_eq!(dropped(&report, "second supersede on the same anchor"), 2);
        assert!(
            report
                .consolidate
                .skips
                .get("consolidate_ambiguous")
                .copied()
                .unwrap_or(0)
                >= 2
        );
        assert!(ended(live_any(&slots(&gm, "widget").await, "port", "8080")).is_none());
        assert!(ended(live_any(&slots(&gm, "panel").await, "port", "9090")).is_none());
    }

    #[tokio::test]
    async fn a_non_supersede_leaves_component_counters_at_zero() {
        for verdict in ["unrelated", "duplicate", "unsure"] {
            let (judged, ended) = sibling_verdict(true, verdict).await;
            assert_eq!(judged.report.consolidate.closes_sup, 0, "{verdict}");
            assert_eq!(judged.report.consolidate.link_sibling, Some(0), "{verdict}");
            assert_eq!(judged.report.consolidate.link_derived, Some(0), "{verdict}");
            assert_eq!(
                judged.report.consolidate.quote_trailing,
                Some(0),
                "{verdict}"
            );
            assert!(ended.is_none(), "{verdict}");
        }
    }

    #[tokio::test]
    async fn a_pending_sibling_is_skipped_and_arms_no_hold() {
        let (_tmp, gm) = open().await;
        let widget = edge(
            Predicate::PartOf,
            "widget",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let serve = edge(
            Predicate::PartOf,
            "acme widget",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let old = setting("widget", Kind::Component, "port", "8080", "port is 8080");
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("widget is part of acme")],
            &[widget, serve, old],
        )
        .await;
        mark_pending(&gm, "widget", "port", "8080").await;
        let quote = "Default port is now 8765.";
        let next = setting("acme widget", Kind::Component, "port", "8765", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Panic);
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next],
            &stats,
            &stub,
            PassFlags {
                links_on: false,
                components_on: true,
                hold_on: true,
            },
        )
        .await;
        assert_eq!(stub.calls(), 0);
        assert!(report.hold.is_blank(), "{:?}", report.hold);
        assert_eq!(report.consolidate.closes_sup, 0);
        assert_eq!(report.consolidate.link_sibling, Some(0));
        assert!(ended(live_any(&slots(&gm, "widget").await, "port", "8080")).is_none());
    }

    async fn mark_pending(gm: &GraphMemory, name: &str, key: &str, value: &str) {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let entity = entities.iter().find(|entity| entity.name == name).unwrap();
        let mut attributes = entity.attributes.clone().unwrap();
        let slots = attributes["v2"]["slots"].as_array_mut().unwrap();
        let slot = slots
            .iter_mut()
            .find(|slot| slot["key"] == key && slot["value"] == value)
            .unwrap();
        slot["pending"] = json!({"since": "s2", "conflicts": "entity:none#0"});
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", entity.id_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    #[tokio::test]
    async fn a_partner_outside_the_neighbourhood_is_not_paired_with_components_on() {
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
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next],
            &stats,
            &stub,
            flags(false, true),
        )
        .await;
        let message = stub.message();
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

    fn anchor_named(subject: &str, key: &str, new_value: &str) -> Anchor {
        Anchor {
            id: "a3:0".into(),
            subject: subject.into(),
            pred: "setting".into(),
            key: key.into(),
            value: String::new(),
            text: String::new(),
            new_value: new_value.into(),
            replaces: String::new(),
            entity: String::new(),
            reason: "new",
            turn: 0,
            quote: String::new(),
            live: None,
            a2: false,
            closed_row: None,
        }
    }

    fn row_named(subject: &str, entity: &str, key: &str, value: &str) -> Row {
        Row {
            id: format!("s:{entity}:0"),
            entity: entity.into(),
            slot: Some(0),
            edge: None,
            subject: subject.into(),
            pred: "setting".into(),
            key: key.into(),
            value: value.into(),
            evidence: Value::Null,
            from: String::new(),
            here: false,
            pending: false,
        }
    }

    #[test]
    fn sibling_link_needs_two_children_of_the_session_project() {
        let owners = [
            ("p", "acme"),
            ("w", "widget"),
            ("s", "acme widget"),
            ("n", "panel"),
            ("a", "acme labs"),
        ]
        .into_iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect::<Vec<_>>();
        let anchor = anchor_named("acme widget", "port", "8765");
        let partner = row_named("widget", "w", "port", "8080");
        let session = project();
        let rows: &[Row] = &[];
        let live = vec![("w".into(), "p".into()), ("s".into(), "p".into())];
        assert!(sibling_link(
            &anchor, &partner, rows, &live, &owners, &session
        ));
        let panel = vec![("w".into(), "n".into()), ("s".into(), "n".into())];
        assert!(!sibling_link(
            &anchor, &partner, rows, &panel, &owners, &session
        ));
        let one = vec![("w".into(), "p".into())];
        assert!(!sibling_link(
            &anchor, &partner, rows, &one, &owners, &session
        ));
        let same = row_named("acme widget", "s", "port", "8080");
        assert!(!sibling_link(
            &anchor, &same, rows, &live, &owners, &session
        ));
        let mut aliased = project();
        aliased.aliases = vec!["acme labs".into()];
        let via_alias = vec![("w".into(), "a".into()), ("s".into(), "a".into())];
        assert!(sibling_link(
            &anchor, &partner, rows, &via_alias, &owners, &aliased
        ));
        let mut doubled = owners.clone();
        doubled.push(("w2".into(), "widget".into()));
        assert!(unique_owner("widget", &doubled).is_none());
    }

    #[test]
    fn a_colliding_name_uses_the_partner_entity_not_the_first_owner() {
        let owners = [
            ("acme", "acme"),
            ("acme-widget", "widget"),
            ("acme-client", "http client"),
            ("other", "other"),
            ("other-widget", "widget"),
            ("other-client", "http client"),
        ]
        .into_iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect::<Vec<_>>();
        let part_of = [
            ("acme-widget", "acme"),
            ("acme-client", "acme"),
            ("other-widget", "other"),
            ("other-client", "other-widget"),
        ]
        .into_iter()
        .map(|(from, to)| (from.to_string(), to.to_string()))
        .collect::<Vec<_>>();
        assert_eq!(owner_id("widget", &owners), Some("acme-widget"));
        assert_eq!(owner_id("http client", &owners), Some("acme-client"));
        let mut anchor = anchor_named("widget", "port", "8765");
        anchor.live = Some(0);
        let rows = [row_named("widget", "acme-widget", "port", "8765")];
        let partner = row_named("http client", "other-client", "port", "8080");
        assert!(!sibling_link(
            &anchor,
            &partner,
            &rows,
            &part_of,
            &owners,
            &project()
        ));
    }

    #[tokio::test]
    async fn a_same_named_component_in_another_project_is_not_closed() {
        let (_tmp, gm) = open().await;
        let widget = edge(
            Predicate::PartOf,
            "widget",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let client = edge(
            Predicate::PartOf,
            "http client",
            Kind::Component,
            "acme",
            Kind::Project,
        );
        let other = setting("other", Kind::Project, "title", "host", "other is host");
        commit(
            &gm,
            "s1",
            "2026-03-01",
            &[turn("stored")],
            &[widget, client, other],
        )
        .await;
        let acme_widget = id_named(&gm, "widget").await;
        let other_widget = format!("{acme_widget}z");
        let other_client = format!("{}z", id_named(&gm, "http client").await);
        let other_project = id_named(&gm, "other").await;
        create_named(&gm, &other_widget, "widget", empty_component()).await;
        create_named(&gm, &other_client, "http client", port_slot()).await;
        relate_part_of(&gm, &other_widget, &other_project).await;
        relate_part_of(&gm, &other_client, &other_widget).await;
        let quote = "Default port is now 8765.";
        let next = setting("widget", Kind::Component, "port", "8765", quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next],
            &stats,
            &stub,
            flags(false, true),
        )
        .await;
        assert_eq!(stub.calls(), 1, "{}", stub.message());
        assert!(stub.message().contains("8080"), "{}", stub.message());
        assert_eq!(report.consolidate.closes_sup, 0);
        assert_eq!(report.consolidate.link_sibling, Some(0));
        let stored = attributes_of(&gm, &other_client).await;
        let slot = &stored["v2"]["slots"][0];
        assert_eq!(slot["value"], "8080");
        assert!(slot.get("ended_by").is_none(), "{slot}");
    }

    async fn id_named(gm: &GraphMemory, name: &str) -> String {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        entities
            .iter()
            .find(|entity| entity.name == name)
            .unwrap()
            .id_string()
    }

    async fn create_named(gm: &GraphMemory, id: &str, name: &str, attributes: Value) {
        gm.db()
            .query(
                "CREATE type::record($id) SET name = $name, entity_type = 'concept', abstract = 'host', overview = '', attributes = $attributes, mutable = true",
            )
            .bind(("id", id.to_string()))
            .bind(("name", name.to_string()))
            .bind(("attributes", attributes))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    async fn relate_part_of(gm: &GraphMemory, from: &str, to: &str) {
        gm.db()
            .query(format!(
                "RELATE {from}->relates_to->{to} SET rel_type = 'part_of', status = 'decided', valid_from = d'2026-03-01T00:00:00Z'"
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    async fn attributes_of(gm: &GraphMemory, id: &str) -> Value {
        let mut response = gm
            .db()
            .query("SELECT attributes FROM type::record($id)")
            .bind(("id", id.to_string()))
            .await
            .unwrap();
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        rows[0]["attributes"].clone()
    }

    fn empty_component() -> Value {
        json!({"v2": {"kind": "component", "slots": []}})
    }

    fn port_slot() -> Value {
        json!({
            "v2": {
                "kind": "component",
                "slots": [{
                    "predicate": "setting",
                    "key": "port",
                    "value": "8080",
                    "status": "decided",
                    "valid_from": "2026-03-01T00:00:00Z",
                    "evidence": [{"session": "s0", "turn": 0, "quote": "port is 8080", "role": "user"}]
                }]
            }
        })
    }

    #[test]
    fn derived_link_needs_links_and_the_closed_row() {
        let mut anchor = anchor_named("acme", "storage_path", "sqlite index");
        anchor.closed_row = Some("entity:acme#0".into());
        let partner = row_named("store", "entity:store", "storage_path", "walnut marker");
        let hit = DependentHit {
            entity_id: "entity:store".into(),
            canonical: "entity:store#0".into(),
            derived_from: vec!["entity:acme#0".into(), "relates_to:other".into()],
        };
        let hits = std::slice::from_ref(&hit);
        assert!(derived_link(&anchor, &partner, hits, true));
        assert!(!derived_link(&anchor, &partner, hits, false));
        anchor.closed_row = None;
        assert!(!derived_link(&anchor, &partner, hits, true));
    }

    #[test]
    fn component_close_line_tags_the_pair() {
        assert_eq!(
            component_close_line(
                "sibling",
                "widget:port",
                "acme widget:port",
                "Default port is now 8765."
            ),
            "consolidate close: component sibling | widget:port | acme widget:port | Default port is now 8765."
        );
        assert_eq!(close_tag(&CueTally::none(), true), "same subject");
        assert_eq!(close_tag(&CueTally::none(), false), "part_of");
        assert_eq!(
            close_tag(
                &CueTally {
                    sibling: false,
                    derived: true,
                    trailing: false,
                },
                false
            ),
            "derived"
        );
        assert_eq!(
            close_tag(
                &CueTally {
                    sibling: true,
                    derived: false,
                    trailing: true,
                },
                false
            ),
            "trailing"
        );
    }

    #[test]
    fn two_close_events_for_one_fact_are_not_a_link() {
        use super::super::write::CloseEvent;

        let fact = setting(
            "acme",
            Kind::Project,
            "storage_path",
            "sqlite index",
            "Storage is now sqlite index for the rows.",
        );
        let retirement = crate::graph::types::Retirement {
            reason: "superseded".into(),
            subject: "acme".into(),
            predicate: "setting".into(),
            key: "storage_path".into(),
            old_value: "walnut marker".into(),
            new_value: "sqlite index".into(),
        };
        let stats = WriteStats {
            retirements: vec![retirement.clone()],
            close_events: vec![CloseEvent {
                row_id: "entity:acme#0".into(),
                reason: "superseded".into(),
                fact_index: 0,
            }],
            ..WriteStats::default()
        };
        assert_eq!(
            unique_closed_row(&retirement, std::slice::from_ref(&fact), &stats).as_deref(),
            Some("entity:acme#0")
        );
        let mut ambiguous = stats.clone();
        ambiguous.close_events.push(CloseEvent {
            row_id: "entity:acme#1".into(),
            reason: "superseded".into(),
            fact_index: 0,
        });
        assert!(unique_closed_row(&retirement, &[fact], &ambiguous).is_none());
    }

    #[test]
    fn two_retirements_for_one_fact_are_not_a_link() {
        use super::super::write::CloseEvent;

        let fact = setting(
            "acme",
            Kind::Project,
            "storage_path",
            "sqlite index",
            "Storage is now sqlite index for the rows.",
        );
        let first = crate::graph::types::Retirement {
            reason: "superseded".into(),
            subject: "acme".into(),
            predicate: "setting".into(),
            key: "storage_path".into(),
            old_value: "walnut marker".into(),
            new_value: "sqlite index".into(),
        };
        let second = crate::graph::types::Retirement {
            reason: "superseded".into(),
            subject: "acme".into(),
            predicate: "setting".into(),
            key: "archive_path".into(),
            old_value: "cedar marker".into(),
            new_value: "sqlite index".into(),
        };
        let stats = WriteStats {
            retirements: vec![first.clone(), second],
            close_events: vec![CloseEvent {
                row_id: "entity:acme#0".into(),
                reason: "superseded".into(),
                fact_index: 0,
            }],
            ..WriteStats::default()
        };
        assert!(unique_closed_row(&first, std::slice::from_ref(&fact), &stats).is_none());
    }

    #[allow(clippy::too_many_arguments)]
    async fn quote_case(
        anchor_key: &str,
        anchor_value: &str,
        partner_subject: &str,
        partner_key: &str,
        partner_value: &str,
        quote: &str,
        components_on: bool,
    ) -> Judged {
        let (_tmp, gm) = open().await;
        let kind = if partner_subject == "acme" {
            Kind::Project
        } else {
            Kind::Component
        };
        let mut seeded = Vec::new();
        if partner_subject != "acme" {
            seeded.push(edge(
                Predicate::PartOf,
                partner_subject,
                Kind::Component,
                "acme",
                Kind::Project,
            ));
        }
        seeded.push(setting(
            partner_subject,
            kind,
            partner_key,
            partner_value,
            &format!("{partner_key} is {partner_value}"),
        ));
        commit(&gm, "s1", "2026-03-01", &[turn("stored")], &seeded).await;
        let next = setting("acme", Kind::Project, anchor_key, anchor_value, quote);
        let turns = [turn(quote)];
        let stats = commit(&gm, "s2", "2026-03-02", &turns, std::slice::from_ref(&next)).await;
        let stub = Stub::new(Answer::Reply(judged(&[(1, "supersedes", quote)])));
        let report = pass_switches(
            &gm,
            "s2",
            "2026-03-02",
            &turns,
            &[next],
            &stats,
            &stub,
            flags(false, components_on),
        )
        .await;
        let rows = slots(&gm, partner_subject).await;
        let ended = ended(live_any(&rows, partner_key, partner_value)).map(str::to_string);
        Judged {
            report,
            message: stub.message(),
            calls: stub.calls(),
            ended,
        }
    }

    #[tokio::test]
    async fn a_linked_same_slot_number_closes_on_a_trailing_quote() {
        let quote = "Outbound timeout is 15 seconds now.";
        let off = quote_case(
            "outbound_timeout_seconds",
            "15",
            "outbound client",
            "timeout_seconds",
            "10",
            quote,
            false,
        )
        .await;
        let on = quote_case(
            "outbound_timeout_seconds",
            "15",
            "outbound client",
            "timeout_seconds",
            "10",
            quote,
            true,
        )
        .await;
        assert_eq!(off.calls, 1);
        assert_eq!(on.calls, off.calls);
        assert_eq!(on.message, off.message);
        assert_eq!(dropped(&off.report, "no departure cue"), 1);
        assert_eq!(off.report.consolidate.closes_sup, 0);
        assert!(off.ended.is_none());
        assert!(off.report.consolidate.quote_trailing.is_none());
        assert_eq!(on.ended.as_deref(), Some("s2:consolidated"));
        assert_eq!(on.report.consolidate.closes_sup, 1);
        assert_eq!(on.report.consolidate.quote_trailing, Some(1));
        assert_eq!(on.report.consolidate.link_sibling, Some(0));
        assert_eq!(on.report.consolidate.closes_component, Some(1));
    }

    #[tokio::test]
    async fn coverage_is_not_a_trailing_close() {
        for (value, quote) in [
            ("88%", "Coverage is 88% now."),
            ("88", "Coverage is 88 now."),
        ] {
            let judged = quote_case(
                "coverage",
                value,
                "acme part",
                "coverage_min",
                "85",
                quote,
                true,
            )
            .await;
            assert_eq!(judged.report.consolidate.closes_sup, 0, "{quote}");
            assert!(judged.ended.is_none(), "{quote}");
            assert_ne!(judged.report.consolidate.quote_trailing, Some(1), "{quote}");
            if value == "88" {
                assert_eq!(judged.calls, 1, "{quote}");
                assert_eq!(dropped(&judged.report, "no departure cue"), 1, "{quote}");
            }
        }
    }

    #[tokio::test]
    async fn a_unit_only_key_overlap_is_not_a_trailing_close() {
        let quote = "The shutdown wait is 15 seconds now.";
        let judged = quote_case(
            "shutdown_wait_seconds",
            "15",
            "outbound client",
            "timeout_seconds",
            "10",
            quote,
            true,
        )
        .await;
        assert_eq!(judged.calls, 1, "{}", judged.message);
        assert_eq!(dropped(&judged.report, "no departure cue"), 1);
        assert_eq!(judged.report.consolidate.closes_sup, 0);
        assert_eq!(judged.report.consolidate.quote_trailing, Some(0));
        assert!(judged.ended.is_none());
    }

    #[tokio::test]
    async fn trailing_hedged_or_off_key_phrasing_stays_dropped() {
        for quote in [
            "Maybe the outbound timeout is 15 seconds now.",
            "The outbound timeout is 15 seconds now, really.",
            "The outbound timeout is also 15 seconds now.",
            "The outbound timeout is 15 seconds for now.",
            "The outbound timeout is 15 seconds now?",
            "The picker timeout is 15 seconds now.",
            "They said the outbound timeout is 15 seconds now.",
            "The outbound timeout is 15 seconds now from 10.",
            "The outbound timeout is 15 or 20 seconds now.",
        ] {
            let judged = quote_case(
                "outbound_timeout_seconds",
                "15",
                "outbound client",
                "timeout_seconds",
                "10",
                quote,
                true,
            )
            .await;
            assert_eq!(judged.report.consolidate.closes_sup, 0, "{quote}");
            assert!(judged.ended.is_none(), "{quote}");
            assert_eq!(judged.report.consolidate.quote_trailing, Some(0), "{quote}");
        }
    }

    #[tokio::test]
    async fn a_same_subject_pair_gets_no_trailing_waiver() {
        let quote = "The timeout is 15 seconds now.";
        let off = quote_case(
            "timeout_seconds",
            "15",
            "acme",
            "timeout",
            "10",
            quote,
            false,
        )
        .await;
        let on = quote_case(
            "timeout_seconds",
            "15",
            "acme",
            "timeout",
            "10",
            quote,
            true,
        )
        .await;
        assert_eq!(off.calls, 1);
        assert_eq!(on.calls, off.calls);
        assert_eq!(on.message, off.message);
        assert_eq!(dropped(&off.report, "no departure cue"), 1);
        assert_eq!(dropped(&on.report, "no departure cue"), 1);
        assert_eq!(on.report.consolidate.closes_sup, 0);
        assert_eq!(off.report.consolidate.closes_sup, 0);
        assert_eq!(on.report.consolidate.quote_trailing, Some(0));
        assert!(on.ended.is_none());
        assert!(off.ended.is_none());
    }

    #[test]
    fn same_slot_ignores_units_and_keeps_a_minimum() {
        let folded = anchor_named("acme", "outbound_timeout_seconds", "15");
        let client = row_named("outbound client", "c", "timeout_seconds", "10");
        assert!(same_slot(&folded, &client));
        let coverage = anchor_named("acme", "coverage", "88");
        let minimum = row_named("acme part", "p", "coverage_min", "85");
        assert!(!same_slot(&coverage, &minimum));
        let wait = anchor_named("acme", "shutdown_wait_seconds", "15");
        let timeout = row_named("outbound client", "c", "timeout_seconds", "10");
        assert!(!same_slot(&wait, &timeout));
        assert!(numeric_pair(&coverage, &minimum));
        let mut percent = coverage;
        percent.new_value = "88%".into();
        assert!(!numeric_pair(&percent, &minimum));
        assert!(trailing_quote(
            "Outbound timeout is 15 seconds now.",
            "10",
            "15",
            "outbound_timeout_seconds",
        ));
        assert!(trailing_quote(
            "Outbound timeout is 15 seconds now\nRetries stay at three.",
            "10",
            "15",
            "outbound_timeout_seconds",
        ));
        assert!(trailing_quote(
            "Coverage is 88 now.",
            "85",
            "88",
            "coverage"
        ));
        assert!(!trailing_quote(
            "Maybe the outbound timeout is 15 seconds now.",
            "10",
            "15",
            "outbound_timeout_seconds",
        ));
    }
}

#[cfg(test)]
mod linked_hit_tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{
        collect_edge_hits, collect_slot_hits, edge_dependent, eligible_edge, mentions_closed,
    };

    fn closed(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn eligible_edge_rejects_closed_pending_and_undecided() {
        assert!(!eligible_edge(&json!({
            "status": "decided",
            "valid_until": "2026-04-11T00:00:00Z"
        })));
        assert!(!eligible_edge(&json!({
            "status": "decided",
            "pending": {"since": "s2", "conflicts": "entity:harbor#0"}
        })));
        assert!(!eligible_edge(&json!({"status": "considered"})));
        assert!(eligible_edge(&json!({"status": "decided"})));
    }

    #[test]
    fn an_edge_hit_keeps_only_a_live_setting_or_rule() {
        let closed = closed(&["entity:harbor#0"]);
        let live = json!({
            "id": "relates_to:rule1",
            "in": "entity:birch",
            "rel_type": "rule",
            "status": "decided",
            "derived_from": ["entity:harbor#0"]
        });
        let hit = edge_dependent(&live, &closed).expect("live rule");
        assert_eq!(hit.canonical, "relates_to:rule1");
        assert_eq!(hit.entity_id, "entity:birch");
        let skipped = [
            json!({"id": "relates_to:uses1", "rel_type": "uses", "status": "decided", "derived_from": ["entity:harbor#0"]}),
            json!({"id": "relates_to:closed", "rel_type": "setting", "status": "decided", "valid_until": "2026-04-11T00:00:00Z", "derived_from": ["entity:harbor#0"]}),
            json!({"id": "relates_to:pending", "rel_type": "setting", "status": "decided", "pending": {"since": "s2"}, "derived_from": ["entity:harbor#0"]}),
            json!({"id": "relates_to:open", "rel_type": "rule", "status": "considered", "derived_from": ["entity:harbor#0"]}),
            json!({"id": "relates_to:other", "rel_type": "rule", "status": "decided", "derived_from": ["entity:maple#0"]}),
            json!({"rel_type": "setting", "status": "decided", "derived_from": ["entity:harbor#0"]}),
        ];
        assert!(collect_edge_hits(&skipped, &closed).is_empty());
        assert_eq!(collect_edge_hits(&[live], &closed).len(), 1);
        assert!(!mentions_closed(&[], &closed));
    }

    #[test]
    fn a_slot_hit_uses_the_entity_and_index() {
        let closed = closed(&["entity:harbor#0"]);
        let entities = [
            json!({"attributes": {"v2": {"slots": []}}}),
            json!({"id": "entity:maple"}),
            json!({
                "id": "entity:holly",
                "attributes": {"v2": {"slots": [
                    {"predicate": "rule", "status": "decided", "derived_from": ["entity:other#0"]},
                    {"predicate": "uses", "status": "decided", "derived_from": ["entity:harbor#0"]}
                ]}}
            }),
            json!({
                "id": "entity:birch",
                "attributes": {"v2": {"slots": [{
                    "predicate": "setting",
                    "status": "decided",
                    "value": "10s",
                    "derived_from": ["entity:harbor#0"]
                }]}}
            }),
        ];
        let hits = collect_slot_hits(&entities, &closed);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].canonical, "entity:birch#0");
        assert_eq!(hits[0].entity_id, "entity:birch");
    }
}

#[cfg(test)]
mod usable_anchor_tests {
    use serde_json::Value;

    use super::super::write::{CloseEvent, WriteStats};
    use super::{capped_usable, DependentHit, Row};

    fn hit(canonical: &str) -> DependentHit {
        let entity_id = canonical.rsplit_once('#').unwrap().0.to_string();
        DependentHit {
            entity_id,
            canonical: canonical.to_string(),
            derived_from: vec!["premise".into()],
        }
    }

    fn setting_row(canonical: &str, pending: bool, pred: &str) -> Row {
        let (entity, index) = canonical.rsplit_once('#').unwrap();
        Row {
            id: format!("s:{canonical}"),
            entity: entity.to_string(),
            slot: Some(index.parse().unwrap()),
            edge: None,
            subject: "birch".into(),
            pred: pred.into(),
            key: "delay".into(),
            value: canonical.into(),
            evidence: Value::Null,
            from: String::new(),
            here: false,
            pending,
        }
    }

    #[test]
    fn an_unusable_hit_among_the_first_four_does_not_inflate_overflow() {
        let hits = ["e:1#0", "e:2#0", "e:3#0", "e:4#0", "e:5#0", "e:6#0"]
            .into_iter()
            .map(hit)
            .collect::<Vec<_>>();
        let rows = ["e:2#0", "e:3#0", "e:4#0", "e:5#0", "e:6#0"]
            .into_iter()
            .map(|canonical| setting_row(canonical, false, "setting"))
            .collect::<Vec<_>>();
        let stats = WriteStats {
            close_events: vec![CloseEvent {
                row_id: "premise".into(),
                reason: "rejected".into(),
                fact_index: 0,
            }],
            ..WriteStats::default()
        };
        let (kept, overflow) = capped_usable(&hits, &rows, &[], &stats);
        assert_eq!(kept.len(), 4);
        assert_eq!(overflow, 1);
        assert!(kept.iter().all(|hit| hit.canonical != "e:1#0"));
        let pending = setting_row("e:2#0", true, "setting");
        assert_eq!(
            capped_usable(&hits[..2], &[pending], &[], &stats).0.len(),
            0
        );
        let uses = setting_row("e:2#0", false, "uses");
        assert_eq!(capped_usable(&hits[..2], &[uses], &[], &stats).0.len(), 0);
        let quiet = WriteStats::default();
        assert_eq!(capped_usable(&hits, &rows, &[], &quiet).1, 0);
        let bound = [super::Anchor {
            id: "a3:0".into(),
            subject: "birch".into(),
            pred: "setting".into(),
            key: "delay".into(),
            value: "e:2#0".into(),
            text: "e:2#0".into(),
            new_value: "e:2#0".into(),
            replaces: String::new(),
            entity: String::new(),
            reason: "new",
            turn: 0,
            quote: "quoted".into(),
            live: Some(0),
            a2: false,
            closed_row: None,
        }];
        let (kept, overflow) = capped_usable(&hits[1..2], &rows[..1], &bound, &stats);
        assert!(kept.is_empty());
        assert_eq!(overflow, 0);
    }
}

#[cfg(test)]
mod push_capped_edge_tests {
    use std::collections::BTreeMap;

    use serde_json::{json, Value};

    use super::push_capped_edge;

    fn names() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("entity:harbor".into(), "harbor".into()),
            ("entity:queue".into(), "queue".into()),
            ("entity:spool".into(), "spool".into()),
        ])
    }

    fn call(edge: Value, rows: &mut Vec<super::Row>, part_of: &mut Vec<(String, String)>) {
        push_capped_edge(&edge, "s2", &names(), rows, part_of);
    }

    #[test]
    fn closed_undecided_and_non_edges_add_nothing() {
        let mut rows = Vec::new();
        let mut part_of = Vec::new();
        call(
            json!({
                "id": "relates_to:closed",
                "in": "entity:harbor",
                "out": "entity:queue",
                "rel_type": "uses",
                "status": "decided",
                "valid_until": "2026-04-11T00:00:00Z"
            }),
            &mut rows,
            &mut part_of,
        );
        call(
            json!({
                "id": "relates_to:open",
                "in": "entity:harbor",
                "out": "entity:queue",
                "rel_type": "uses",
                "status": "considered"
            }),
            &mut rows,
            &mut part_of,
        );
        call(
            json!({
                "id": "relates_to:setting",
                "in": "entity:harbor",
                "out": "entity:queue",
                "rel_type": "setting",
                "status": "decided"
            }),
            &mut rows,
            &mut part_of,
        );
        assert!(rows.is_empty());
        assert!(part_of.is_empty());
    }

    #[test]
    fn a_decided_uses_edge_is_appended_once() {
        let mut rows = Vec::new();
        let mut part_of = Vec::new();
        let edge = json!({
            "id": "relates_to:uses1",
            "in": "entity:harbor",
            "out": "entity:queue",
            "rel_type": "uses",
            "status": "decided",
            "valid_from": "2026-04-03T00:00:00Z"
        });
        call(edge.clone(), &mut rows, &mut part_of);
        call(edge, &mut rows, &mut part_of);
        assert_eq!(rows.len(), 1);
        assert!(part_of.is_empty());
        assert_eq!(rows[0].id, "e:relates_to:uses1");
        assert_eq!(rows[0].edge.as_deref(), Some("relates_to:uses1"));
        assert_eq!(rows[0].pred, "uses");
        assert_eq!(rows[0].subject, "harbor");
        assert_eq!(rows[0].value, "queue");
        assert!(!rows[0].pending);
        assert_eq!(rows[0].from, "2026-04-03T00:00:00Z");
    }

    #[test]
    fn a_decided_part_of_records_the_link_and_the_row() {
        let mut rows = Vec::new();
        let mut part_of = Vec::new();
        call(
            json!({
                "id": "relates_to:part1",
                "in": "entity:spool",
                "out": "entity:harbor",
                "rel_type": "part_of",
                "status": "decided"
            }),
            &mut rows,
            &mut part_of,
        );
        assert_eq!(
            part_of,
            vec![("entity:spool".into(), "entity:harbor".into())]
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "e:relates_to:part1");
        assert_eq!(rows[0].edge.as_deref(), Some("relates_to:part1"));
        assert_eq!(rows[0].pred, "part_of");
        assert_eq!(rows[0].subject, "spool");
        assert_eq!(rows[0].value, "harbor");
    }

    #[test]
    fn a_pending_decided_edge_is_kept() {
        let mut rows = Vec::new();
        let mut part_of = Vec::new();
        call(
            json!({
                "id": "relates_to:held",
                "in": "entity:harbor",
                "out": "entity:queue",
                "rel_type": "depends_on",
                "status": "decided",
                "pending": {"since": "s2", "conflicts": "entity:queue"}
            }),
            &mut rows,
            &mut part_of,
        );
        assert!(part_of.is_empty());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].pending);
        assert_eq!(rows[0].id, "e:relates_to:held");
        assert_eq!(rows[0].pred, "depends_on");
    }
}

#[cfg(test)]
mod drop_line_tests {
    use super::{drop_line, quote_head};

    #[test]
    fn the_line_is_the_reason_the_keys_and_eighty_chars() {
        let quote = format!("Agreed\n{}", "q".repeat(90));
        let line = drop_line("quote too short", "acme:layout", "acme:format", &quote);
        let head = quote_head(&quote);
        assert_eq!(head.chars().count(), 80);
        assert!(!head.contains('\n'));
        assert_eq!(
            line,
            format!("consolidate drop: quote too short | acme:layout | acme:format | {head}")
        );
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains("plain text"));
        let mut raw = String::from("a");
        raw.push(char::from(0x1b));
        raw.push_str("[31mred\tb");
        raw.push('\u{0}');
        raw.push('c');
        let head = quote_head(&raw);
        assert!(!head.chars().any(char::is_control), "{head:?}");
        assert_eq!(head, "a [31mred b c");
    }
}
