// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A listed value is not a replacement. The writer calls this before retiring.

use super::ground::{numbers_in, sentence_containing, sentences};
use super::known_match::{cue_words, is_negator, run_spans, value_spans, DEPARTURE};
use super::schema::{Fact, Predicate};

const REASON: &str = "enumeration";
const GAP: &[&str] = &["and", "or", "nor", "plus"];

type Run = (String, usize, usize);

/// `value` is one member of a list in `sentence`.
#[must_use]
#[rustfmt::skip]
pub(crate) fn enumerated(value: &str, sentence: &str) -> bool {
    value_spans(sentence, value).into_iter().any(|span| {
        bracketed(sentence, span) || sibling(sentence, value, span) || additive(sentence, span)
    })
}

/// `Some("enumeration")` when a decided value is listed. The quote sentence wins
/// when it mentions the value; otherwise any mentioning sentence does. `issue_status` never is.
#[must_use]
#[rustfmt::skip]
pub(crate) fn enumeration_skip(fact: &Fact, turn: &str) -> Option<&'static str> {
    if fact.status != "decided" || fact.predicate == Predicate::IssueStatus {
        return None;
    }
    let value = fact.object_name.as_str();
    let quoted = sentence_containing(turn, fact.quote.as_str());
    if !value_spans(quoted, value).is_empty() {
        return enumerated(value, quoted).then_some(REASON);
    }
    sentences(turn)
        .any(|sentence| !value_spans(sentence, value).is_empty() && enumerated(value, sentence))
        .then_some(REASON)
}

#[rustfmt::skip]
fn bracketed(sentence: &str, span: (usize, usize)) -> bool {
    let Some((open, close)) = innermost(sentence, span) else { return false };
    let Some(ch) = sentence[open..].chars().next() else { return false };
    let mut depth = 0i32;
    let mut separator = false;
    let mut other = false;
    let mut run: Option<usize> = None;
    let mut index = open + ch.len_utf8();
    while index < close {
        let Some(ch) = sentence[index..].chars().next() else { break };
        let len = ch.len_utf8();
        if depth == 0 && ch.is_alphanumeric() {
            run.get_or_insert(index);
            index += len;
            continue;
        }
        if let Some(from) = run.take() {
            if index <= span.0 || from >= span.1 { other = true; }
            let word = sentence[from..index].to_lowercase();
            if word == "and" || word == "or" { separator = true; }
        }
        match ch {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' | '|' | ';' if depth == 0 => separator = true,
            _ => {}
        }
        index += len;
    }
    if let Some(from) = run.take() {
        if close <= span.0 || from >= span.1 { other = true; }
        let word = sentence[from..close].to_lowercase();
        if word == "and" || word == "or" { separator = true; }
    }
    separator && other
}

#[rustfmt::skip]
fn innermost(sentence: &str, span: (usize, usize)) -> Option<(usize, usize)> {
    let mut stack: Vec<(usize, char)> = Vec::new();
    let mut best: Option<(usize, usize)> = None;
    for (index, ch) in sentence.char_indices() {
        match ch {
            '[' | '(' | '{' => stack.push((index, ch)),
            ']' | ')' | '}' => {
                let Some((open, opener)) = stack.pop() else { continue };
                let paired = matches!((opener, ch), ('[', ']') | ('(', ')') | ('{', '}'));
                if !paired || open >= span.0 || index < span.1 { continue; }
                if best.is_none_or(|(start, end)| open >= start && index <= end) {
                    best = Some((open, index));
                }
            }
            _ => {}
        }
    }
    best
}

#[rustfmt::skip]
fn sibling(sentence: &str, value: &str, span: (usize, usize)) -> bool {
    let runs = run_spans(sentence);
    let items = split_items(&runs, sentence);
    let Some(at) = items.iter().position(|item| {
        item.iter().any(|&i| runs[i].1 < span.1 && runs[i].2 > span.0)
    }) else {
        return false;
    };
    if numbers_in(value).is_empty() {
        return numberless(&runs, &items, at, span, sentence);
    }
    // The gap must touch the span: only a repeated label may sit between them.
    let own = &items[at];
    let first = own.iter().position(|&i| runs[i].2 > span.0).unwrap_or(0);
    let last = own.iter().rposition(|&i| runs[i].1 < span.1).unwrap_or(0);
    let label = |item: &[usize]| {
        first >= 1
            && item.len() >= 2
            && runs[item[0]].0 == runs[own[first - 1]].0
            && digit(&runs[item[1]].0)
    };
    [-1i8, 1].into_iter().any(|dir| {
        let Some(next) = step(at, dir, items.len()) else { return false };
        let item = &items[next];
        if dir < 0 {
            (first == 0 && digit(&runs[item[item.len() - 1]].0)) || (first == 1 && label(item))
        } else {
            last + 1 == own.len() && (digit(&runs[item[0]].0) || label(item))
        }
    })
}

#[rustfmt::skip]
fn numberless(runs: &[Run], items: &[Vec<usize>], at: usize, span: (usize, usize), sentence: &str) -> bool {
    let anchor = &items[at];
    let dir = if runs[anchor[anchor.len() / 2]].1 >= span.1 { 1 } else { -1 };
    let own = cut(anchor, dir);
    if own.iter().any(|&index| bad(&runs[index].0)) {
        return false;
    }
    let extra = if anchor.len() > 3 { 0 } else { extend(runs, items, at, -1) + extend(runs, items, at, 1) };
    let count = 1 + extra;
    count >= 3 || (count == 2 && run_spans(sentence).iter().any(|(token, _, _)| cue(token)))
}

#[rustfmt::skip]
fn extend(runs: &[Run], items: &[Vec<usize>], mut pos: usize, dir: i8) -> usize {
    let mut added = 0usize;
    while let Some(next) = step(pos, dir, items.len()) {
        let item = &items[next];
        let checked = cut(item, dir);
        if checked.is_empty() || checked.iter().any(|&index| bad(&runs[index].0)) {
            break;
        }
        added += 1;
        if item.len() > 3 { break; }
        pos = next;
    }
    added
}

#[rustfmt::skip]
fn split_items(runs: &[Run], sentence: &str) -> Vec<Vec<usize>> {
    let mut items = Vec::new();
    let mut current = Vec::new();
    let mut index = 0;
    while index < runs.len() {
        if !current.is_empty() {
            if let Some(next) = gap_at(runs, sentence, index) {
                items.push(std::mem::take(&mut current));
                index = next;
                continue;
            }
        }
        current.push(index);
        index += 1;
    }
    if !current.is_empty() { items.push(current); }
    items
}

#[rustfmt::skip]
fn gap_at(runs: &[Run], sentence: &str, index: usize) -> Option<usize> {
    let prev = index - 1;
    let text = &sentence[runs[prev].2..runs[index].1];
    if !between_ok(text, endch(&runs[prev].0), startch(&runs[index].0)) { return None; }
    if !gap_word(&runs[index].0) { return marks(text).then_some(index); }
    let mut cursor = index;
    while cursor < runs.len() && gap_word(&runs[cursor].0) {
        if cursor > index && !between_ok(&sentence[runs[cursor - 1].2..runs[cursor].1], endch(&runs[cursor - 1].0), startch(&runs[cursor].0)) { return None; }
        cursor += 1;
    }
    if cursor < runs.len() && !between_ok(&sentence[runs[cursor - 1].2..runs[cursor].1], endch(&runs[cursor - 1].0), startch(&runs[cursor].0)) { return None; }
    Some(cursor)
}

#[rustfmt::skip]
fn additive(sentence: &str, span: (usize, usize)) -> bool {
    // A departure, or `to`, already names a change. The cue is not a list.
    if cue_words(sentence).iter().any(|(word, _, end)| {
        *end <= span.0 && (word == "to" || DEPARTURE.contains(&word.as_str()))
    }) {
        return false;
    }
    let words = cue_words(sentence);
    let before: Vec<&str> = words.iter().filter(|(_, _, end)| *end <= span.0).map(|(w, _, _)| w.as_str()).collect();
    let after: Vec<&str> = words.iter().filter(|(_, start, _)| *start >= span.1).map(|(w, _, _)| w.as_str()).take(3).collect();
    let from = before.len().saturating_sub(3);
    window(&before[from..]) || window(&after)
}

#[rustfmt::skip]
fn window(tokens: &[&str]) -> bool {
    tokens.iter().any(|token| matches!(*token, "also" | "too" | "additionally"))
        || tokens.windows(2).any(|pair| pair == ["as", "well"] || pair == ["in", "addition"])
}

#[rustfmt::skip]
fn cut(item: &[usize], dir: i8) -> Vec<usize> {
    if item.len() <= 3 { return item.to_vec(); }
    if dir < 0 { item[item.len() - 3..].to_vec() } else { item[..3].to_vec() }
}

#[rustfmt::skip]
fn step(pos: usize, dir: i8, len: usize) -> Option<usize> {
    match dir {
        -1 if pos > 0 => Some(pos - 1),
        1 if pos + 1 < len => Some(pos + 1),
        _ => None,
    }
}

#[rustfmt::skip]
fn between_ok(text: &str, left: Option<char>, right: Option<char>) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.iter().copied().enumerate().all(|(index, ch)| match ch {
        c if c.is_whitespace() => true,
        ',' | '|' | '&' | '+' => true,
        '/' => slash_ok(&chars, index, left, right),
        _ => false,
    })
}

#[rustfmt::skip]
fn slash_ok(chars: &[char], index: usize, left_bound: Option<char>, right_bound: Option<char>) -> bool {
    let left_ws = index > 0 && chars[index - 1].is_whitespace();
    let right_ws = index + 1 < chars.len() && chars[index + 1].is_whitespace();
    if left_ws && right_ws { return true; }
    let left = if index == 0 { left_bound } else { Some(chars[index - 1]) };
    let right = if index + 1 == chars.len() { right_bound } else { Some(chars[index + 1]) };
    (left.is_some_and(char::is_whitespace) && right.is_some_and(char::is_whitespace))
        || (left.is_some_and(|ch| ch.is_ascii_digit()) && right.is_some_and(|ch| ch.is_ascii_digit()))
}

#[rustfmt::skip]
fn marks(text: &str) -> bool { text.chars().any(|ch| matches!(ch, ',' | '|' | '&' | '+' | '/')) }
#[rustfmt::skip]
fn gap_word(token: &str) -> bool { GAP.contains(&token) }
#[rustfmt::skip]
fn digit(token: &str) -> bool { token.chars().any(|ch| ch.is_ascii_digit()) }
#[rustfmt::skip]
fn bad(token: &str) -> bool { token == "not" || is_negator(token) || DEPARTURE.contains(&token) }
#[rustfmt::skip]
fn endch(token: &str) -> Option<char> { token.chars().next_back() }
#[rustfmt::skip]
fn startch(token: &str) -> Option<char> { token.chars().next() }

#[rustfmt::skip]
fn cue(token: &str) -> bool {
    matches!(
        token,
        "matrix" | "versions" | "variants" | "targets" | "tested" | "test" | "tests" | "across"
            | "each" | "every" | "all" | "supported" | "supports" | "both" | "either"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::confidence::Provenance;
    use crate::graph::v2::schema::Kind;

    fn fact(value: &str, quote: &str, predicate: Predicate) -> Fact {
        Fact {
            subject_name: "app".into(),
            subject_kind: Kind::Project,
            predicate,
            role: None,
            key: "runtime_version".into(),
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

    #[test]
    fn bracket_lists_need_a_separator_and_another_word() {
        assert!(enumerated("4.3", "runtime: [4.1, 4.2, 4.3]"));
        assert!(enumerated("4.3", "runtime: [4.1; 4.2; 4.3]"));
        assert!(!enumerated("4.2", "(was 4.2) now 4.3"));
        assert!(!enumerated("4.3", "(was 4.2) now 4.3"));
        assert!(!enumerated("4.3", "runtime: [4.3]"));
    }

    #[test]
    fn numbered_siblings_stop_at_a_real_replacement() {
        assert!(enumerated("4.3", "Test on runtime 4.2 and 4.3."));
        assert!(enumerated("4.3", "CI runs runtime 4.3 and 4.4."));
        assert!(enumerated("4.3", "runtime 4.2 and runtime 4.3"));
        assert!(enumerated("4.3", "Move from 4.1 to 4.2 and 4.3"));
        assert!(enumerated("4.2", "Move from 4.1 to 4.2 and 4.3"));
        assert!(!enumerated("4.3", "Bump the runtime from 4.2 to 4.3."));
        assert!(!enumerated("8100", "Port 8000 -> 8100."));
        assert!(!enumerated("4.3", "Bump to 4.3, and drop 4.2."));
        assert!(!enumerated("4.3", "Also, we now bump the runtime to 4.3."));
        assert!(!enumerated("4.3", "Runtime is 4.2, bump it to 4.3."));
        assert!(!enumerated("4.3", "Switched from 4.1, 4.2 to 4.3."));
        assert!(!enumerated("30", "Set retries to 3, timeout to 30."));
        assert!(enumerated("4.3", "CI runs runtime 4.3 and runtime 4.4."));
        assert!(!enumerated("4.3", "We were on 4.2, now upgrading to 4.3."));
        assert!(!enumerated("4.3", "Runtime was 4.2, now 4.3."));
        assert!(!enumerated("8100", "Port was 8000, use 8100 now."));
        assert!(!enumerated("4.3", "Date 10/11, bump to 4.3."));
    }

    #[test]
    fn additive_cues_are_local_to_the_value() {
        assert!(enumerated("4.3", "Keep 4.2 and also test 4.3."));
        assert!(enumerated("4.3", "Keep 4.2, also test 4.3."));
        assert!(enumerated("4.3", "Test on runtime 4.2, also 4.3."));
        assert!(enumerated("4.3", "Also test on 4.3 as well as 4.2."));
        assert!(!enumerated("4.3", "Also, we now bump the runtime to 4.3."));
        assert!(!enumerated("4.3", "Also upgrade to 4.3."));
        assert!(!enumerated("4.3", "We also moved to 4.3."));
        assert!(!enumerated("4.3", "Upgrade to 4.3 too."));
        assert!(!enumerated("LibB", "We also switched to LibB."));
    }

    #[test]
    fn numberless_chains_need_three_items_or_two_and_a_cue() {
        assert!(enumerated("Beta", "Builds run on Alpha, Beta and Gamma."));
        assert!(enumerated("Beta", "Tested on Alpha and Beta."));
        assert!(!enumerated("Alpha", "Use Alpha and drop Delta."));
        assert!(!enumerated("Delta", "Use Alpha and drop Delta."));
        assert!(!enumerated("Alpha", "Use Alpha and Beta."));
        assert!(!enumerated("LibB", "Use LibB instead of LibA."));
        assert!(!enumerated(
            "make check",
            "Switch the test command to make check."
        ));
    }

    #[test]
    fn a_path_slash_between_letters_is_not_a_list() {
        assert!(!enumerated(
            "src/bin/main.rs",
            "Entry point is src/bin/main.rs."
        ));
        assert!(!enumerated("bin", "Entry point is src/bin/main.rs."));
    }

    #[test]
    fn the_quote_sentence_wins_over_a_later_list() {
        let turn = "Bump to 4.3. CI runs 4.2 and 4.3.";
        let bump = fact("4.3", "Bump to 4.3", Predicate::Setting);
        let listed = fact("4.3", "CI runs 4.2 and 4.3", Predicate::Setting);
        assert!(enumeration_skip(&bump, turn).is_none());
        assert_eq!(enumeration_skip(&listed, turn), Some("enumeration"));
    }

    #[test]
    fn issue_status_and_a_considered_fact_are_not_lists() {
        let mut status = fact(
            "4.3",
            "Test on runtime 4.2 and 4.3.",
            Predicate::IssueStatus,
        );
        assert!(enumeration_skip(&status, status.quote.clone().as_str()).is_none());
        status.predicate = Predicate::Setting;
        status.status = "considered".into();
        assert!(enumeration_skip(&status, "Test on runtime 4.2 and 4.3.").is_none());
    }
}
