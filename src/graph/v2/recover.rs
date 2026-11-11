// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-only scan of text turns for values the store does not hold.
//!
//! The pass never writes, never adds anchors, and never calls a model.
//! Listed items are missing values, not stored facts.

use std::collections::BTreeSet;

use serde_json::Value;

use super::super::error::GraphError;
use super::super::{deserialize_take, GraphMemory};
use super::consolidate::{for_now_pair, hedge_word, tentative_word};
use super::input::{ProjectIdentity, SessionInput, Turn, TurnRole};
use super::known_match::{cue_words, is_negator};
use super::schema::Fact;
use super::write::{alphanumeric_runs, values_equal};
use crate::graph::types::{RecoveredItem, RecoveredNearest, RecoveredReport};

const TURN_CAP: usize = 4;
const SESSION_CAP: usize = 16;
const NAME_CAP: usize = 48;
const QUOTE_CHARS: usize = 200;
const READ_ERROR: &str = "stored values could not be read; recovered list is empty";

const SETTING_VERBS: &[&str] = &[
    "set", "change", "changed", "bump", "bumped", "raise", "raised", "lower", "lowered", "rename",
    "renamed", "switch", "switched",
];

const PLACEHOLDERS: &[&str] = &["x", "n", "foo", "bar", "…", "..."];

const LIVE_SQL: &str = "SELECT name, attributes.v2.slots AS slots, \
     ->(relates_to WHERE valid_until = NONE).out.name AS edge_objects, \
     ->(relates_to WHERE valid_until = NONE).rel_type AS edge_keys \
     FROM entity WHERE name INSIDE $names";

#[cfg(test)]
thread_local! {
    static FAIL_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The next store read returns an error and lists nothing.
#[cfg(test)]
pub(crate) fn fail_next_read() {
    FAIL_READ.with(|cell| cell.set(true));
}

struct Hit {
    turn: usize,
    role: &'static str,
    pattern: &'static str,
    key: String,
    value: String,
    quote: String,
}

struct Tick<'a> {
    inner: &'a str,
    end: usize,
}

struct LiveRow {
    subject: String,
    key: String,
    value: String,
}

/// Scan `session` when `enabled` and record misses on `report`.
///
/// A store read failure adds one error and lists nothing. The caller still
/// succeeds. `facts` supplies subjects stored this session; the project name
/// and its aliases are always included, up to 48 names.
pub(crate) async fn fill_report(
    gm: &GraphMemory,
    enabled: bool,
    session: &SessionInput,
    facts: &[Fact],
    report: &mut crate::graph::types::IngestionReport,
) {
    if !enabled {
        return;
    }
    let hits = collect_hits(&session.turns);
    if hits.is_empty() {
        return;
    }
    let names = neighbourhood(&session.project, facts);
    let live = match load_live(gm, &names).await {
        Ok(rows) => rows,
        Err(_) => {
            report.errors.push(READ_ERROR.to_string());
            return;
        }
    };
    report.recovered = classify(&hits, &live);
}

fn collect_hits(turns: &[Turn]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (index, turn) in turns.iter().enumerate() {
        if in_scope(turns, index) {
            push_turn(turn, &mut hits);
        }
    }
    hits
}

fn in_scope(turns: &[Turn], index: usize) -> bool {
    match turns[index].role {
        TurnRole::User => true,
        TurnRole::Assistant => index > 0 && turns[index - 1].role == TurnRole::User,
    }
}

fn push_turn(turn: &Turn, hits: &mut Vec<Hit>) {
    for sentence in sentence_slices(&turn.text) {
        if !sentence_blocked(sentence) {
            push_sentence(turn, sentence, hits);
        }
    }
}

fn push_sentence(turn: &Turn, sentence: &str, hits: &mut Vec<Hit>) {
    let quote = clip_quote(sentence);
    let spans = backtick_spans(sentence);
    push_assigns(turn, sentence, &quote, &spans, hits);
    push_spans(turn, &quote, &spans, hits);
    push_dotted(turn, sentence, &quote, &spans, hits);
    push_becomes(turn, sentence, &quote, hits);
    push_is_now(turn, sentence, &quote, hits);
    push_to_value(turn, sentence, &quote, hits);
}

fn sentence_blocked(sentence: &str) -> bool {
    sentence.contains('?') || words_blocked(&cue_words(sentence))
}

fn words_blocked(words: &[(String, usize, usize)]) -> bool {
    has_for_now(words) || words.iter().any(|(word, _, _)| blocked_word(word))
}

fn has_for_now(words: &[(String, usize, usize)]) -> bool {
    words
        .windows(2)
        .any(|pair| for_now_pair(&pair[0].0, &pair[1].0))
}

fn blocked_word(word: &str) -> bool {
    hedge_word(word) || tentative_word(word) || is_negator(word)
}

fn sentence_slices(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if sentence_boundary(bytes, index) {
            push_slice(text, start, index + 1, &mut out);
            start = index + 1;
        }
        index += 1;
    }
    if start < bytes.len() {
        push_slice(text, start, bytes.len(), &mut out);
    }
    out
}

fn sentence_boundary(bytes: &[u8], index: usize) -> bool {
    match bytes[index] {
        b'\n' => true,
        b'.' | b'!' | b'?' => next_is_end(bytes, index),
        _ => false,
    }
}

fn next_is_end(bytes: &[u8], index: usize) -> bool {
    let next = index + 1;
    next >= bytes.len() || bytes[next].is_ascii_whitespace()
}

fn push_slice<'a>(text: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    let sentence = text[start..end].trim();
    if !sentence.is_empty() {
        out.push(sentence);
    }
}

fn clip_quote(sentence: &str) -> String {
    sentence.chars().take(QUOTE_CHARS).collect()
}

fn backtick_spans(sentence: &str) -> Vec<Tick<'_>> {
    let bytes = sentence.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            if let Some(close) = sentence[index + 1..].find('`') {
                let start = index + 1;
                let end = start + close;
                if end > start {
                    spans.push(Tick {
                        inner: &sentence[start..end],
                        end: end + 1,
                    });
                }
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    spans
}

fn push_assigns(turn: &Turn, sentence: &str, quote: &str, spans: &[Tick<'_>], hits: &mut Vec<Hit>) {
    for span in spans {
        if let Some((key, value)) = assign_inside(span.inner) {
            push_hit(turn, quote, "backtick_assign", key, value, hits);
            continue;
        }
        if let Some((key, value)) = assign_outside(sentence, span) {
            push_hit(turn, quote, "backtick_assign", key, value, hits);
        }
    }
}

fn assign_inside(inner: &str) -> Option<(String, String)> {
    let (key, raw) = split_assign(inner)?;
    let key = key.trim();
    if !is_key(key) {
        return None;
    }
    let value = strip_quotes(raw.trim());
    kept_assign(&value).then(|| (key.to_string(), value))
}

fn split_assign(inner: &str) -> Option<(&str, &str)> {
    let index = inner.find(['=', ':'])?;
    let key = inner[..index].trim();
    let value = inner[index + 1..].trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key, value))
}

fn assign_outside(sentence: &str, span: &Tick<'_>) -> Option<(String, String)> {
    let key = span.inner.trim();
    if !bare_key(key) {
        return None;
    }
    let rest = sentence[span.end..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let value = quoted_body(rest)?;
    kept_assign(&value).then(|| (key.to_string(), value))
}

fn bare_key(key: &str) -> bool {
    is_key(key) && !key.contains('=') && !key.contains(':')
}

fn quoted_body(rest: &str) -> Option<String> {
    let mut chars = rest.chars();
    let open = chars.next()?;
    if open != '"' && open != '\'' {
        return None;
    }
    let mut body = String::new();
    for ch in chars {
        if ch == open {
            return Some(body);
        }
        body.push(ch);
    }
    None
}

fn strip_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && matching_quotes(bytes[0], bytes[bytes.len() - 1]) {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

fn matching_quotes(open: u8, close: u8) -> bool {
    (open == b'"' && close == b'"') || (open == b'\'' && close == b'\'')
}

fn push_spans(turn: &Turn, quote: &str, spans: &[Tick<'_>], hits: &mut Vec<Hit>) {
    for span in spans {
        if assign_inside(span.inner).is_some() {
            continue;
        }
        if span_value(span.inner) {
            push_hit(
                turn,
                quote,
                "backtick_span",
                String::new(),
                span.inner.to_string(),
                hits,
            );
        }
    }
}

fn span_value(inner: &str) -> bool {
    kept_assign(inner) && span_shape(inner)
}

fn span_shape(inner: &str) -> bool {
    is_path(inner) || dotted_ident(inner, false) || snake_or_camel(inner) || is_command(inner)
}

fn push_dotted(turn: &Turn, sentence: &str, quote: &str, spans: &[Tick<'_>], hits: &mut Vec<Hit>) {
    let mut index = 0usize;
    while index < sentence.len() {
        if !sentence.is_char_boundary(index) || covered(spans, index) {
            index += 1;
            continue;
        }
        match dotted_at(sentence, index) {
            Some((end, text)) => {
                if dotted_ident(&text, true) {
                    push_hit(turn, quote, "dotted_ident", String::new(), text, hits);
                }
                index = end.max(index + 1);
            }
            None => index += 1,
        }
    }
}

fn covered(spans: &[Tick<'_>], index: usize) -> bool {
    spans.iter().any(|span| tick_covers(span, index))
}

fn tick_covers(span: &Tick<'_>, index: usize) -> bool {
    let open = span.end.saturating_sub(span.inner.len() + 2);
    index >= open && index < span.end
}

fn dotted_at(sentence: &str, index: usize) -> Option<(usize, String)> {
    if !ident_start(sentence, index) {
        return None;
    }
    let rest = &sentence[index..];
    let cursor = dotted_end(rest.as_bytes());
    if cursor == 0 {
        return None;
    }
    Some((index + cursor, rest[..cursor].to_string()))
}

fn ident_start(sentence: &str, index: usize) -> bool {
    let bytes = sentence.as_bytes();
    bytes[index].is_ascii_alphabetic() && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
}

fn dotted_end(bytes: &[u8]) -> usize {
    let mut cursor = 0usize;
    let mut parts = 0usize;
    loop {
        let start = cursor;
        cursor = ident_end(bytes, cursor);
        if cursor == start {
            break;
        }
        parts += 1;
        if cursor >= bytes.len() || bytes[cursor] != b'.' {
            break;
        }
        cursor += 1;
    }
    if parts < 2 {
        return 0;
    }
    trim_dot(bytes, cursor)
}

fn ident_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_alphanumeric() {
        index += 1;
    }
    index
}

fn trim_dot(bytes: &[u8], cursor: usize) -> usize {
    if cursor > 0 && bytes[cursor - 1] == b'.' {
        cursor - 1
    } else {
        cursor
    }
}

fn dotted_ident(span: &str, upper_tail: bool) -> bool {
    if span.contains(' ') {
        return false;
    }
    let parts: Vec<&str> = span.split('.').filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 || !parts.iter().copied().all(part_ok) {
        return false;
    }
    !upper_tail || tail_has_upper(&parts)
}

fn part_ok(part: &str) -> bool {
    part.chars().count() >= 2 && part.chars().all(|ch| ch.is_ascii_alphanumeric())
}

fn tail_has_upper(parts: &[&str]) -> bool {
    parts[1..]
        .iter()
        .any(|part| part.chars().any(|ch| ch.is_ascii_uppercase()))
}

fn push_becomes(turn: &Turn, sentence: &str, quote: &str, hits: &mut Vec<Hit>) {
    let words = cue_words(sentence);
    let Some(pos) = word_at(&words, "becomes") else {
        return;
    };
    let head = head_before(sentence, words[pos].1);
    if head.is_empty() {
        return;
    }
    if let Some(value) = shaped_value(&sentence[words[pos].2..]) {
        push_hit(turn, quote, "becomes_value", head, value, hits);
    }
}

fn push_is_now(turn: &Turn, sentence: &str, quote: &str, hits: &mut Vec<Hit>) {
    let words = cue_words(sentence);
    let Some(pos) = adjacent(&words, "is", "now") else {
        return;
    };
    let head = head_before(sentence, words[pos].1);
    if head.is_empty() {
        return;
    }
    let after = words
        .get(pos + 1)
        .map(|word| word.2)
        .unwrap_or(words[pos].2);
    if let Some(value) = shaped_value(&sentence[after..]) {
        push_hit(turn, quote, "is_now_value", head, value, hits);
    }
}

fn push_to_value(turn: &Turn, sentence: &str, quote: &str, hits: &mut Vec<Hit>) {
    if from_to_benchmark(sentence) {
        return;
    }
    let words = cue_words(sentence);
    let Some(verb) = setting_verb(&words) else {
        return;
    };
    let Some(to_at) = to_within(&words, verb) else {
        return;
    };
    let key = head_between(sentence, words[verb].2, words[to_at].1);
    if let Some(value) = shaped_value(&sentence[words[to_at].2..]) {
        push_hit(turn, quote, "to_value", key, value, hits);
    }
}

fn word_at(words: &[(String, usize, usize)], target: &str) -> Option<usize> {
    words.iter().position(|(word, _, _)| word == target)
}

fn adjacent(words: &[(String, usize, usize)], left: &str, right: &str) -> Option<usize> {
    words
        .windows(2)
        .position(|pair| pair[0].0 == left && pair[1].0 == right)
}

fn setting_verb(words: &[(String, usize, usize)]) -> Option<usize> {
    words
        .iter()
        .position(|(word, _, _)| SETTING_VERBS.contains(&word.as_str()))
}

fn to_within(words: &[(String, usize, usize)], verb: usize) -> Option<usize> {
    let end = (verb + 4).min(words.len());
    (verb + 1..end).find(|index| words[*index].0 == "to")
}

fn head_before(sentence: &str, start: usize) -> String {
    trim_articles(sentence[..start].trim())
}

fn head_between(sentence: &str, from: usize, to: usize) -> String {
    if to < from {
        return String::new();
    }
    trim_articles(sentence[from..to].trim())
}

fn trim_articles(text: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping = true;
    for part in text.split_whitespace() {
        if skipping && is_article(part) {
            continue;
        }
        skipping = false;
        kept.push(part);
    }
    kept.join(" ")
}

fn is_article(word: &str) -> bool {
    let folded = word.to_ascii_lowercase();
    matches!(folded.as_str(), "the" | "a" | "an")
}

fn from_to_benchmark(sentence: &str) -> bool {
    let words = cue_words(sentence);
    let Some(from) = word_at(&words, "from") else {
        return false;
    };
    let Some(to) = words.iter().rposition(|(word, _, _)| word == "to") else {
        return false;
    };
    to > from && digit_word(&words, from + 1) && digit_word(&words, to + 1)
}

fn digit_word(words: &[(String, usize, usize)], index: usize) -> bool {
    words
        .get(index)
        .is_some_and(|(word, _, _)| word.chars().all(|ch| ch.is_ascii_digit()) && !word.is_empty())
}

fn shaped_value(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    if let Some(inner) = take_tick(rest) {
        return kept_verb(inner, true);
    }
    take_plain(rest)
}

fn take_tick(rest: &str) -> Option<&str> {
    let rest = rest.strip_prefix('`')?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

fn take_plain(rest: &str) -> Option<String> {
    if let Some(value) = take_version(rest) {
        return kept_verb(&value, false);
    }
    if let Some(value) = take_number(rest) {
        return kept_verb(&value, false);
    }
    take_token_value(rest).and_then(|value| kept_verb(&value, false))
}

fn kept_verb(value: &str, backticked: bool) -> Option<String> {
    accept_verb_value(value, backticked).then(|| value.to_string())
}

fn take_version(rest: &str) -> Option<String> {
    let bytes = rest.as_bytes();
    let mut index = version_prefix(bytes);
    let start = index;
    index = digits_end(bytes, index);
    if index == start || !dot_at(bytes, index) {
        return None;
    }
    index += 1;
    let minor = index;
    index = digits_end(bytes, index);
    if index == minor {
        return None;
    }
    index = optional_patch(bytes, index);
    Some(rest[..index].to_string())
}

fn version_prefix(bytes: &[u8]) -> usize {
    match bytes.first() {
        Some(b'v' | b'V') => 1,
        _ => 0,
    }
}

fn dot_at(bytes: &[u8], index: usize) -> bool {
    index < bytes.len() && bytes[index] == b'.'
}

fn optional_patch(bytes: &[u8], index: usize) -> usize {
    if !dot_at(bytes, index) {
        return index;
    }
    let after = digits_end(bytes, index + 1);
    if after > index + 1 {
        after
    } else {
        index
    }
}

fn digits_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    index
}

fn take_number(rest: &str) -> Option<String> {
    let end = number_end(rest)?;
    if end == 0 {
        return None;
    }
    let end = percent_or_unit(rest.as_bytes(), end);
    Some(rest[..end].to_string())
}

fn number_end(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let index = digits_end(bytes, 0);
    if index == 0 {
        return None;
    }
    Some(with_fraction(bytes, index))
}

fn with_fraction(bytes: &[u8], index: usize) -> usize {
    if !dot_at(bytes, index) {
        return index;
    }
    let after = digits_end(bytes, index + 1);
    if after > index + 1 {
        after
    } else {
        index
    }
}

fn percent_or_unit(bytes: &[u8], end: usize) -> usize {
    if end < bytes.len() && bytes[end] == b'%' {
        return end + 1;
    }
    extend_unit(bytes, end)
}

fn extend_unit(bytes: &[u8], end: usize) -> usize {
    let Some(start) = unit_offset(bytes, end) else {
        return end;
    };
    let stop = ident_end(bytes, start);
    if stop > start && stop - start <= 12 {
        stop
    } else {
        end
    }
}

fn unit_offset(bytes: &[u8], end: usize) -> Option<usize> {
    if end >= bytes.len() {
        return None;
    }
    if bytes[end].is_ascii_alphabetic() {
        return Some(end);
    }
    if bytes[end] == b' ' && end + 1 < bytes.len() && bytes[end + 1].is_ascii_alphabetic() {
        return Some(end + 1);
    }
    None
}

fn take_token_value(rest: &str) -> Option<String> {
    let token = rest.split_whitespace().next()?.trim_matches(trim_mark);
    if token.is_empty() {
        return None;
    }
    if is_path(token) || dotted_ident(token, false) {
        return Some(token.to_string());
    }
    None
}

fn trim_mark(ch: char) -> bool {
    matches!(ch, ',' | ';' | ':' | ')' | '(')
}

fn accept_verb_value(value: &str, backticked: bool) -> bool {
    if rejected_value(value) {
        return false;
    }
    let len = value.chars().count();
    if len == 0 || len > 120 {
        return false;
    }
    if backticked {
        return len > 1;
    }
    number_value(value) || version_value(value) || is_path(value) || dotted_ident(value, false)
}

fn kept_assign(value: &str) -> bool {
    !rejected_value(value) && within_assign_len(value)
}

fn within_assign_len(value: &str) -> bool {
    let len = value.chars().count();
    (1..=120).contains(&len) && !single_letter(value)
}

fn rejected_value(value: &str) -> bool {
    let folded = value.to_ascii_lowercase();
    PLACEHOLDERS.contains(&folded.as_str()) || angle_placeholder(value)
}

fn angle_placeholder(value: &str) -> bool {
    value.starts_with('<') && value.ends_with('>') && value.chars().count() >= 2
}

fn single_letter(value: &str) -> bool {
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => !ch.is_ascii_digit(),
        _ => false,
    }
}

fn number_value(value: &str) -> bool {
    take_number(value).is_some_and(|parsed| parsed == value)
}

fn version_value(value: &str) -> bool {
    take_version(value).is_some_and(|parsed| parsed == value)
}

fn is_path(span: &str) -> bool {
    !span.contains(' ') && (span.contains('/') || span.contains('\\') || ends_with_extension(span))
}

fn ends_with_extension(span: &str) -> bool {
    let Some((base, ext)) = span.rsplit_once('.') else {
        return false;
    };
    !base.is_empty() && extension_chars(ext) && base.chars().any(|ch| ch.is_ascii_alphanumeric())
}

fn extension_chars(ext: &str) -> bool {
    let count = ext.chars().count();
    (1..=8).contains(&count) && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
}

fn snake_or_camel(span: &str) -> bool {
    snake_parts(span) || camel_hump(span)
}

fn snake_parts(span: &str) -> bool {
    if !span.contains('_') {
        return false;
    }
    let parts: Vec<&str> = span.split('_').collect();
    parts.len() >= 2 && parts.iter().all(|part| part_ok(part))
}

fn camel_hump(span: &str) -> bool {
    if !span.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return false;
    }
    let mut parts = 0usize;
    let mut prev_lower = false;
    let mut current = 0usize;
    for ch in span.chars() {
        if ch.is_ascii_uppercase() && prev_lower {
            parts += 1;
            current = 1;
        } else {
            current += 1;
        }
        prev_lower = ch.is_ascii_lowercase();
    }
    if current > 0 {
        parts += 1;
    }
    parts >= 2
}

fn is_command(span: &str) -> bool {
    let mut pieces = span.split_whitespace();
    let Some(head) = pieces.next() else {
        return false;
    };
    let Some(flag) = pieces.next() else {
        return false;
    };
    is_key(head) && flag_option(flag)
}

fn flag_option(flag: &str) -> bool {
    if let Some(body) = flag.strip_prefix("--") {
        return alpha_start(body);
    }
    if let Some(body) = flag.strip_prefix('-') {
        return alpha_start(body);
    }
    false
}

fn alpha_start(body: &str) -> bool {
    body.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric())
}

fn is_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
}

fn push_hit(
    turn: &Turn,
    quote: &str,
    pattern: &'static str,
    key: String,
    value: String,
    hits: &mut Vec<Hit>,
) {
    if value.is_empty() {
        return;
    }
    hits.push(Hit {
        turn: turn.index,
        role: role_name(turn.role),
        pattern,
        key,
        value,
        quote: quote.to_string(),
    });
}

fn role_name(role: TurnRole) -> &'static str {
    match role {
        TurnRole::User => "user",
        TurnRole::Assistant => "assistant",
    }
}

fn classify(hits: &[Hit], live: &[LiveRow]) -> RecoveredReport {
    let mut report = RecoveredReport::default();
    let mut listed = Vec::new();
    let mut per_turn = Vec::new();
    for hit in hits {
        note_hit(hit, live, &mut listed, &mut per_turn, &mut report);
    }
    report
}

fn note_hit(
    hit: &Hit,
    live: &[LiveRow],
    listed: &mut Vec<String>,
    per_turn: &mut Vec<(usize, usize)>,
    report: &mut RecoveredReport,
) {
    if listed.iter().any(|value| values_equal(value, &hit.value)) {
        return;
    }
    if live.iter().any(|row| values_equal(&row.value, &hit.value)) {
        report.redundant = report.redundant.saturating_add(1);
        return;
    }
    if at_cap(per_turn, hit.turn, report.items.len()) {
        report.overflow = report.overflow.saturating_add(1);
        return;
    }
    bump_turn(per_turn, hit.turn);
    listed.push(hit.value.clone());
    report.items.push(hit.item(nearest(live, hit)));
}

fn at_cap(per_turn: &[(usize, usize)], turn: usize, session: usize) -> bool {
    session >= SESSION_CAP || turn_count(per_turn, turn) >= TURN_CAP
}

fn turn_count(per_turn: &[(usize, usize)], turn: usize) -> usize {
    per_turn
        .iter()
        .find(|(index, _)| *index == turn)
        .map(|(_, count)| *count)
        .unwrap_or(0)
}

fn bump_turn(per_turn: &mut Vec<(usize, usize)>, turn: usize) {
    if let Some((_, count)) = per_turn.iter_mut().find(|(index, _)| *index == turn) {
        *count += 1;
        return;
    }
    per_turn.push((turn, 1));
}

fn nearest(rows: &[LiveRow], hit: &Hit) -> Option<RecoveredNearest> {
    let tokens = match_tokens(&hit.key, &hit.value);
    if tokens.is_empty() {
        return None;
    }
    best_row(rows, &tokens)
}

fn match_tokens(key: &str, value: &str) -> Vec<String> {
    if key.trim().is_empty() {
        key_tokens(value)
    } else {
        key_tokens(key)
    }
}

fn key_tokens(text: &str) -> Vec<String> {
    alphanumeric_runs(text)
        .into_iter()
        .filter(|token| token.chars().count() >= 2)
        .collect()
}

fn best_row(rows: &[LiveRow], tokens: &[String]) -> Option<RecoveredNearest> {
    let mut best = 0usize;
    let mut winner = None;
    let mut tied = false;
    for (index, row) in rows.iter().enumerate() {
        let score = shared(tokens, &key_tokens(&row.key));
        if score > best {
            best = score;
            winner = Some(index);
            tied = false;
        } else if score == best && score > 0 {
            tied = true;
        }
    }
    if tied || winner.is_none() {
        return None;
    }
    winner.map(|index| nearest_of(&rows[index]))
}

fn shared(left: &[String], right: &[String]) -> usize {
    left.iter()
        .filter(|token| right.iter().any(|other| other == *token))
        .count()
}

fn nearest_of(row: &LiveRow) -> RecoveredNearest {
    RecoveredNearest {
        subject: row.subject.clone(),
        key: row.key.clone(),
        value: row.value.clone(),
    }
}

impl Hit {
    fn item(&self, nearest: Option<RecoveredNearest>) -> RecoveredItem {
        RecoveredItem {
            turn: self.turn,
            role: self.role.to_string(),
            pattern: self.pattern.to_string(),
            key: self.key.clone(),
            value: self.value.clone(),
            quote: self.quote.clone(),
            nearest,
        }
    }
}

fn neighbourhood(project: &ProjectIdentity, facts: &[Fact]) -> Vec<String> {
    let mut names = BTreeSet::new();
    insert_name(&mut names, &project.name);
    for alias in &project.aliases {
        insert_name(&mut names, alias);
    }
    for fact in facts {
        insert_name(&mut names, &fact.subject_name);
    }
    names.into_iter().take(NAME_CAP).collect()
}

fn insert_name(names: &mut BTreeSet<String>, name: &str) {
    if !name.is_empty() {
        names.insert(name.to_string());
    }
}

async fn load_live(gm: &GraphMemory, names: &[String]) -> Result<Vec<LiveRow>, GraphError> {
    if take_forced_failure() {
        return Err(GraphError::Parse(READ_ERROR.into()));
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut response = gm
        .db()
        .query(LIVE_SQL)
        .bind(("names", names.to_vec()))
        .await?;
    let values: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(values.iter().flat_map(rows_of).collect())
}

fn take_forced_failure() -> bool {
    #[cfg(test)]
    {
        FAIL_READ.with(|cell| {
            let fail = cell.get();
            if fail {
                cell.set(false);
            }
            fail
        })
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn rows_of(value: &Value) -> Vec<LiveRow> {
    let mut rows = Vec::new();
    let Some(subject) = value.get("name").and_then(Value::as_str) else {
        return rows;
    };
    if subject.is_empty() {
        return rows;
    }
    push_slot_rows(subject, value.get("slots"), &mut rows);
    push_edge_rows(subject, value, &mut rows);
    rows
}

fn push_slot_rows(subject: &str, slots: Option<&Value>, rows: &mut Vec<LiveRow>) {
    let Some(slots) = slots.and_then(Value::as_array) else {
        return;
    };
    for slot in slots {
        if let Some(row) = live_slot(subject, slot) {
            rows.push(row);
        }
    }
}

fn live_slot(subject: &str, slot: &Value) -> Option<LiveRow> {
    if !slot_is_live(slot) {
        return None;
    }
    let value = slot.get("value").and_then(Value::as_str)?;
    if value.is_empty() {
        return None;
    }
    Some(LiveRow {
        subject: subject.to_string(),
        key: slot
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        value: value.to_string(),
    })
}

fn slot_is_live(slot: &Value) -> bool {
    slot_open(slot) && !pending_object(slot) && slot_status_live(slot)
}

fn slot_open(slot: &Value) -> bool {
    slot.get("valid_until").is_none_or(Value::is_null)
}

fn pending_object(slot: &Value) -> bool {
    slot.get("pending").is_some_and(Value::is_object)
}

fn slot_status_live(slot: &Value) -> bool {
    matches!(
        slot.get("status").and_then(Value::as_str),
        Some("decided" | "considered")
    )
}

fn push_edge_rows(subject: &str, value: &Value, rows: &mut Vec<LiveRow>) {
    let Some(objects) = value.get("edge_objects").and_then(Value::as_array) else {
        return;
    };
    let empty = Vec::new();
    let keys = value
        .get("edge_keys")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for (index, object) in objects.iter().enumerate() {
        push_edge(subject, keys.get(index), object, rows);
    }
}

fn push_edge(subject: &str, key: Option<&Value>, object: &Value, rows: &mut Vec<LiveRow>) {
    let Some(object) = json_name(object) else {
        return;
    };
    if object.is_empty() {
        return;
    }
    rows.push(LiveRow {
        subject: subject.to_string(),
        key: key.and_then(json_name).unwrap_or_default(),
        value: object,
    });
}

fn json_name(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::RecoveredReport;

    fn user(index: usize, text: &str) -> Turn {
        Turn {
            index,
            role: TurnRole::User,
            text: text.to_string(),
        }
    }

    fn assistant(index: usize, text: &str) -> Turn {
        Turn {
            index,
            role: TurnRole::Assistant,
            text: text.to_string(),
        }
    }

    fn scan(turns: &[Turn]) -> RecoveredReport {
        classify(&collect_hits(turns), &[])
    }

    fn one(text: &str) -> RecoveredReport {
        scan(&[user(0, text)])
    }

    fn item<'a>(report: &'a RecoveredReport, pattern: &str) -> &'a RecoveredItem {
        report
            .items
            .iter()
            .find(|item| item.pattern == pattern)
            .unwrap_or_else(|| panic!("missing {pattern} in {report:?}"))
    }

    #[test]
    fn v1_patterns_keep_the_key_value_and_the_parenthetical() {
        let license = one("Set `license = \"MIT OR Apache-2.0\"`.");
        let hit = item(&license, "backtick_assign");
        assert_eq!(hit.key, "license");
        assert_eq!(hit.value, "MIT OR Apache-2.0");
        assert_eq!(hit.role, "user");
        assert_eq!(hit.turn, 0);

        let outside = one("Use `license` = \"MIT OR Apache-2.0\".");
        let hit = item(&outside, "backtick_assign");
        assert_eq!(hit.key, "license");
        assert_eq!(hit.value, "MIT OR Apache-2.0");

        let span = one("Run `scripts/migrate.py` (12 rows, 3 tags).");
        let hit = item(&span, "backtick_span");
        assert_eq!(hit.value, "scripts/migrate.py");
        assert!(hit.quote.contains("(12 rows, 3 tags)"), "{}", hit.quote);
        assert!(hit.key.is_empty());

        let dotted = one("Call it with httpx.AsyncClient and a semaphore.");
        let hit = item(&dotted, "dotted_ident");
        assert_eq!(hit.value, "httpx.AsyncClient");

        let now = one("Default jobs is now 4.");
        let hit = item(&now, "is_now_value");
        assert_eq!(hit.key, "Default jobs");
        assert_eq!(hit.value, "4");

        let becomes = one("Header becomes `X-Sig: v2`.");
        let hit = item(&becomes, "becomes_value");
        assert_eq!(hit.key, "Header");
        assert_eq!(hit.value, "X-Sig: v2");

        let switched = one("We switched the limit to 8.");
        let hit = item(&switched, "to_value");
        assert_eq!(hit.key, "limit");
        assert_eq!(hit.value, "8");
    }

    #[test]
    fn guards_drop_questions_hedges_benchmarks_and_placeholders() {
        let samples = [
            "Coverage is 88%.",
            "It is now faster.",
            "The port is now ready.",
            "Is the port now 4?",
            "Maybe set it to 4.",
            "It went from 41 s to 9.8 s.",
            "Write it to `<path>`.",
            "See github.com for the host.",
            "See e.g. the note.",
            "now faster",
        ];
        for sample in samples {
            let report = one(sample);
            assert!(
                report.items.is_empty(),
                "{sample} listed {:?}",
                report.items
            );
            assert_eq!(report.overflow, 0, "{sample}");
            assert!(report.is_blank(), "{sample}");
        }
    }

    #[test]
    fn caps_count_overflow_and_the_first_value_wins() {
        let turn = user(
            3,
            "`scripts/aa.py` `scripts/bb.py` `scripts/cc.py` `scripts/dd.py` `scripts/ee.py` `scripts/ff.py`",
        );
        let report = scan(&[turn]);
        assert_eq!(report.items.len(), 4);
        assert_eq!(report.overflow, 2);
        assert!(report.items.iter().all(|item| item.turn == 3));

        let mut turns = Vec::new();
        for index in 0..5 {
            turns.push(user(
                index,
                &format!("`scripts/a{index}.py` `scripts/b{index}.py` `scripts/c{index}.py` `scripts/d{index}.py`"),
            ));
        }
        let report = scan(&turns);
        assert_eq!(report.items.len(), 16);
        assert_eq!(report.overflow, 4);

        let repeated = scan(&[
            user(0, "Default jobs is now 4."),
            assistant(1, "Default jobs is now 4."),
        ]);
        assert_eq!(repeated.items.len(), 1);
        assert_eq!(repeated.items[0].role, "user");
        assert_eq!(repeated.overflow, 0);
    }

    #[test]
    fn an_assistant_turn_is_scanned_only_after_a_user_turn() {
        let report = scan(&[
            assistant(0, "Default jobs is now 4."),
            user(1, "ok"),
            assistant(2, "Default jobs is now 9."),
            assistant(3, "Default jobs is now 8."),
        ]);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].turn, 2);
        assert_eq!(report.items[0].value, "9");
        assert_eq!(report.items[0].role, "assistant");
    }

    #[test]
    fn a_stored_value_is_redundant_and_nearest_breaks_ties_by_absence() {
        let hits = collect_hits(&[user(0, "Default jobs is now 4. See `scripts/migrate.py`.")]);
        let live = vec![
            LiveRow {
                subject: "tidepool".into(),
                key: "jobs".into(),
                value: "4".into(),
            },
            LiveRow {
                subject: "other".into(),
                key: "jobs".into(),
                value: "1".into(),
            },
        ];
        let report = classify(&hits, &live);
        assert_eq!(report.redundant, 1);
        assert!(report.items.iter().all(|item| item.value != "4"));
        let span = item(&report, "backtick_span");
        assert!(span.nearest.is_none(), "{span:?}");

        let only = classify(
            &collect_hits(&[user(0, "Run `scripts/migrate.py`.")]),
            &[LiveRow {
                subject: "tidepool".into(),
                key: "migrate script".into(),
                value: "old".into(),
            }],
        );
        let span = item(&only, "backtick_span");
        let nearest = span.nearest.as_ref().expect("nearest");
        assert_eq!(nearest.subject, "tidepool");
        assert_eq!(nearest.key, "migrate script");
        assert_eq!(nearest.value, "old");

        let tied = classify(
            &collect_hits(&[user(0, "Run `scripts/migrate.py`.")]),
            &[
                LiveRow {
                    subject: "left".into(),
                    key: "migrate".into(),
                    value: "a".into(),
                },
                LiveRow {
                    subject: "right".into(),
                    key: "migrate".into(),
                    value: "b".into(),
                },
            ],
        );
        assert!(item(&tied, "backtick_span").nearest.is_none());
    }

    #[test]
    fn an_edge_object_counts_as_redundant() {
        let hits = collect_hits(&[user(0, "Use `scripts/migrate.py`.")]);
        let live = vec![LiveRow {
            subject: "tidepool".into(),
            key: "uses".into(),
            value: "scripts/migrate.py".into(),
        }];
        let report = classify(&hits, &live);
        assert!(report.items.is_empty());
        assert_eq!(report.redundant, 1);
        assert!(!report.is_blank());
    }

    #[test]
    fn a_quote_stops_at_200_characters() {
        let long = format!("Default jobs is now 4. {}", "word ".repeat(80));
        let report = one(&long);
        let hit = item(&report, "is_now_value");
        assert!(
            hit.quote.chars().count() <= 200,
            "{}",
            hit.quote.chars().count()
        );
    }
}

#[cfg(all(test, feature = "embedded"))]
mod store_tests {
    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::graph::llm::{Completion, LlmProvider};
    use crate::graph::v2::pass1::extract_session;
    use crate::graph::IngestContext;

    const TURN: &str =
        "Default jobs is now 4. The helper is `scripts/migrate.py` (12 rows, 3 tags).";
    const REPLY: &str = r#"{"facts":[{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"jobs","object":"4","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Default jobs is now 4.","confidence":0.9}],"bugs":[],"releases":[]}"#;

    struct Scripted {
        reply: &'static str,
    }

    #[async_trait]
    impl LlmProvider for Scripted {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Ok(r#"{"decision":"create","reason":"distinct"}"#.into())
        }

        async fn complete_structured_chat(
            &self,
            _: &str,
            _: &[crate::graph::llm::Message],
            _: &Value,
            _: u32,
        ) -> Result<Completion, GraphError> {
            Ok(Completion::unmeasured(self.reply))
        }
    }

    fn plant(memory: &std::path::Path, recover: bool) {
        let cwd = memory.join("tidepool");
        std::fs::create_dir_all(&cwd).unwrap();
        crate::graph::v2::input::write_turns_sidecar(
            memory,
            1,
            "s",
            "2026-01-02",
            Some(cwd.to_str().unwrap()),
            &[crate::conversation::ConversationEntry::UserMessage(
                TURN.into(),
            )],
        )
        .unwrap();
        let line = if recover {
            "recover_turns = true\n"
        } else {
            ""
        };
        std::fs::write(memory.join(".recall-echo.toml"), format!("[graph]\n{line}")).unwrap();
    }

    async fn open_and_extract(
        recover: bool,
    ) -> (
        tempfile::TempDir,
        GraphMemory,
        crate::graph::types::IngestionReport,
    ) {
        let dir = tempfile::tempdir().unwrap();
        plant(dir.path(), recover);
        let gm = GraphMemory::open_embedded(&dir.path().join("graph"))
            .await
            .unwrap();
        let provider = Scripted { reply: REPLY };
        let report = extract_session(
            &gm,
            dir.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        (dir, gm, report)
    }

    async fn fact_rows(gm: &GraphMemory) -> Vec<String> {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let mut rows = Vec::new();
        for entity in &entities {
            push_slot_lines(&entity.name, entity.attributes.as_ref(), &mut rows);
        }
        let edges = crate::graph::crud::list_all_relationships(gm.db())
            .await
            .unwrap();
        for edge in edges {
            rows.push(format!(
                "edge|{}|{}",
                edge.rel_type,
                edge.valid_until.is_none()
            ));
        }
        rows.sort();
        rows
    }

    fn push_slot_lines(name: &str, attributes: Option<&Value>, rows: &mut Vec<String>) {
        let Some(slots) = attributes
            .and_then(|attrs| attrs.pointer("/v2/slots"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for slot in slots {
            rows.push(format!(
                "slot|{name}|{}|{}|{}|{}",
                slot.get("predicate").and_then(Value::as_str).unwrap_or(""),
                slot.get("key").and_then(Value::as_str).unwrap_or(""),
                slot.get("value").and_then(Value::as_str).unwrap_or(""),
                slot.get("status").and_then(Value::as_str).unwrap_or(""),
            ));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flag_on_lists_misses_and_leaves_the_store_matching_flag_off() {
        let (_off_dir, off_gm, off) = open_and_extract(false).await;
        let (_on_dir, on_gm, on) = open_and_extract(true).await;
        assert!(off.errors.is_empty(), "{:?}", off.errors);
        assert!(on.errors.is_empty(), "{:?}", on.errors);
        assert_eq!(fact_rows(&off_gm).await, fact_rows(&on_gm).await);
        assert_eq!(off.candidates, on.candidates);
        assert_eq!(off.kept, on.kept);
        assert_eq!(off.slots_written, on.slots_written);
        assert_eq!(off.drops, on.drops);
        assert!(off.recovered.is_blank());
        assert!(serde_json::to_value(&off)
            .unwrap()
            .get("recovered")
            .is_none());

        let span = on
            .recovered
            .items
            .iter()
            .find(|item| item.pattern == "backtick_span")
            .expect("span");
        assert_eq!(span.value, "scripts/migrate.py");
        assert!(span.quote.contains("(12 rows, 3 tags)"), "{}", span.quote);
        assert!(on.recovered.redundant >= 1, "{:?}", on.recovered);
        assert!(on.recovered.items.iter().all(|item| item.value != "4"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_store_read_lists_nothing_and_the_session_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        plant(dir.path(), true);
        let gm = GraphMemory::open_embedded(&dir.path().join("graph"))
            .await
            .unwrap();
        let provider = Scripted { reply: REPLY };
        fail_next_read();
        let report = extract_session(
            &gm,
            dir.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert_eq!(report.errors, vec![READ_ERROR.to_string()]);
        assert!(report.recovered.is_blank());
        assert!(report.slots_written >= 1, "{report:?}");
        let rows = fact_rows(&gm).await;
        assert!(rows.iter().any(|row| row.contains("|jobs|4|")), "{rows:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_live_edge_object_is_read_with_the_slot_values() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("tidepool");
        std::fs::create_dir_all(&cwd).unwrap();
        let turn = "The helper is `scripts/migrate.py`.";
        crate::graph::v2::input::write_turns_sidecar(
            dir.path(),
            1,
            "s",
            "2026-01-02",
            Some(cwd.to_str().unwrap()),
            &[crate::conversation::ConversationEntry::UserMessage(
                turn.into(),
            )],
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nrecover_turns = true\n",
        )
        .unwrap();
        let gm = GraphMemory::open_embedded(&dir.path().join("graph"))
            .await
            .unwrap();
        let reply = r#"{"facts":[{"subject":"tidepool","subject_type":"project","predicate":"uses","role":"none","key":"","object":"scripts/migrate.py","object_type":"technology","status":"decided","replaces":"","turn":0,"quote":"The helper is `scripts/migrate.py`.","confidence":0.9}],"bugs":[],"releases":[]}"#;
        let provider = Scripted { reply };
        let report = extract_session(
            &gm,
            dir.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report.recovered.redundant >= 1,
            "drops {:?} recovered {:?}",
            report.drops,
            report.recovered
        );
        assert!(report
            .recovered
            .items
            .iter()
            .all(|item| item.value != "scripts/migrate.py"));
    }
}
