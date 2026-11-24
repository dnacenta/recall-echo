// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Exact-value checks for derived questions, on two channels.
//!
//! The store channel looks for the answer on a live decided row of the final
//! store, v2 slots included. The recall channel looks for it in the text
//! `recall_query` returns for the question. The first 8,000 characters of
//! that text are the read budget; the whole text, which the server stops at
//! 24,000 characters, is a sidecar. Current-value questions also scan the
//! recall text for stale values, anywhere in the window rather than only in
//! the first result.
//!
//! Matching is literal and ignores case and runs of whitespace. A value must
//! stand as its own span: `8080` does not hit inside `18080`, nor `black`
//! inside `blacklist`, nor `3.12` inside `3.12.1`.

use super::grade::FinalEdge;

/// Characters of the recall render a reader is assumed to see.
pub(crate) const READ_BUDGET_CHARS: usize = 8_000;
/// The MCP result ceiling. A recall render is never longer than this, plus
/// the server's cut notice.
pub(crate) const SIDECAR_CHARS: usize = 24_000;

/// Answer and stale hits over one recall render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RenderHits {
    pub answer: bool,
    pub answer_24k: bool,
    pub stale: bool,
    pub stale_24k: bool,
}

/// Look for `answer` and every `stale` value in the 8k window and the 24k
/// sidecar of `render`. An empty render hits nothing.
pub(crate) fn scan_render(render: &str, answer: &str, stale: &[String]) -> RenderHits {
    let budget = clip_chars(render, READ_BUDGET_CHARS);
    let sidecar = clip_chars(render, SIDECAR_CHARS);
    RenderHits {
        answer: has_value(budget, answer),
        answer_24k: has_value(sidecar, answer),
        stale: has_any(budget, stale),
        stale_24k: has_any(sidecar, stale),
    }
}

/// Whether `answer` stands on a live decided row.
///
/// A row with no status is a v1 edge and counts as decided. Considered,
/// pending, rejected and retired rows do not count.
pub(crate) fn store_hit(rows: &[FinalEdge], answer: &str) -> bool {
    rows.iter().any(|row| {
        row.live
            && !row.pending
            && matches!(row.status.as_deref(), None | Some("decided"))
            && has_value(&row.object, answer)
    })
}

/// The first `max` characters of `text`.
pub(crate) fn clip_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Whether any of `values` stands in `text`.
pub(crate) fn has_any(text: &str, values: &[String]) -> bool {
    values.iter().any(|value| has_value(text, value))
}

/// Whether `value` stands as its own span in `text`.
pub(crate) fn has_value(text: &str, value: &str) -> bool {
    let needle = normalize(value);
    if needle.is_empty() {
        return false;
    }
    let hay = normalize(text);
    let mut from = 0;
    while let Some(found) = hay[from..].find(&needle) {
        let start = from + found;
        let end = start + needle.len();
        if open_before(&hay, start, &needle) && open_after(&hay, end, &needle) {
            return true;
        }
        from = start + hay[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn token_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// A dot or a hyphen continues a token when a token character is on its far
/// side: `3.12.1`, `shelf.db-wal`.
fn joiner(ch: char) -> bool {
    ch == '.' || ch == '-'
}

fn open_before(hay: &str, start: usize, needle: &str) -> bool {
    if !needle.chars().next().is_some_and(token_char) {
        return true;
    }
    open_side(hay[..start].chars().rev())
}

fn open_after(hay: &str, end: usize, needle: &str) -> bool {
    if !needle.chars().next_back().is_some_and(token_char) {
        return true;
    }
    open_side(hay[end..].chars())
}

fn open_side(mut outward: impl Iterator<Item = char>) -> bool {
    match outward.next() {
        None => true,
        Some(ch) if token_char(ch) => false,
        Some(ch) if joiner(ch) => !outward.next().is_some_and(token_char),
        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    /// `body` padded with filler lines until it is at least `chars` long.
    fn padded(body: &str, chars: usize) -> String {
        let mut text = body.to_string();
        while text.chars().count() < chars {
            text.push_str("\n   filler line about nothing in particular");
        }
        text
    }

    fn row(object: &str, live: bool, status: Option<&str>, pending: bool) -> FinalEdge {
        FinalEdge {
            id: format!("edge:{object}"),
            subject: "shelf".into(),
            relation: "setting:port".into(),
            object: object.into(),
            live,
            status: status.map(str::to_string),
            pending,
            retired_in: None,
            matched: None,
            pattern_fact: None,
        }
    }

    #[test]
    fn a_value_matches_as_its_own_span_ignoring_case_and_spacing() {
        assert!(has_value("1. Port [concept] — 8765, matched", "8765"));
        assert!(has_value("We use Ruff for linting.", "ruff"));
        assert!(has_value("ruff's formatter", "ruff"));
        assert!(has_value("listen on 127.0.0.1:8765", "8765"));
        assert!(has_value("move it to\n   shelf.db.", "shelf.db"));
        assert!(has_value("run `just   test` now", "just test"));
        assert!(has_value("Python 3.12, managed with uv", "3.12"));
    }

    #[test]
    fn a_short_generic_value_does_not_hit_inside_a_longer_string() {
        assert!(!has_value("listen on 18080", "8080"));
        assert!(!has_value("port 80801", "8080"));
        assert!(!has_value("the blacklist grew", "black"));
        assert!(!has_value("pinned 3.12.1", "3.12"));
        assert!(!has_value("released v13.12", "3.12"));
        assert!(!has_value("left shelf.db-wal behind", "shelf.db"));
        assert!(!has_value("bookmark_tags_idx", "bookmark_tags"));
        assert!(!has_value("anything", ""));
        assert!(!has_value("", "8080"));
    }

    #[test]
    fn a_later_clean_span_still_hits_after_an_embedded_one() {
        assert!(has_value("18080 then 8080", "8080"));
    }

    #[test]
    fn the_budget_and_the_sidecar_clip_on_characters() {
        let text = "é".repeat(10);
        assert_eq!(clip_chars(&text, 3), "ééé");
        assert_eq!(clip_chars(&text, 50), text);
        assert_eq!(clip_chars("", 8), "");
        let long = padded("start", SIDECAR_CHARS * 2);
        assert_eq!(
            clip_chars(&long, READ_BUDGET_CHARS).chars().count(),
            READ_BUDGET_CHARS
        );
        assert_eq!(
            clip_chars(&long, SIDECAR_CHARS).chars().count(),
            SIDECAR_CHARS
        );
    }

    #[test]
    fn a_stale_value_at_rank_one_is_in_budget() {
        let render = padded(
            "Memory for \"which port\":\n\n1. 8080 [concept] — score 0.91, matched directly\n\
             \n2. 8765 [concept] — score 0.80, matched directly",
            READ_BUDGET_CHARS * 2,
        );
        let hits = scan_render(&render, "8765", &values(&["8080"]));
        assert!(hits.answer && hits.answer_24k);
        assert!(hits.stale && hits.stale_24k);
    }

    #[test]
    fn a_stale_value_only_at_the_back_of_the_window_is_in_budget() {
        let head = padded("1. 8765 [concept] — score 0.91", READ_BUDGET_CHARS - 200);
        let render = padded(&format!("{head}\n5. port was 8080"), READ_BUDGET_CHARS * 2);
        let at = render.find("8080").unwrap();
        assert!(render[..at].chars().count() > READ_BUDGET_CHARS - 200);
        assert!(render[..at].chars().count() < READ_BUDGET_CHARS);
        let hits = scan_render(&render, "8765", &values(&["8080"]));
        assert!(hits.answer);
        assert!(hits.stale, "a stale value below rank 1 still counts");
        assert!(hits.stale_24k);
    }

    #[test]
    fn a_stale_value_only_past_the_budget_is_in_the_sidecar_alone() {
        let head = padded("1. 8765 [concept] — score 0.91", READ_BUDGET_CHARS + 100);
        let render = format!("{head}\n9. port was 8080");
        let hits = scan_render(&render, "8765", &values(&["8080"]));
        assert!(hits.answer);
        assert!(!hits.stale);
        assert!(hits.stale_24k);
    }

    #[test]
    fn a_value_past_the_sidecar_is_not_seen() {
        let render = format!("{}\nport 8765", padded("", SIDECAR_CHARS + 10));
        let hits = scan_render(&render, "8765", &[]);
        assert!(!hits.answer && !hits.answer_24k);
    }

    #[test]
    fn no_stale_value_present_reads_false() {
        let hits = scan_render("1. 8765 [concept] — score 0.91", "8765", &values(&["8080"]));
        assert!(hits.answer);
        assert!(!hits.stale && !hits.stale_24k);
        let none = scan_render("1. 8765 [concept]", "8765", &[]);
        assert!(!none.stale && !none.stale_24k);
    }

    #[test]
    fn a_cross_key_stale_value_is_found_by_its_text() {
        // The old value lived under another key. Only its text is scanned for,
        // so a mention under any heading counts.
        let render = "1. shelf [project]\n   storage_path was shelf.json, now storage is shelf.db";
        let hits = scan_render(render, "shelf.db", &values(&["shelf.json"]));
        assert!(hits.answer && hits.stale);
    }

    #[test]
    fn an_old_value_still_shown_beside_the_current_one_sets_both_flags() {
        let hits = scan_render("8765\n8080", "8765", &values(&["8080"]));
        assert!(hits.answer && hits.stale);
    }

    #[test]
    fn an_empty_render_hits_nothing() {
        assert_eq!(
            scan_render("", "8765", &values(&["8080"])),
            RenderHits::default()
        );
    }

    #[test]
    fn the_store_channel_reads_live_decided_rows_only() {
        assert!(store_hit(&[row("8765", true, None, false)], "8765"));
        assert!(store_hit(
            &[row("8765", true, Some("decided"), false)],
            "8765"
        ));
        assert!(!store_hit(
            &[row("8765", true, Some("considered"), false)],
            "8765"
        ));
        assert!(!store_hit(
            &[row("8765", false, Some("rejected"), false)],
            "8765"
        ));
        assert!(!store_hit(&[row("8765", false, None, true)], "8765"));
        assert!(!store_hit(
            &[row("8765", false, Some("decided"), false)],
            "8765"
        ));
        assert!(!store_hit(&[row("18765", true, None, false)], "8765"));
        assert!(!store_hit(&[], "8765"));
    }
}
