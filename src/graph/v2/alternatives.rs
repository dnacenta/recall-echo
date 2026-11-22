// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pair a rejected or considered `uses` alternative with one decided `uses`.
//!
//! The result is optional text on the decided edge. Nothing is closed and no
//! donor row is edited.

use super::ground::sentence_containing;
use super::input::Turn;
use super::schema::{normalise, Fact, Predicate, Role};

const DETAIL_CAP: usize = 200;

/// One donor that belongs on a decided `uses` edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailPart {
    pub name: String,
    pub status: String,
    pub reason: &'static str,
    pub turn: usize,
}

/// Details aligned with the fact slice, plus donors that had two homes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pairing {
    pub details: Vec<Option<String>>,
    pub parts: Vec<Vec<DetailPart>>,
    pub ambiguous: u32,
}

impl Pairing {
    fn none(len: usize) -> Self {
        Self {
            details: vec![None; len],
            parts: vec![Vec::new(); len],
            ambiguous: 0,
        }
    }
}

/// Pair when the flag is on. Off returns an empty alignment and counts nothing.
pub(crate) fn pair_for_ingest(enabled: bool, facts: &[Fact], turns: &[Turn]) -> Pairing {
    if enabled {
        pair(facts, turns)
    } else {
        Pairing::none(facts.len())
    }
}

/// Attach each related donor to the one decided `uses` it belongs to.
pub(crate) fn pair(facts: &[Fact], turns: &[Turn]) -> Pairing {
    let departing: Vec<String> = facts
        .iter()
        .filter(|fact| fact.status == "decided" && !fact.replaces.trim().is_empty())
        .map(|fact| normalise(&fact.replaces))
        .filter(|name| !name.is_empty())
        .collect();
    let donors: Vec<usize> = facts
        .iter()
        .enumerate()
        .filter(|(_, fact)| is_donor(fact))
        .map(|(index, _)| index)
        .collect();
    let canonicals: Vec<usize> = facts
        .iter()
        .enumerate()
        .filter(|(_, fact)| is_canonical(fact))
        .map(|(index, _)| index)
        .collect();
    let mut parts = vec![Vec::new(); facts.len()];
    let mut ambiguous = 0u32;
    for donor_index in donors {
        let donor = &facts[donor_index];
        if departing
            .iter()
            .any(|name| name == &normalise(&donor.object_name))
        {
            continue;
        }
        let mut related = Vec::new();
        for canon_index in &canonicals {
            let canon = &facts[*canon_index];
            if normalise(&donor.subject_name) != normalise(&canon.subject_name) {
                continue;
            }
            if normalise(&donor.object_name) == normalise(&canon.object_name) {
                continue;
            }
            if let Some(reason) = related_reason(donor, canon, turns) {
                related.push((*canon_index, reason));
            }
        }
        if related.len() > 1 {
            ambiguous += 1;
            continue;
        }
        let Some((canon_index, reason)) = related.pop() else {
            continue;
        };
        parts[canon_index].push(DetailPart {
            name: donor.object_name.clone(),
            status: donor.status.clone(),
            reason,
            turn: donor.turn,
        });
    }
    let mut details = Vec::with_capacity(facts.len());
    let mut kept_parts = Vec::with_capacity(facts.len());
    for mut bucket in parts {
        bucket.sort_by_key(|part| part.turn);
        let (text, kept) = join_parts(&bucket);
        details.push(text);
        kept_parts.push(kept);
    }
    Pairing {
        details,
        parts: kept_parts,
        ambiguous,
    }
}

fn is_donor(fact: &Fact) -> bool {
    fact.predicate == Predicate::Uses && matches!(fact.status.as_str(), "rejected" | "considered")
}

fn is_canonical(fact: &Fact) -> bool {
    fact.predicate == Predicate::Uses && fact.status == "decided"
}

fn related_reason(donor: &Fact, canon: &Fact, turns: &[Turn]) -> Option<&'static str> {
    if donor.turn == canon.turn && same_attachable_role(donor, canon) {
        return Some("R1");
    }
    named_in_donor_sentence(canon, donor, turns).then_some("R2")
}

fn same_attachable_role(left: &Fact, right: &Fact) -> bool {
    match (left.role, right.role) {
        (Some(role), Some(other)) if role == other => !matches!(role, Role::Other | Role::None),
        _ => false,
    }
}

fn named_in_donor_sentence(canon: &Fact, donor: &Fact, turns: &[Turn]) -> bool {
    let Some(turn) = turns.iter().find(|turn| turn.index == donor.turn) else {
        return false;
    };
    let sentence = sentence_containing(&turn.text, &donor.quote);
    let needle = token_runs(&canon.object_name);
    if needle.is_empty() {
        return false;
    }
    token_runs(sentence)
        .windows(needle.len())
        .any(|window| window == needle.as_slice())
}

/// Text and donors left after a live-edge filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeptDetails {
    pub text: String,
    pub parts: Vec<DetailPart>,
}

pub(crate) fn rejoin(parts: &[DetailPart]) -> Option<KeptDetails> {
    let (text, kept) = join_parts(parts);
    text.map(|text| KeptDetails { text, parts: kept })
}

fn join_parts(parts: &[DetailPart]) -> (Option<String>, Vec<DetailPart>) {
    let mut text = String::new();
    let mut kept = Vec::new();
    for part in parts {
        let piece = format!("{}: {}", part.status, part.name);
        let next = if text.is_empty() {
            piece
        } else {
            format!("{text}; {piece}")
        };
        if next.chars().count() > DETAIL_CAP {
            break;
        }
        text = next;
        kept.push(part.clone());
    }
    if text.is_empty() {
        (None, Vec::new())
    } else {
        (Some(text), kept)
    }
}

fn token_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::confidence::Provenance;
    use crate::graph::v2::input::TurnRole;
    use crate::graph::v2::schema::Kind;

    fn turn(index: usize, text: &str) -> Turn {
        Turn {
            index,
            role: TurnRole::User,
            text: text.into(),
        }
    }

    fn uses(
        subject: &str,
        object: &str,
        status: &str,
        role: Role,
        turn_index: usize,
        quote: &str,
        replaces: &str,
    ) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Uses,
            role: Some(role),
            key: String::new(),
            object_name: object.into(),
            object_kind: Some(Kind::Technology),
            status: status.into(),
            turn: turn_index,
            quote: quote.into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: replaces.into(),
        }
    }

    #[test]
    fn rejected_same_turn_same_role_attaches() {
        let facts = vec![
            uses(
                "acme",
                "stdparser",
                "decided",
                Role::Library,
                0,
                "use stdparser",
                "",
            ),
            uses(
                "acme",
                "htmlkit",
                "rejected",
                Role::Library,
                0,
                "not htmlkit",
                "",
            ),
        ];
        let turns = vec![turn(0, "use the stdparser, not htmlkit")];
        let pairing = pair(&facts, &turns);
        assert_eq!(pairing.ambiguous, 0);
        assert_eq!(
            pairing.details,
            vec![Some("rejected: htmlkit".into()), None]
        );
        assert_eq!(pairing.parts[0][0].reason, "R1");
    }

    #[test]
    fn considered_named_in_sentence_attaches() {
        let sentence = "I'd use Webframe + servekit";
        let facts = vec![
            uses(
                "acme",
                "servekit",
                "considered",
                Role::Library,
                0,
                sentence,
                "",
            ),
            uses(
                "acme",
                "Webframe",
                "decided",
                Role::Framework,
                1,
                "Webframe is the one",
                "",
            ),
        ];
        let turns = vec![turn(0, sentence), turn(1, "Webframe is the one we ship.")];
        let pairing = pair(&facts, &turns);
        assert_eq!(pairing.ambiguous, 0);
        assert_eq!(pairing.details[0], None);
        assert_eq!(pairing.details[1].as_deref(), Some("considered: servekit"));
        assert_eq!(pairing.parts[1][0].reason, "R2");
    }

    #[test]
    fn departing_value_is_never_attached() {
        let sentence = "switch to B and drop A";
        let facts = vec![
            uses("acme", "B", "decided", Role::Library, 0, sentence, "A"),
            uses("acme", "A", "rejected", Role::Library, 0, sentence, ""),
        ];
        let pairing = pair(&facts, &[turn(0, sentence)]);
        assert!(pairing.details.iter().all(Option::is_none));
        assert_eq!(pairing.ambiguous, 0);
    }

    #[test]
    fn two_candidates_is_ambiguous() {
        let facts = vec![
            uses("acme", "stdparser", "decided", Role::Library, 0, "a", ""),
            uses("acme", "otherkit", "decided", Role::Library, 0, "b", ""),
            uses("acme", "htmlkit", "rejected", Role::Library, 0, "c", ""),
        ];
        let pairing = pair(&facts, &[turn(0, "stdparser otherkit htmlkit")]);
        assert!(pairing.details.iter().all(Option::is_none));
        assert_eq!(pairing.ambiguous, 1);
    }

    #[test]
    fn role_other_same_turn_does_not_attach() {
        let facts = vec![
            uses(
                "acme",
                "stdparser",
                "decided",
                Role::Other,
                0,
                "keep it",
                "",
            ),
            uses("acme", "htmlkit", "rejected", Role::Other, 0, "keep it", ""),
        ];
        let pairing = pair(&facts, &[turn(0, "keep the current tool")]);
        assert!(pairing.details.iter().all(Option::is_none));
        assert_eq!(pairing.ambiguous, 0);
    }

    #[test]
    fn flag_off_pairs_nothing() {
        let facts = vec![
            uses(
                "acme",
                "stdparser",
                "decided",
                Role::Library,
                0,
                "use stdparser",
                "",
            ),
            uses(
                "acme",
                "htmlkit",
                "rejected",
                Role::Library,
                0,
                "not htmlkit",
                "",
            ),
        ];
        let turns = vec![turn(0, "use the stdparser, not htmlkit")];
        let off = pair_for_ingest(false, &facts, &turns);
        assert!(off.details.iter().all(Option::is_none));
        assert!(off.parts.iter().all(Vec::is_empty));
        assert_eq!(off.ambiguous, 0);
        assert!(pair(&facts, &turns).details[0].is_some());
    }

    #[test]
    fn details_keep_whole_parts_under_the_cap() {
        let long = "n".repeat(190);
        let facts = vec![
            uses("acme", "stdparser", "decided", Role::Library, 0, "a", ""),
            uses("acme", &long, "rejected", Role::Library, 0, "b", ""),
            uses("acme", "htmlkit", "rejected", Role::Library, 1, "c", ""),
        ];
        let pairing = pair(&facts, &[turn(0, "a"), turn(1, "b")]);
        let text = pairing.details[0].as_deref().expect("details");
        assert!(text.chars().count() <= 200, "{text}");
        assert!(text.contains(&long));
        assert!(!text.contains("htmlkit"));
    }
}
