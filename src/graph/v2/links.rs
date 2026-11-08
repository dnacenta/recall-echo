// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Premise choice for a derived-from list.
//!
//! A link is a whole-token mention or an exact projected value. A mention
//! that fits more than one label is left off, and so is a value that is
//! shared or empty. Nothing here calls a provider.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::known_match::{mention, value_spans};
use super::schema::{restatement_projection, Predicate};

/// At most this many premises are stored on one row.
const FAN_IN_CAP: usize = 4;

/// A live row the new fact may point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Premise {
    pub id: String,
    /// Entity label, or the setting's stored value.
    pub text: String,
    pub kind: PremiseKind,
}

/// What kind of already-stored row `Premise` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PremiseKind {
    /// A live `uses`, `part_of`, or `depends_on` edge.
    Entity,
    /// A live setting.
    Setting,
}

/// The list to store, already capped, and how many proved premises did not fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedLinks {
    pub ids: Vec<String>,
    pub overflow: u32,
}

/// Premises for a new decided setting or rule.
///
/// Other predicates get an empty list. The result is deduplicated and sorted
/// by row id. Empty means the row is stored with no `derived_from` key.
#[must_use]
pub(crate) fn select_premises(
    premises: &[Premise],
    predicate: Predicate,
    text: &str,
) -> SelectedLinks {
    if !matches!(predicate, Predicate::Setting | Predicate::Rule) {
        return SelectedLinks {
            ids: Vec::new(),
            overflow: 0,
        };
    }
    let mut ids = BTreeSet::new();
    collect_entities(premises, text, &mut ids);
    if predicate == Predicate::Setting {
        collect_value_copy(premises, text, &mut ids);
    } else {
        collect_setting_mentions(premises, text, &mut ids);
    }
    let all: Vec<String> = ids.into_iter().collect();
    let overflow = all.len().saturating_sub(FAN_IN_CAP);
    let ids = all.into_iter().take(FAN_IN_CAP).collect();
    SelectedLinks {
        ids,
        overflow: u32::try_from(overflow).unwrap_or(u32::MAX),
    }
}

fn collect_entities(premises: &[Premise], text: &str, ids: &mut BTreeSet<String>) {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for premise in premises {
        if premise.kind != PremiseKind::Entity {
            continue;
        }
        groups
            .entry(premise.text.clone())
            .or_default()
            .push(premise.id.clone());
    }
    let mut mentioned = Vec::new();
    for (label, group_ids) in groups {
        let spans = value_spans(text, &label);
        if spans.is_empty() {
            continue;
        }
        mentioned.push(Mentioned {
            spans,
            ids: group_ids,
        });
    }
    let excluded = overlapping(&mentioned);
    for (index, group) in mentioned.iter().enumerate() {
        if excluded.contains(&index) {
            continue;
        }
        ids.extend(group.ids.iter().cloned());
    }
}

struct Mentioned {
    spans: Vec<(usize, usize)>,
    ids: Vec<String>,
}

/// Labels whose spans touch are one mention of more than one label.
///
/// A label outside that set stays, including one that does not overlap any of them.
fn overlapping(mentioned: &[Mentioned]) -> HashSet<usize> {
    let count = mentioned.len();
    let mut parent: Vec<usize> = (0..count).collect();
    for left in 0..count {
        for right in (left + 1)..count {
            if spans_overlap(&mentioned[left].spans, &mentioned[right].spans) {
                unite(&mut parent, left, right);
            }
        }
    }
    let roots: Vec<usize> = (0..count).map(|index| find(&mut parent, index)).collect();
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for root in &roots {
        *sizes.entry(*root).or_default() += 1;
    }
    roots
        .into_iter()
        .enumerate()
        .filter_map(|(index, root)| (sizes[&root] > 1).then_some(index))
        .collect()
}

fn spans_overlap(left: &[(usize, usize)], right: &[(usize, usize)]) -> bool {
    left.iter().any(|&(start, end)| {
        right
            .iter()
            .any(|&(other_start, other_end)| start < other_end && other_start < end)
    })
}

fn find(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

fn unite(parent: &mut [usize], left: usize, right: usize) {
    let left = find(parent, left);
    let right = find(parent, right);
    if left != right {
        parent[right] = left;
    }
}

fn collect_value_copy(premises: &[Premise], text: &str, ids: &mut BTreeSet<String>) {
    let projected = restatement_projection(text);
    if projected.is_empty() {
        return;
    }
    let matches: Vec<&str> = premises
        .iter()
        .filter(|premise| {
            premise.kind == PremiseKind::Setting
                && restatement_projection(&premise.text) == projected
        })
        .map(|premise| premise.id.as_str())
        .collect();
    if matches.len() == 1 {
        ids.insert(matches[0].to_string());
    }
}

fn collect_setting_mentions(premises: &[Premise], text: &str, ids: &mut BTreeSet<String>) {
    let mut groups: BTreeMap<String, Vec<&Premise>> = BTreeMap::new();
    for premise in premises {
        if premise.kind != PremiseKind::Setting {
            continue;
        }
        let projected = restatement_projection(&premise.text);
        if projected.is_empty() {
            continue;
        }
        groups.entry(projected).or_default().push(premise);
    }
    for group in groups.values() {
        let [premise] = group.as_slice() else {
            continue;
        };
        if mention(&premise.text, text) {
            ids.insert(premise.id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, label: &str) -> Premise {
        Premise {
            id: id.into(),
            text: label.into(),
            kind: PremiseKind::Entity,
        }
    }

    fn setting(id: &str, value: &str) -> Premise {
        Premise {
            id: id.into(),
            text: value.into(),
            kind: PremiseKind::Setting,
        }
    }

    fn rule(premises: &[Premise], text: &str) -> SelectedLinks {
        select_premises(premises, Predicate::Rule, text)
    }

    fn copied(premises: &[Premise], text: &str) -> SelectedLinks {
        select_premises(premises, Predicate::Setting, text)
    }

    #[test]
    fn a_mention_is_a_whole_token_run() {
        let premises = [entity("relates_to:queue", "cache")];
        assert_eq!(
            rule(&premises, "use the cache now").ids,
            vec!["relates_to:queue".to_string()]
        );
        assert!(rule(&premises, "leave the uncached path").ids.is_empty());
        assert!(rule(&premises, "caches stay warm").ids.is_empty());
        assert!(rule(&[entity("relates_to:n", "12")], "312").ids.is_empty());
        let named = copied(&premises, "warm the cache");
        assert_eq!(named.ids, vec!["relates_to:queue".to_string()]);
        assert!(copied(&premises, "warm the uncached path").ids.is_empty());
    }

    #[test]
    fn overlapping_spans_drop_every_label_in_the_set() {
        let cache = entity("relates_to:cache", "cache");
        let service = entity("relates_to:service", "cache service");
        let word = entity("relates_to:word", "service");
        let broker = entity("relates_to:broker", "broker");
        let nested = [cache.clone(), service.clone()];
        assert!(rule(&nested, "use the cache service").ids.is_empty());

        let chain = [cache.clone(), service.clone(), word];
        assert!(rule(&chain, "use the cache service").ids.is_empty());

        let with_broker = [cache.clone(), service.clone(), broker.clone()];
        assert_eq!(
            rule(&with_broker, "use the cache service and the broker").ids,
            vec!["relates_to:broker".to_string()]
        );

        let apart = [cache, broker];
        assert_eq!(
            rule(&apart, "note the cache and the broker").ids,
            vec![
                "relates_to:broker".to_string(),
                "relates_to:cache".to_string()
            ]
        );
    }

    #[test]
    fn several_unique_labels_keep_each_group_then_the_cap() {
        let premises = [
            entity("relates_to:m", "alpha"),
            entity("relates_to:m", "alpha"),
            entity("relates_to:a", "bravo"),
            entity("relates_to:z", "cedar"),
            entity("relates_to:b", "delta"),
            entity("relates_to:k", "ember"),
        ];
        let selected = rule(&premises, "note alpha bravo cedar delta ember");
        assert_eq!(
            selected.ids,
            vec![
                "relates_to:a".to_string(),
                "relates_to:b".to_string(),
                "relates_to:k".to_string(),
                "relates_to:m".to_string(),
            ]
        );
        assert_eq!(selected.overflow, 1);
    }

    #[test]
    fn a_value_copy_is_exact_projected_equality() {
        let one = [setting("entity:alpha#0", "  Use   10s  ")];
        assert_eq!(
            copied(&one, "use 10s").ids,
            vec!["entity:alpha#0".to_string()]
        );
        assert!(copied(&one, "10S").ids.is_empty());
        assert!(copied(&[setting("entity:alpha#0", "10s")], "10S")
            .ids
            .is_empty());
        assert!(copied(&[setting("entity:alpha#0", "10s")], "10ms")
            .ids
            .is_empty());
        assert_eq!(
            copied(&[setting("entity:alpha#0", "timeout=10s")], "timeout=10s").ids,
            vec!["entity:alpha#0".to_string()]
        );
        assert!(
            copied(&[setting("entity:alpha#0", "timeout=10s")], "timeout 10s")
                .ids
                .is_empty()
        );
        assert_eq!(
            copied(&[setting("entity:alpha#0", "use 10s.")], "use 10s.").ids,
            vec!["entity:alpha#0".to_string()]
        );
        assert!(copied(&[setting("entity:alpha#0", "10s")], "wait 10s")
            .ids
            .is_empty());
    }

    #[test]
    fn a_shared_or_empty_projected_value_is_not_a_copy() {
        let shared = [
            setting("entity:alpha#0", "10s"),
            setting("entity:gamma#0", "  10s  "),
        ];
        let shared_copy = copied(&shared, "10s");
        assert!(shared_copy.ids.is_empty());
        assert_eq!(shared_copy.overflow, 0);

        let blank = [setting("entity:alpha#1", "   ")];
        assert!(copied(&blank, "   ").ids.is_empty());
        assert!(copied(&blank, "").ids.is_empty());
        assert!(rule(&blank, "keep the note").ids.is_empty());
        assert!(copied(&[setting("entity:alpha#1", "")], "10s")
            .ids
            .is_empty());
    }

    #[test]
    fn a_rule_links_a_uniquely_mentioned_setting_value() {
        let one = [setting("entity:alpha#0", "10s")];
        assert_eq!(
            rule(&one, "wait 10s before retry").ids,
            vec!["entity:alpha#0".to_string()]
        );
        let shared = [
            setting("entity:alpha#0", "10s"),
            setting("entity:gamma#0", "10s"),
        ];
        assert!(rule(&shared, "wait 10s before retry").ids.is_empty());
        assert!(rule(&one, "wait longer").ids.is_empty());
        assert!(rule(&[setting("entity:alpha#0", "   ")], "wait 10s")
            .ids
            .is_empty());
    }

    #[test]
    fn ineligible_predicates_receive_no_list() {
        let premises = [
            entity("relates_to:queue", "cache"),
            setting("entity:alpha#0", "10s"),
        ];
        for predicate in [
            Predicate::Uses,
            Predicate::PartOf,
            Predicate::DependsOn,
            Predicate::Affects,
            Predicate::WorksOn,
            Predicate::Prefers,
            Predicate::IssueSymptom,
            Predicate::IssueStatus,
            Predicate::IssueCause,
            Predicate::IssueFix,
            Predicate::ReleaseStatus,
            Predicate::ReleaseContents,
        ] {
            let selected = select_premises(&premises, predicate, "use the cache for 10s");
            assert!(selected.ids.is_empty(), "{predicate:?}");
            assert_eq!(selected.overflow, 0, "{predicate:?}");
        }
    }

    #[test]
    fn entity_and_value_premises_sort_together() {
        let premises = [
            setting("entity:zeta#0", "10s"),
            entity("relates_to:queue", "cache"),
        ];
        let selected = copied(&premises, "warm the cache");
        assert_eq!(selected.ids, vec!["relates_to:queue".to_string()]);
        let mixed = select_premises(&premises, Predicate::Setting, "10s");
        assert_eq!(mixed.ids, vec!["entity:zeta#0".to_string()]);
        let rule_both = rule(&premises, "warm the cache after 10s");
        assert_eq!(
            rule_both.ids,
            vec!["entity:zeta#0".to_string(), "relates_to:queue".to_string(),]
        );
    }
}
