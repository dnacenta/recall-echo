// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic match of kept facts to stored live rows.
//!
//! Runs after pass 1. It may fill or canonicalise a setting key, or name one
//! `uses` edge to retire. It does not change a value, quote, turn, status or
//! provenance, and it does not create a fact.

use super::ground::SessionGround;
use super::ground::{
    numbers_in, sentence_containing, sentences, specific_key_tokens, tokens_required,
};
use super::known::KnownRow;
use super::schema::{key_is_empty, normalise, Fact, Predicate, Role};
use super::write::{alphanumeric_runs, keys_match, KEY_AFFIXES};
use crate::graph::types::MatchReport;

/// One stored row a fact may retire. Slots are checked by content, not by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchTarget {
    pub row_id: String,
    pub subject: String,
    pub predicate: String,
    pub key: String,
    pub value: String,
}

/// Facts after key fill or canonicalisation, aligned with `targets`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reconciled {
    pub facts: Vec<Fact>,
    pub targets: Vec<Option<MatchTarget>>,
    pub report: MatchReport,
}

const DEPARTURE: &[&str] = &[
    "instead",
    "rather",
    "replace",
    "replaces",
    "replaced",
    "replacing",
    "switch",
    "switched",
    "switching",
    "migrate",
    "migrated",
    "move",
    "moved",
    "drop",
    "dropped",
    "remove",
    "removed",
    "not",
];

/// Apply A1–A3. A fact that does not match is returned unchanged.
#[must_use]
pub(crate) fn reconcile(facts: &[Fact], rows: &[KnownRow], ground: &SessionGround) -> Reconciled {
    let mut out = Vec::with_capacity(facts.len());
    let mut targets = Vec::with_capacity(facts.len());
    let mut report = MatchReport::default();
    for fact in facts {
        let (next, target) = decide(fact, rows, ground, &mut report);
        out.push(next);
        targets.push(target);
    }
    Reconciled {
        facts: out,
        targets,
        report,
    }
}

fn decide(
    fact: &Fact,
    rows: &[KnownRow],
    ground: &SessionGround,
    report: &mut MatchReport,
) -> (Fact, Option<MatchTarget>) {
    if fact.status != "decided" {
        return (fact.clone(), None);
    }
    match fact.predicate {
        Predicate::Setting if key_is_empty(&fact.key) => fill_key(fact, rows, ground, report),
        Predicate::Setting => canonicalise(fact, rows, report),
        Predicate::Uses => edge_target(fact, rows, ground, report),
        _ => (fact.clone(), None),
    }
}

fn fill_key(
    fact: &Fact,
    rows: &[KnownRow],
    ground: &SessionGround,
    report: &mut MatchReport,
) -> (Fact, Option<MatchTarget>) {
    let sentence = sentence_containing(cited(fact, ground), &fact.quote);
    let mut dropped_shape = false;
    let mut kept = Vec::new();
    for row in decided_settings(fact, rows) {
        if !every_required_token(&row.key, sentence) {
            continue;
        }
        if numbers_in(&row.value).is_empty() != numbers_in(&fact.object_name).is_empty() {
            dropped_shape = true;
            continue;
        }
        kept.push(row);
    }
    let target = match kept.as_slice() {
        [one] => Some(target_of(one)),
        [] if dropped_shape => {
            skip(report, "shape");
            None
        }
        [] => None,
        _ => {
            skip(report, "ambiguous");
            None
        }
    };
    apply_key(fact, target, report, true)
}

fn canonicalise(
    fact: &Fact,
    rows: &[KnownRow],
    report: &mut MatchReport,
) -> (Fact, Option<MatchTarget>) {
    let settings = decided_settings(fact, rows);
    if settings.iter().any(|row| keys_match(&fact.key, &row.key)) {
        return (fact.clone(), None);
    }
    let want = key_tokens(&fact.key);
    if want.is_empty() {
        return (fact.clone(), None);
    }
    let hits: Vec<&KnownRow> = settings
        .into_iter()
        .filter(|row| key_tokens(&row.key) == want)
        .collect();
    let target = match hits.as_slice() {
        [one] => Some(target_of(one)),
        [] => None,
        _ => {
            skip(report, "ambiguous");
            None
        }
    };
    apply_key(fact, target, report, false)
}

fn edge_target(
    fact: &Fact,
    rows: &[KnownRow],
    ground: &SessionGround,
    report: &mut MatchReport,
) -> (Fact, Option<MatchTarget>) {
    let role = fact_role(fact);
    if !fact.replaces.trim().is_empty() || generic_role(role) {
        return (fact.clone(), None);
    }
    let turn = cited(fact, ground);
    let same: Vec<&KnownRow> = rows
        .iter()
        .filter(|row| {
            row.predicate == "uses" && same_subject(fact, row) && same_role(role, &row.role)
        })
        .collect();
    if same
        .iter()
        .any(|row| normalise(&row.value) == normalise(&fact.object_name))
    {
        skip(report, "already_live");
        return (fact.clone(), None);
    }
    let hits: Vec<&KnownRow> = same
        .into_iter()
        .filter(|row| {
            normalise(&row.value) != normalise(&fact.object_name)
                && mentioned_with_cue(&row.value, turn)
        })
        .collect();
    match hits.as_slice() {
        [one] => {
            report.edge_targets += 1;
            (fact.clone(), Some(target_of(one)))
        }
        [] => (fact.clone(), None),
        _ => {
            skip(report, "ambiguous");
            (fact.clone(), None)
        }
    }
}

fn apply_key(
    fact: &Fact,
    target: Option<MatchTarget>,
    report: &mut MatchReport,
    fill: bool,
) -> (Fact, Option<MatchTarget>) {
    let Some(target) = target else {
        return (fact.clone(), None);
    };
    if fill {
        report.key_filled += 1;
    } else {
        report.key_canonical += 1;
    }
    let mut next = fact.clone();
    next.key = target.key.clone();
    (next, Some(target))
}

fn decided_settings<'a>(fact: &Fact, rows: &'a [KnownRow]) -> Vec<&'a KnownRow> {
    rows.iter()
        .filter(|row| {
            row.predicate == "setting"
                && row.status == "decided"
                && same_subject(fact, row)
                && !key_is_empty(&row.key)
        })
        .collect()
}

fn every_required_token(key: &str, sentence: &str) -> bool {
    let specific = specific_key_tokens(key);
    let required = tokens_required(key, &specific);
    !required.is_empty() && required.iter().all(|token| whole_token(token, sentence))
}

/// Gate 3's direct rule on one turn: any specific key token, and every stem token of a `_version` key.
fn direct_key(key: &str, turn: &str) -> bool {
    let specific = specific_key_tokens(key);
    if specific.is_empty() {
        return false;
    }
    if let Some(stem) = key.strip_suffix("_version") {
        let parts: Vec<&str> = stem.split('_').filter(|token| !token.is_empty()).collect();
        if parts.is_empty() || !parts.iter().all(|token| whole_token(token, turn)) {
            return false;
        }
    }
    specific.iter().any(|token| whole_token(token, turn))
}

fn whole_token(token: &str, text: &str) -> bool {
    let token = normalise(token);
    !token.is_empty() && normalise(text).split_whitespace().any(|word| word == token)
}

/// Separator and camel tokens, lowercased, with [`KEY_AFFIXES`] removed.
/// Equality is the whole list. A shorter list is not a stem and not a subset.
fn key_tokens(key: &str) -> Vec<String> {
    let chars: Vec<char> = key.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for (index, ch) in chars.iter().enumerate() {
        let camel = index > 0 && ch.is_uppercase() && chars[index - 1].is_lowercase();
        if !ch.is_alphanumeric() || camel {
            push_token(&mut tokens, &mut current);
        }
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        }
    }
    push_token(&mut tokens, &mut current);
    tokens
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if current.is_empty() {
        return;
    }
    let token = std::mem::take(current);
    if !KEY_AFFIXES.contains(&token.as_str()) {
        tokens.push(token);
    }
}

fn same_subject(fact: &Fact, row: &KnownRow) -> bool {
    let subject = normalise(&fact.subject_name);
    !subject.is_empty() && subject == normalise(&row.subject)
}

fn fact_role(fact: &Fact) -> &str {
    fact.role.map(Role::as_str).unwrap_or("none")
}

fn same_role(fact_role: &str, row_role: &str) -> bool {
    let row = if row_role.is_empty() {
        "none"
    } else {
        row_role
    };
    fact_role == row
}

fn generic_role(role: &str) -> bool {
    matches!(role, "other" | "none" | "library")
}

fn mentioned_with_cue(old: &str, turn: &str) -> bool {
    mention(old, turn)
        && sentences(turn).any(|sentence| mention(old, sentence) && departure_near(old, sentence))
}

fn target_of(row: &KnownRow) -> MatchTarget {
    MatchTarget {
        row_id: row.id.clone(),
        subject: row.subject.clone(),
        predicate: row.predicate.clone(),
        key: row.key.clone(),
        value: row.value.clone(),
    }
}

fn cited<'a>(fact: &'a Fact, ground: &'a SessionGround) -> &'a str {
    ground.turn_text(fact.turn).unwrap_or(fact.quote.as_str())
}

fn skip(report: &mut MatchReport, reason: &str) {
    *report.skipped.entry(reason.to_string()).or_insert(0) += 1;
}

/// Why a decided fact must not retire `old_value`, or nothing when it may.
#[must_use]
pub(crate) fn retirement_skip(fact: &Fact, old_value: &str, turn: &str) -> Option<&'static str> {
    let keyed = fact.predicate == Predicate::Setting && !key_is_empty(&fact.key);
    if !(keyed && direct_key(&fact.key, turn)) && !mention(old_value, turn) {
        return Some("no_mention");
    }
    if sentences(turn).any(|sentence| coexist(old_value, &fact.object_name, sentence)) {
        return Some("coexist");
    }
    None
}

/// `old`'s alphanumeric runs occur as one contiguous run of whole tokens in `text`.
#[must_use]
pub(crate) fn mention(old: &str, text: &str) -> bool {
    !value_spans(text, old).is_empty()
}

/// Both values share a sentence and either sit on a connector or a keep-cue is beside the old value.
#[must_use]
pub(crate) fn coexist(old: &str, new: &str, sentence: &str) -> bool {
    let old_spans = value_spans(sentence, old);
    let new_spans = value_spans(sentence, new);
    if old_spans.is_empty() || new_spans.is_empty() {
        return false;
    }
    connectors_only(nearest_between(&old_spans, &new_spans, sentence))
        || near_cue(&old_spans, sentence, false)
}

/// A closed departure cue within three word tokens of `old`.
#[must_use]
pub(crate) fn departure_near(old: &str, sentence: &str) -> bool {
    let spans = value_spans(sentence, old);
    !spans.is_empty() && near_cue(&spans, sentence, true)
}

fn value_spans(text: &str, value: &str) -> Vec<(usize, usize)> {
    let want = alphanumeric_runs(value);
    if want.is_empty() {
        return Vec::new();
    }
    let runs = run_spans(text);
    if runs.len() < want.len() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for index in 0..=runs.len() - want.len() {
        let window = &runs[index..index + want.len()];
        if window
            .iter()
            .map(|(token, _, _)| token.as_str())
            .eq(want.iter().map(String::as_str))
        {
            spans.push((window[0].1, window[want.len() - 1].2));
        }
    }
    spans
}

fn run_spans(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    let mut current = String::new();
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if start.is_none() {
                start = Some(index);
            }
            current.extend(ch.to_lowercase());
        } else if let Some(from) = start.take() {
            out.push((std::mem::take(&mut current), from, index));
        }
    }
    if let Some(from) = start {
        out.push((current, from, text.len()));
    }
    out
}

fn nearest_between<'a>(old: &[(usize, usize)], new: &[(usize, usize)], text: &'a str) -> &'a str {
    let mut best_gap = usize::MAX;
    let mut best = "";
    for &(left, right) in old {
        for &(start, end) in new {
            let (gap, between) = if right <= start {
                (start - right, &text[right..start])
            } else if end <= left {
                (left - end, &text[end..left])
            } else {
                (0, "")
            };
            if gap < best_gap {
                best_gap = gap;
                best = between;
            }
        }
    }
    best
}

fn connectors_only(text: &str) -> bool {
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            word.extend(ch.to_lowercase());
            continue;
        }
        if !take_connector(&mut word) {
            return false;
        }
        if !(ch.is_whitespace() || matches!(ch, ',' | '/' | '|' | '&' | '+')) {
            return false;
        }
    }
    take_connector(&mut word)
}

fn take_connector(word: &mut String) -> bool {
    if word.is_empty() {
        return true;
    }
    let ok = matches!(
        word.as_str(),
        "and" | "or" | "plus" | "both" | "nor" | "with" | "alongside"
    );
    word.clear();
    ok
}

fn near_cue(spans: &[(usize, usize)], sentence: &str, departure: bool) -> bool {
    let tokens = run_spans(sentence);
    for &(start, end) in spans {
        let before: Vec<&str> = tokens
            .iter()
            .filter(|(_, _, stop)| *stop <= start)
            .map(|(word, _, _)| word.as_str())
            .collect();
        let from = before.len().saturating_sub(3);
        let after: Vec<&str> = tokens
            .iter()
            .filter(|(_, begin, _)| *begin >= end)
            .map(|(word, _, _)| word.as_str())
            .take(3)
            .collect();
        if has_cue(&before[from..], departure) || has_cue(&after, departure) {
            return true;
        }
    }
    false
}

fn has_cue(tokens: &[&str], departure: bool) -> bool {
    if departure {
        tokens.iter().any(|token| DEPARTURE.contains(token))
            || tokens.windows(2).any(|pair| *pair == ["no", "longer"])
    } else {
        tokens
            .iter()
            .any(|token| matches!(*token, "keep" | "also" | "too" | "alongside"))
            || tokens.windows(2).any(|pair| *pair == ["as", "well"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::confidence::Provenance;
    use crate::graph::v2::input::{Turn, TurnRole};
    use crate::graph::v2::schema::Kind;

    fn ground(text: &str) -> SessionGround {
        SessionGround::prepare(&[Turn {
            index: 0,
            role: TurnRole::User,
            text: text.to_string(),
        }])
    }

    fn setting(key: &str, value: &str, quote: &str) -> Fact {
        Fact {
            subject_name: "app".into(),
            subject_kind: Kind::Project,
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

    fn uses(object: &str, role: Role, quote: &str) -> Fact {
        Fact {
            subject_name: "app".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Uses,
            role: Some(role),
            key: String::new(),
            object_name: object.into(),
            object_kind: Some(Kind::Technology),
            status: "decided".into(),
            turn: 0,
            quote: quote.into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    fn row(predicate: &str, key: &str, value: &str, role: &str) -> KnownRow {
        KnownRow {
            subject: "app".into(),
            predicate: predicate.into(),
            key: key.into(),
            value: value.into(),
            role: role.into(),
            id: format!("{predicate}:{key}:{value}:{role}"),
            status: "decided".into(),
        }
    }

    fn one(fact: Fact, rows: Vec<KnownRow>, text: &str) -> Reconciled {
        reconcile(&[fact], &rows, &ground(text))
    }

    #[test]
    fn a1_fills_the_only_key_whose_tokens_are_in_the_sentence() {
        let text = "Switch the log format to json lines.";
        let got = one(
            setting("", "json lines", text),
            vec![row("setting", "log_format", "plain", "")],
            text,
        );
        assert_eq!(got.facts[0].key, "log_format");
        assert_eq!(got.facts[0].object_name, "json lines");
        assert_eq!(got.report.key_filled, 1);
        assert_eq!(
            got.targets[0].as_ref().map(|target| target.value.as_str()),
            Some("plain")
        );
    }

    #[test]
    fn a1_needs_every_required_key_token() {
        let text = "Set the backend to disk.";
        let got = one(
            setting("", "disk", text),
            vec![row("setting", "cache_backend", "memory", "")],
            text,
        );
        assert!(got.facts[0].key.is_empty());
        assert!(got.targets[0].is_none());
        assert!(got.report.is_blank());
    }

    #[test]
    fn a1_two_qualifying_keys_are_ambiguous() {
        let text = "Read and write mode go lenient.";
        let got = one(
            setting("", "lenient", text),
            vec![
                row("setting", "read_mode", "strict", ""),
                row("setting", "write_mode", "strict", ""),
            ],
            text,
        );
        assert!(got.facts[0].key.is_empty());
        assert_eq!(got.report.skipped.get("ambiguous").copied(), Some(1));
    }

    #[test]
    fn a1_rejects_a_number_shape_mismatch() {
        let text = "No retry limit any more, unlimited.";
        let got = one(
            setting("", "unlimited", text),
            vec![row("setting", "retry_limit", "5", "")],
            text,
        );
        assert!(got.facts[0].key.is_empty());
        assert!(got.targets[0].is_none());
        assert_eq!(got.report.skipped.get("shape").copied(), Some(1));
    }

    #[test]
    fn a1_equal_value_still_fills_the_key() {
        let text = "The log format stays plain.";
        let got = one(
            setting("", "plain", text),
            vec![row("setting", "log_format", "plain", "")],
            text,
        );
        assert_eq!(got.facts[0].key, "log_format");
        assert_eq!(got.facts[0].object_name, "plain");
        assert_eq!(got.report.key_filled, 1);
    }

    #[test]
    fn a2_rewrites_separators_and_case_onto_the_stored_key() {
        let text = "The test command is now just check.";
        let got = one(
            setting("testCommand", "just check", text),
            vec![row("setting", "test_command", "make check", "")],
            text,
        );
        assert_eq!(got.facts[0].key, "test_command");
        assert_eq!(got.report.key_canonical, 1);
        assert_eq!(got.facts[0].object_name, "just check");
    }

    #[test]
    fn a2_two_equal_token_lists_are_ambiguous() {
        let got = one(
            setting("BuildDir", "out", "Build dir is out."),
            vec![
                row("setting", "build_dir", "target", ""),
                row("setting", "build-dir", "dist", ""),
            ],
            "Build dir is out.",
        );
        assert_eq!(got.facts[0].key, "BuildDir");
        assert_eq!(got.report.skipped.get("ambiguous").copied(), Some(1));
        assert_eq!(got.report.key_canonical, 0);
    }

    #[test]
    fn a2_does_not_treat_a_subset_as_the_same_key() {
        let got = one(
            setting("storage", "disk", "Storage is disk."),
            vec![row("setting", "tag_storage", "memory", "")],
            "Storage is disk.",
        );
        assert_eq!(got.facts[0].key, "storage");
        assert_eq!(got.report.key_canonical, 0);
        assert!(got.targets[0].is_none());
    }

    #[test]
    fn a2_leaves_a_key_preferred_key_already_rewrites() {
        let got = one(
            setting("port_value", "9", "The port value is 9."),
            vec![row("setting", "port", "8", "")],
            "The port value is 9.",
        );
        assert_eq!(got.facts[0].key, "port_value");
        assert_eq!(got.report.key_canonical, 0);
    }

    #[test]
    fn a3_targets_the_one_role_match_with_a_departure_cue() {
        let text = "Use LibB instead of LibA for linting.";
        let got = one(
            uses("LibB", Role::Linter, text),
            vec![row("uses", "", "LibA", "linter")],
            text,
        );
        assert_eq!(got.report.edge_targets, 1);
        assert_eq!(
            got.targets[0].as_ref().map(|target| target.row_id.as_str()),
            Some("uses::LibA:linter")
        );
        assert!(got.facts[0].replaces.is_empty());
    }

    #[test]
    fn a3_without_a_departure_cue_sets_no_target() {
        let text = "Run LibB with LibA.";
        let got = one(
            uses("LibB", Role::TestFramework, text),
            vec![row("uses", "", "LibA", "test_framework")],
            text,
        );
        assert!(got.targets[0].is_none());
        assert_eq!(got.report.edge_targets, 0);
        assert!(coexist("LibA", "LibB", text));
    }

    #[test]
    fn a3_generic_roles_do_not_retire() {
        let text = "Use LibB instead of LibA for linting.";
        for role in [Role::Other, Role::Library] {
            let got = one(
                uses("LibB", role, text),
                vec![row("uses", "", "LibA", role.as_str())],
                text,
            );
            assert!(got.targets[0].is_none(), "{role:?}");
            assert_eq!(got.report.edge_targets, 0);
        }
    }

    #[test]
    fn a3_requires_the_old_value_to_be_mentioned() {
        let text = "Switch the linter to LibB.";
        assert!(!mention("LibA", text));
        let got = one(
            uses("LibB", Role::Linter, text),
            vec![row("uses", "", "LibA", "linter")],
            text,
        );
        assert!(got.targets[0].is_none());
        assert_eq!(got.report.edge_targets, 0);
    }

    #[test]
    fn a_component_subject_does_not_match_a_project_fact() {
        let text = "Switch the log format to json lines.";
        let mut stored = row("setting", "log_format", "plain", "");
        stored.subject = "widget".into();
        let got = one(setting("", "json lines", text), vec![stored], text);
        assert!(got.facts[0].key.is_empty());
        let mut fact = setting("", "json lines", text);
        fact.subject_name = "widget".into();
        fact.subject_kind = Kind::Component;
        let got = one(fact, vec![row("setting", "log_format", "plain", "")], text);
        assert!(got.facts[0].key.is_empty());
    }

    #[test]
    fn mention_is_a_whole_token_run_and_never_a_substring() {
        assert!(mention("plain", "The log format stays plain."));
        assert!(mention("3.12", "Test on Python 3.12 and 3.13."));
        assert!(!mention(
            "csv|html",
            "Use the standard library HTML parser."
        ));
        assert!(!mention("lib", "Switch LibB now."));
        assert!(!mention("12", "312"));
    }

    #[test]
    fn coexist_blocks_lists_and_keep_cues_and_allows_a_replacement() {
        assert!(coexist("3.4", "3.5", "Test on runtime 3.4 and 3.5."));
        assert!(!coexist(
            "3.4",
            "3.5",
            "Upgrade the runtime from 3.4 to 3.5."
        ));
        assert!(coexist("3.4", "3.5", "Keep 3.4, also test 3.5."));
        assert!(!coexist("8080", "8765", "8080 -> 8765"));
        assert!(departure_near(
            "LibA",
            "Use LibB instead of LibA for linting."
        ));
        assert!(!departure_near("LibA", "Run LibB with LibA."));
    }

    #[test]
    fn a_rule_and_a_depends_on_row_never_match() {
        let text = "Switch the log format to json lines.";
        let got = one(
            setting("", "json lines", text),
            vec![row("rule", "log_format", "plain", "")],
            text,
        );
        assert!(got.facts[0].key.is_empty());
        let got = one(
            uses("LibB", Role::Linter, "Use LibB instead of LibA."),
            vec![row("depends_on", "", "LibA", "linter")],
            "Use LibB instead of LibA.",
        );
        assert!(got.targets[0].is_none());
    }
}
