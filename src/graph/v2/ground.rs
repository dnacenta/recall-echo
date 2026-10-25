// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Grounding gates for one session.
//!
//! Each turn is normalised once. Facts then check that precomputed text, so
//! the cost stays linear in the session. Tool lines are not turns and are
//! never passed in, so tool output cannot ground a fact.
//!
//! A quote is an exact, case-sensitive substring. Fewer than three characters
//! is not a span. A quote found only in a different turn is re-cited to that
//! turn, and the fact takes that turn's role. The first turn in transcript
//! order wins when several others contain it.
//!
//! A number is a maximal `\d+(\.\d+)*`. `8080` does not occur inside `18080`,
//! and `0.2` does not occur inside `0.2.0`.
//!
//! Content tokens are the normalised tokens that are longer than one character
//! or are digits, minus the stop list. A token longer than three characters
//! may match as a case-insensitive substring of the normalised text. A shorter
//! token must equal a whole normalised token. Key tokens are whole tokens
//! only: a four-character key token does not match inside a longer word.
//!
//! The sentence that contains the quote runs from the previous boundary to the
//! next. Boundaries are `.`, `!`, `?` and newline. A dot between digits is a
//! decimal or version dot, not a boundary. The first occurrence of the quote
//! is the one that is scored.

use super::input::{Turn, TurnRole};
use super::schema::{normalise, DROP_QUOTE};

/// A quote shorter than this is not a verbatim span.
const MIN_QUOTE_CHARS: usize = 3;

/// How a stored value is shaped for the token rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueShape {
    /// An entity name. Only a short one needs token overlap.
    Entity,
    /// A literal. A long one needs half its tokens in the text.
    Literal,
}

/// Where a bug or release field found its support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldCredit {
    /// The cited turn supports the value.
    Turn,
    /// Only the rest of the session supports the value.
    Session,
    /// Neither text supports the value.
    Drop,
}

/// The turn a quote was verified against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cited {
    pub turn: usize,
    pub role: TurnRole,
}

/// One session, normalised once.
pub(crate) struct SessionGround {
    turns: Vec<TurnGround>,
    session_norm: String,
    session_words: Vec<String>,
    session_numbers: Vec<String>,
}

struct TurnGround {
    index: usize,
    role: TurnRole,
    text: String,
    normalised: String,
    words: Vec<String>,
    numbers: Vec<String>,
}

impl SessionGround {
    /// Normalise every turn. Call once per session, then share the result.
    #[must_use]
    pub(crate) fn prepare(turns: &[Turn]) -> Self {
        let mut prepared = Vec::with_capacity(turns.len());
        let mut session_norm = String::new();
        let mut session_words = Vec::new();
        let mut session_numbers = Vec::new();
        for turn in turns {
            let normalised = normalise(&turn.text);
            let words: Vec<String> = words_of(&normalised);
            let numbers = numbers_in(&turn.text);
            if !session_norm.is_empty() && !normalised.is_empty() {
                session_norm.push(' ');
            }
            session_norm.push_str(&normalised);
            session_words.extend(words.iter().cloned());
            session_numbers.extend(numbers.iter().cloned());
            prepared.push(TurnGround {
                index: turn.index,
                role: turn.role,
                text: turn.text.clone(),
                normalised,
                words,
                numbers,
            });
        }
        Self {
            turns: prepared,
            session_norm,
            session_words,
            session_numbers,
        }
    }

    /// Keep the cited turn when the quote is there. Otherwise re-cite.
    pub(crate) fn cite(&self, cited: usize, quote: &str) -> Result<Cited, &'static str> {
        if quote.chars().count() < MIN_QUOTE_CHARS {
            return Err(DROP_QUOTE);
        }
        if let Some(turn) = self.found(cited, quote) {
            return Ok(turn);
        }
        self.turns
            .iter()
            .find(|turn| turn.text.contains(quote))
            .map(|turn| Cited {
                turn: turn.index,
                role: turn.role,
            })
            .ok_or(DROP_QUOTE)
    }

    /// The raw text of a turn, after [`SessionGround::cite`] has resolved it.
    #[must_use]
    pub(crate) fn turn_text(&self, index: usize) -> Option<&str> {
        self.turn(index).map(|turn| turn.text.as_str())
    }

    /// Numbers in the value occur in the turn, and the token rule holds.
    #[must_use]
    pub(crate) fn supports_value(
        &self,
        value: &str,
        turn: usize,
        quote: &str,
        shape: ValueShape,
    ) -> bool {
        let Some(turn) = self.turn(turn) else {
            return false;
        };
        supported(
            value,
            &turn.normalised,
            &turn.words,
            &turn.numbers,
            Some(quote),
            shape,
        )
    }

    /// Cause, fix and release contents: the cited turn, else the session.
    #[must_use]
    pub(crate) fn credit_field(&self, value: &str, turn: usize, quote: &str) -> FieldCredit {
        if self.supports_value(value, turn, quote, ValueShape::Literal) {
            return FieldCredit::Turn;
        }
        if supported(
            value,
            &self.session_norm,
            &self.session_words,
            &self.session_numbers,
            None,
            ValueShape::Literal,
        ) {
            return FieldCredit::Session;
        }
        FieldCredit::Drop
    }

    /// A numbered setting must name its key in the quote or that sentence.
    ///
    /// A key whose tokens are all generic has nothing to bind. The number is
    /// still required to occur in the turn by [`SessionGround::supports_value`].
    #[must_use]
    pub(crate) fn key_bound(&self, key: &str, value: &str, turn: usize, quote: &str) -> bool {
        if numbers_in(value).is_empty() {
            return true;
        }
        let Some(turn) = self.turn(turn) else {
            return false;
        };
        let sentence = sentence_containing(&turn.text, quote);
        let texts = [quote, sentence];
        if let Some(stem) = version_stem(key) {
            if stem.is_empty() || !token_occurs(stem, &texts) {
                return false;
            }
        }
        let specific: Vec<&str> = key
            .split('_')
            .filter(|token| !token.is_empty() && !is_generic_key(token))
            .collect();
        specific.is_empty() || specific.iter().any(|token| token_occurs(token, &texts))
    }

    /// A replacement stays only on a decided fact that names it in the turn.
    #[must_use]
    pub(crate) fn replaces_grounded(&self, replaces: &str, status: &str, turn: usize) -> bool {
        if status != "decided" || replaces.trim().is_empty() {
            return false;
        }
        let Some(turn) = self.turn(turn) else {
            return false;
        };
        content_tokens(replaces)
            .iter()
            .any(|token| token_in(token, &turn.normalised, &turn.words))
    }

    fn found(&self, index: usize, quote: &str) -> Option<Cited> {
        self.turns
            .iter()
            .find(|turn| turn.index == index && turn.text.contains(quote))
            .map(|turn| Cited {
                turn: turn.index,
                role: turn.role,
            })
    }

    fn turn(&self, index: usize) -> Option<&TurnGround> {
        self.turns.iter().find(|turn| turn.index == index)
    }
}

fn supported(
    value: &str,
    normalised: &str,
    words: &[String],
    numbers: &[String],
    quote: Option<&str>,
    shape: ValueShape,
) -> bool {
    if !numbers_ok(value, numbers) {
        return false;
    }
    let tokens = content_tokens(value);
    if tokens.is_empty() {
        return false;
    }
    if tokens.len() <= 3 {
        let quoted = quote.is_none_or(|quote| one_in_quote(&tokens, quote));
        return fraction_met(&tokens, normalised, words) && quoted;
    }
    match shape {
        // A long entity is kept when its numbers are in the text. The
        // token-fraction rule is only stated for long literals.
        ValueShape::Entity => true,
        ValueShape::Literal => fraction_met(&tokens, normalised, words),
    }
}

fn numbers_ok(value: &str, numbers: &[String]) -> bool {
    numbers_in(value)
        .iter()
        .all(|number| numbers.iter().any(|have| have == number))
}

fn fraction_met(tokens: &[String], normalised: &str, words: &[String]) -> bool {
    let matched = tokens
        .iter()
        .filter(|token| token_in(token, normalised, words))
        .count();
    matched * 2 >= tokens.len()
}

fn one_in_quote(tokens: &[String], quote: &str) -> bool {
    let normalised = normalise(quote);
    let words = words_of(&normalised);
    tokens
        .iter()
        .any(|token| token_in(token, &normalised, &words))
}

fn token_in(token: &str, normalised: &str, words: &[String]) -> bool {
    if token.chars().count() > 3 {
        normalised.contains(token)
    } else {
        words.iter().any(|word| word == token)
    }
}

fn token_occurs(token: &str, texts: &[&str]) -> bool {
    let token = normalise(token);
    if token.is_empty() {
        return false;
    }
    texts.iter().any(|text| {
        let normalised = normalise(text);
        words_of(&normalised).iter().any(|word| word == &token)
    })
}

fn version_stem(key: &str) -> Option<&str> {
    key.strip_suffix("_version")
}

fn content_tokens(value: &str) -> Vec<String> {
    words_of(&normalise(value))
        .into_iter()
        .filter(|token| is_content(token))
        .collect()
}

fn words_of(normalised: &str) -> Vec<String> {
    normalised.split_whitespace().map(str::to_string).collect()
}

fn is_content(token: &str) -> bool {
    if is_stop(token) {
        return false;
    }
    token.chars().count() > 1 || token.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_stop(token: &str) -> bool {
    matches!(
        token,
        "the"
            | "a"
            | "an"
            | "of"
            | "and"
            | "or"
            | "to"
            | "in"
            | "on"
            | "for"
            | "with"
            | "is"
            | "are"
            | "be"
            | "by"
            | "at"
            | "as"
            | "it"
            | "its"
            | "use"
            | "uses"
            | "using"
            | "used"
            | "from"
            | "that"
            | "this"
            | "now"
            | "all"
            | "any"
            | "not"
            | "no"
    )
}

fn is_generic_key(token: &str) -> bool {
    matches!(
        token,
        "value" | "number" | "setting" | "default" | "current" | "new" | "min" | "max"
    )
}

fn numbers_in(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        index = end_of_number(bytes, index);
        out.push(text[start..index].to_string());
    }
    out
}

fn end_of_number(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    while index + 1 < bytes.len() && bytes[index] == b'.' && bytes[index + 1].is_ascii_digit() {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
    }
    index
}

fn sentence_containing<'a>(text: &'a str, quote: &str) -> &'a str {
    let Some(start) = text.find(quote) else {
        return text;
    };
    let end = start + quote.len();
    let bytes = text.as_bytes();
    let mut from = start;
    while from > 0 && !is_boundary(bytes, from - 1) {
        from -= 1;
    }
    let mut to = end;
    while to < bytes.len() && !is_boundary(bytes, to) {
        to += 1;
    }
    text[from..to].trim()
}

fn is_boundary(bytes: &[u8], index: usize) -> bool {
    match bytes[index] {
        b'\n' | b'!' | b'?' => true,
        b'.' => {
            let prev_digit = index > 0 && bytes[index - 1].is_ascii_digit();
            let next_digit = index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit();
            !(prev_digit && next_digit)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::v2::input::Turn;

    fn ground(lines: &[(&str, TurnRole)]) -> SessionGround {
        let turns = lines
            .iter()
            .enumerate()
            .map(|(index, (text, role))| Turn {
                index,
                role: *role,
                text: (*text).to_string(),
            })
            .collect::<Vec<_>>();
        SessionGround::prepare(&turns)
    }

    #[test]
    fn exact_quote_kept() {
        let session = ground(&[("Ship the shelf today.", TurnRole::User)]);
        let cited = session.cite(0, "the shelf").unwrap();
        assert_eq!(cited.turn, 0);
        assert_eq!(cited.role, TurnRole::User);
    }

    #[test]
    fn quote_in_other_turn_is_recited() {
        let session = ground(&[
            ("Ship the shelf.", TurnRole::User),
            ("Port 9000 is chosen.", TurnRole::Assistant),
        ]);
        let cited = session.cite(0, "Port 9000 is chosen.").unwrap();
        assert_eq!(cited.turn, 1);
        assert_eq!(cited.role, TurnRole::Assistant);
    }

    #[test]
    fn paraphrased_quote_dropped() {
        let session = ground(&[("Port 9000 is chosen.", TurnRole::User)]);
        assert_eq!(
            session.cite(0, "the port we picked").unwrap_err(),
            DROP_QUOTE
        );
        assert_eq!(
            session.cite(0, "port 9000 is chosen").unwrap_err(),
            DROP_QUOTE
        );
    }

    #[test]
    fn value_number_must_be_in_turn() {
        let other = ground(&[("Listen on 8765 for now.", TurnRole::User)]);
        assert!(!other.supports_value("8080", 0, "Listen on 8765", ValueShape::Literal));
        let inside = ground(&[("Listen on 18080 for now.", TurnRole::User)]);
        assert!(!inside.supports_value("8080", 0, "Listen on 18080", ValueShape::Literal));
        let same = ground(&[("Listen on 8080 for now.", TurnRole::User)]);
        assert!(same.supports_value("8080", 0, "Listen on 8080", ValueShape::Literal));
    }

    #[test]
    fn entity_object_needs_token_in_quote() {
        let session = ground(&[("We use the database for storage.", TurnRole::User)]);
        assert!(!session.supports_value(
            "PostgreSQL",
            0,
            "We use the database",
            ValueShape::Entity
        ));
        let named = ground(&[("We use PostgreSQL for storage.", TurnRole::User)]);
        assert!(named.supports_value("PostgreSQL", 0, "use PostgreSQL", ValueShape::Entity));
    }

    #[test]
    fn number_needs_key_token() {
        let session = ground(&[("Coverage is 88%. Port 9000.", TurnRole::User)]);
        assert!(session.supports_value("88", 0, "Coverage is 88%", ValueShape::Literal));
        assert!(!session.key_bound("line_length", "88", 0, "Coverage is 88%"));
        assert!(session.supports_value("9000", 0, "Port 9000.", ValueShape::Literal));
        assert!(session.key_bound("port", "9000", 0, "Port 9000."));
    }

    #[test]
    fn version_key_needs_stem() {
        let tagged = ground(&[("Tag v0.2.0 today.", TurnRole::User)]);
        assert!(tagged.supports_value("0.2.0", 0, "Tag v0.2.0", ValueShape::Literal));
        assert!(!tagged.key_bound("python_version", "0.2.0", 0, "Tag v0.2.0"));
        let python = ground(&[("Python 3.12 is the runtime.", TurnRole::User)]);
        assert!(python.supports_value("3.12", 0, "Python 3.12", ValueShape::Literal));
        assert!(python.key_bound("python_version", "3.12", 0, "Python 3.12"));
    }

    #[test]
    fn long_literal_half_tokens() {
        let session = ground(&[("alpha beta are kept here", TurnRole::User)]);
        assert!(session.supports_value(
            "alpha beta gamma delta",
            0,
            "kept here",
            ValueShape::Literal
        ));
        assert!(!session.supports_value(
            "alpha gamma delta epsilon",
            0,
            "kept here",
            ValueShape::Literal
        ));
    }

    #[test]
    fn unicode_quote_is_exact_and_case_sensitive() {
        let session = ground(&[("The café opened today.", TurnRole::User)]);
        assert!(session.cite(0, "café opened").is_ok());
        assert_eq!(session.cite(0, "cafe opened").unwrap_err(), DROP_QUOTE);
        assert!(session.supports_value("café", 0, "café opened", ValueShape::Literal));
    }

    #[test]
    fn empty_or_short_quote_is_dropped() {
        let session = ground(&[("abc is enough.", TurnRole::User)]);
        assert_eq!(session.cite(0, "").unwrap_err(), DROP_QUOTE);
        assert_eq!(session.cite(0, "ab").unwrap_err(), DROP_QUOTE);
        assert!(session.cite(0, "abc").is_ok());
    }

    #[test]
    fn version_number_does_not_match_a_shorter_prefix() {
        let full = ground(&[("Ship 0.2.0 now.", TurnRole::User)]);
        assert!(!full.supports_value("0.2", 0, "Ship 0.2.0", ValueShape::Literal));
        assert!(full.supports_value("0.2.0", 0, "Ship 0.2.0", ValueShape::Literal));
        let short = ground(&[("Ship 0.2 now.", TurnRole::User)]);
        assert!(!short.supports_value("0.2.0", 0, "Ship 0.2", ValueShape::Literal));
    }

    #[test]
    fn repeated_number_occurs_once() {
        let session = ground(&[("Use 8080 and also 8080.", TurnRole::User)]);
        assert!(session.supports_value("8080", 0, "Use 8080", ValueShape::Literal));
        assert!(session.supports_value("8080 8080", 0, "also 8080", ValueShape::Literal));
    }

    #[test]
    fn key_tokens_are_split_on_underscores() {
        let both = ground(&[("The request timeout is 30 seconds.", TurnRole::User)]);
        assert!(both.key_bound("request_timeout", "30", 0, "request timeout is 30"));
        // One specific token is enough. A sentence with neither token is not.
        let one = ground(&[("Set the request to 30.", TurnRole::User)]);
        assert!(one.key_bound("request_timeout", "30", 0, "Set the request to 30."));
        let neither = ground(&[("Set the limit to 30.", TurnRole::User)]);
        assert!(!neither.key_bound("request_timeout", "30", 0, "Set the limit to 30."));
    }

    #[test]
    fn key_token_is_limited_to_the_quote_sentence() {
        let later = ground(&[("Coverage is 88%. The line length is 100.", TurnRole::User)]);
        assert!(!later.key_bound("line_length", "88", 0, "Coverage is 88%"));
        let version = ground(&[("Use Python 3.12 today.", TurnRole::User)]);
        assert!(version.key_bound("python_version", "3.12", 0, "Python 3.12"));
        let split = ground(&[("Python is ready. 3.12 ships tomorrow.", TurnRole::User)]);
        assert!(!split.key_bound("python_version", "3.12", 0, "3.12 ships"));
    }

    #[test]
    fn generic_key_does_not_bind_and_numberless_value_skips_it() {
        let session = ground(&[("Port 9000.", TurnRole::User)]);
        assert!(session.key_bound("value", "9000", 0, "Port 9000."));
        assert!(session.key_bound("line_length", "wide", 0, "Port 9000."));
    }

    #[test]
    fn replaces_stays_only_when_decided_and_named() {
        let session = ground(&[("Move from Postgres to SQLite.", TurnRole::User)]);
        assert!(session.replaces_grounded("Postgres", "decided", 0));
        assert!(!session.replaces_grounded("Postgres", "rejected", 0));
        assert!(!session.replaces_grounded("MySQL", "decided", 0));
    }

    #[test]
    fn session_text_can_support_a_field_the_quote_cannot() {
        let session = ground(&[
            ("The cache returned stale entries.", TurnRole::User),
            ("The TTL was ignored in the reader.", TurnRole::Assistant),
        ]);
        assert_eq!(
            session.credit_field("the TTL was ignored", 0, "stale entries"),
            FieldCredit::Session
        );
        assert_eq!(
            session.credit_field("invented remedy", 0, "stale entries"),
            FieldCredit::Drop
        );
        assert_eq!(
            session.credit_field("stale entries", 0, "stale entries"),
            FieldCredit::Turn
        );
    }
}
