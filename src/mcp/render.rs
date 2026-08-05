// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turning retrieval results into text an LLM can use.
//!
//! The daemon answers in JSON built for programs: record ids, distances,
//! nested traversal nodes. Handing that to a model wastes context on syntax
//! and buries the parts that matter. Everything here renders the same data as
//! compact prose-with-structure, keeps the numbers a reader would act on
//! (retrieval score, similarity, edge confidence, utility) and drops the ones
//! nobody reads.
//!
//! Every renderer is total: an empty result set produces guidance about what
//! to try next, not an empty string.

use std::fmt::Write as _;

use serde_json::Value;

use crate::graph::traverse::format_traversal;
use crate::graph::types::{
    EntityDetail, EpisodeSearchResult, GraphStats, MatchSource, QueryResult, ScoredEntity,
    TraversalNode,
};

/// Longest abstract kept verbatim.
const MAX_ABSTRACT_CHARS: usize = 300;
/// Longest overview kept verbatim. Overviews are the L1 tier — worth showing,
/// not worth showing whole.
const MAX_OVERVIEW_CHARS: usize = 400;
/// Longest verbatim excerpt of one episode's original text.
const MAX_EPISODE_CHARS: usize = 1_200;
/// Ceiling on a whole tool result. A memory lookup that eats the context
/// window defeats its own purpose.
const MAX_RESULT_CHARS: usize = 24_000;
/// The neutral utility score an entity carries until outcome feedback moves
/// it. Reporting it would be reporting the absence of information.
const NEUTRAL_UTILITY: f64 = 0.5;

/// Entity search results.
#[must_use]
pub fn entities(query: &str, results: &[ScoredEntity]) -> String {
    if results.is_empty() {
        return format!(
            "No entities in memory match \"{query}\".\n\
             Entities are distilled knowledge; the raw conversations may still hold it — try \
             recall_episodes. If recall_status shows an empty graph, nothing has been ingested \
             yet."
        );
    }

    let mut out = format!(
        "{} {} in memory for \"{query}\":\n",
        results.len(),
        plural(results.len(), "entity", "entities")
    );
    for (index, result) in results.iter().enumerate() {
        write_entity(&mut out, index + 1, result);
    }
    budget(out)
}

/// Hybrid query results: entities, then the episodes behind them.
#[must_use]
pub fn query_result(query: &str, result: &QueryResult) -> String {
    if result.entities.is_empty() && result.episodes.is_empty() {
        return format!(
            "Memory holds nothing about \"{query}\".\n\
             Either it was never discussed, or it has not been ingested yet — recall_status \
             says which."
        );
    }

    let mut out = format!("Memory for \"{query}\":\n");

    if result.entities.is_empty() {
        out.push_str("\nNo distilled entities matched, but these conversations did.\n");
    } else {
        let _ = writeln!(
            out,
            "\n{} {}:",
            result.entities.len(),
            plural(result.entities.len(), "entity", "entities")
        );
        for (index, entity) in result.entities.iter().enumerate() {
            write_entity(&mut out, index + 1, entity);
        }
    }

    if !result.episodes.is_empty() {
        let _ = writeln!(
            out,
            "\n{} conversation {}:",
            result.episodes.len(),
            plural(result.episodes.len(), "fragment", "fragments")
        );
        for (index, episode) in result.episodes.iter().enumerate() {
            write_episode(&mut out, index + 1, episode);
        }
    }

    budget(out)
}

/// Episode search results.
#[must_use]
pub fn episodes(query: &str, results: &[EpisodeSearchResult]) -> String {
    if results.is_empty() {
        return format!(
            "No past conversation in memory matches \"{query}\".\n\
             If recall_status shows episodes exist, the topic is genuinely absent; if it shows \
             none, no sessions have been archived into the graph yet."
        );
    }

    let mut out = format!(
        "{} conversation {} for \"{query}\":\n",
        results.len(),
        plural(results.len(), "fragment", "fragments")
    );
    for (index, result) in results.iter().enumerate() {
        write_episode(&mut out, index + 1, result);
    }
    budget(out)
}

/// A traversal tree rooted at one entity.
#[must_use]
pub fn traversal(entity: &str, depth: u32, node: &TraversalNode) -> String {
    if node.edges.is_empty() {
        return format!(
            "\"{}\" ({}) exists in memory but has no relationships recorded within {depth} \
             {}.\nIts own description: {}",
            node.entity.name,
            node.entity.entity_type,
            plural(depth as usize, "hop", "hops"),
            clip(&node.entity.abstract_text, MAX_ABSTRACT_CHARS)
        );
    }

    let tree = format_traversal(node, 0);
    let mut out = format!(
        "Relationships from \"{entity}\", up to {depth} {}:\n\n{tree}",
        plural(depth as usize, "hop", "hops")
    );
    if tree.contains('%') || tree.contains("[superseded]") {
        out.push_str(
            "\nA percentage is the edge's accumulated confidence (absent means fully \
             corroborated); [superseded] marks a relationship that was true once and no longer \
             is.\n",
        );
    }
    budget(out)
}

/// Graph counts.
#[must_use]
pub fn status(stats: &GraphStats) -> String {
    let mut out = format!(
        "Memory graph: {} {}, {} {}, {} conversation {}.\n",
        stats.entity_count,
        plural(stats.entity_count as usize, "entity", "entities"),
        stats.relationship_count,
        plural(
            stats.relationship_count as usize,
            "relationship",
            "relationships"
        ),
        stats.episode_count,
        plural(stats.episode_count as usize, "episode", "episodes"),
    );

    if !stats.entity_type_counts.is_empty() {
        let mut types: Vec<_> = stats.entity_type_counts.iter().collect();
        types.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        let listed: Vec<String> = types
            .iter()
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        let _ = writeln!(out, "By type: {}.", listed.join(", "));
    }

    if stats.entity_count == 0 && stats.episode_count == 0 {
        out.push_str(
            "The graph is empty: no sessions have been ingested, so recall tools will find \
             nothing.\n",
        );
    } else if stats.entity_count == 0 {
        out.push_str(
            "Conversations have been ingested but never distilled into entities, so \
             recall_search and recall_query will be thin — recall_episodes still works.\n",
        );
    }

    budget(out)
}

// ── Pieces ───────────────────────────────────────────────────────────────

fn write_entity(out: &mut String, position: usize, result: &ScoredEntity) {
    let entity = &result.entity;
    let _ = writeln!(
        out,
        "\n{position}. {} [{}] — score {:.2}, {}",
        entity.name,
        entity.entity_type,
        result.score,
        match_source(&result.source)
    );
    let _ = writeln!(
        out,
        "   {}",
        clip(&entity.abstract_text, MAX_ABSTRACT_CHARS)
    );
    if adds_detail(entity) {
        let _ = writeln!(out, "   {}", clip(&entity.overview, MAX_OVERVIEW_CHARS));
    }
    if let Some(provenance) = entity_provenance(entity) {
        let _ = writeln!(out, "   {provenance}");
    }
}

/// The overview is worth its tokens only when it says more than the abstract
/// already did.
fn adds_detail(entity: &EntityDetail) -> bool {
    let overview = entity.overview.trim();
    !overview.is_empty() && overview != entity.abstract_text.trim()
}

/// The line that says how much to trust this entity and where it came from.
fn entity_provenance(entity: &EntityDetail) -> Option<String> {
    let mut parts = Vec::new();
    let updated = short_time(&entity.updated_at);
    if !updated.is_empty() {
        parts.push(format!("updated {updated}"));
    }
    if let Some(source) = entity.source.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(format!("from {source}"));
    }
    if (entity.utility_score - NEUTRAL_UTILITY).abs() > 0.005 {
        parts.push(format!("usefulness {:.2}", entity.utility_score));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn match_source(source: &MatchSource) -> String {
    match source {
        MatchSource::Semantic => "matched directly".to_string(),
        MatchSource::Keyword => "matched by keyword".to_string(),
        MatchSource::Graph { parent, rel_type } => {
            format!("reached from \"{parent}\" via {rel_type}")
        }
    }
}

fn write_episode(out: &mut String, position: usize, result: &EpisodeSearchResult) {
    let episode = &result.episode;
    let mut header = format!("\n{position}. session {}", episode.session_id);
    if let Some(log) = episode.log_number {
        let _ = write!(header, ", archive log #{log}");
    }
    let timestamp = short_time(&episode.timestamp);
    if !timestamp.is_empty() {
        let _ = write!(header, ", {timestamp}");
    }
    let _ = writeln!(
        out,
        "{header} — score {:.2}, similarity {:.2}",
        result.score,
        1.0 - result.distance
    );
    let _ = writeln!(
        out,
        "   {}",
        clip(&episode.abstract_text, MAX_ABSTRACT_CHARS)
    );

    // The chunk itself is the reason to call this tool at all: the abstract is
    // a label, the content is what was said.
    if let Some(content) = episode.content.as_deref().filter(|c| !c.trim().is_empty()) {
        let excerpt = clip(content, MAX_EPISODE_CHARS);
        if excerpt != episode.abstract_text.trim() {
            let _ = writeln!(out, "   ---\n{}", indent(&excerpt, "   "));
        }
    }
}

// ── Text utilities ───────────────────────────────────────────────────────

/// Trim to `max` characters on a character boundary, marking the cut.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push_str(" […]");
    clipped
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Timestamps arrive as JSON scalars. Keep them to seconds — sub-second
/// precision on a memory from last March is noise.
fn short_time(value: &Value) -> String {
    let raw = match value {
        Value::Null => return String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    match raw.find('.') {
        Some(dot) if raw.contains('T') => raw[..dot].to_string(),
        _ => raw,
    }
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// Hold a rendered result inside [`MAX_RESULT_CHARS`], saying so when it cuts.
fn budget(text: String) -> String {
    if text.chars().count() <= MAX_RESULT_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX_RESULT_CHARS).collect();
    clipped.push_str("\n\n[result truncated — ask a narrower question or lower `limit`]");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::{EntitySummary, EntityType, Episode, TraversalEdge};
    use serde_json::json;

    fn entity(name: &str) -> EntityDetail {
        EntityDetail {
            id: json!(format!("entity:{name}")),
            name: name.to_string(),
            entity_type: EntityType::Tool,
            abstract_text: format!("{name} is a thing."),
            overview: format!("{name} does something in more words than the abstract."),
            attributes: None,
            access_count: 3,
            utility_score: NEUTRAL_UTILITY,
            updated_at: json!("2026-05-01T09:15:30.123456Z"),
            source: Some("archive-log-042".into()),
        }
    }

    fn scored(name: &str, score: f64, source: MatchSource) -> ScoredEntity {
        ScoredEntity {
            entity: entity(name),
            score,
            // Fixtures only need a self-consistent value; the render layer
            // shows similarity for episodes, not entities.
            similarity: score,
            source,
        }
    }

    fn episode(session: &str, content: &str) -> EpisodeSearchResult {
        EpisodeSearchResult {
            episode: Episode {
                id: json!("episode:1"),
                session_id: session.to_string(),
                timestamp: json!("2026-04-02T18:00:00Z"),
                abstract_text: "A chat about deploys.".into(),
                overview: None,
                content: Some(content.to_string()),
                embedding: None,
                log_number: Some(42),
                provenance: Some("human".into()),
                access_count: 0,
            },
            score: 0.71,
            distance: 0.32,
        }
    }

    #[test]
    fn empty_entity_search_points_at_the_next_move() {
        let text = entities("deploys", &[]);
        assert!(text.contains("No entities"));
        assert!(text.contains("recall_episodes"));
        assert!(text.contains("recall_status"));
    }

    #[test]
    fn entities_carry_name_type_score_and_provenance() {
        let text = entities("rust", &[scored("Rust", 0.812, MatchSource::Semantic)]);
        assert!(
            text.contains("1. Rust [tool] — score 0.81, matched directly"),
            "{text}"
        );
        assert!(text.contains("Rust is a thing."), "{text}");
        assert!(text.contains("updated 2026-05-01T09:15:30"), "{text}");
        assert!(text.contains("from archive-log-042"), "{text}");
        // Neutral usefulness is the absence of feedback, not a fact.
        assert!(!text.contains("usefulness"), "{text}");
    }

    #[test]
    fn graph_reached_entities_say_how_they_were_reached() {
        let text = entities(
            "rust",
            &[scored(
                "Cargo",
                0.4,
                MatchSource::Graph {
                    parent: "Rust".into(),
                    rel_type: "USES".into(),
                },
            )],
        );
        assert!(text.contains("reached from \"Rust\" via USES"), "{text}");
    }

    #[test]
    fn moved_usefulness_is_reported() {
        let mut result = scored("Rust", 0.5, MatchSource::Semantic);
        result.entity.utility_score = 0.82;
        let text = entities("rust", &[result]);
        assert!(text.contains("usefulness 0.82"), "{text}");
    }

    #[test]
    fn identical_overview_is_not_repeated() {
        let mut result = scored("Rust", 0.5, MatchSource::Semantic);
        result.entity.overview = result.entity.abstract_text.clone();
        let text = entities("rust", &[result]);
        assert_eq!(text.matches("Rust is a thing.").count(), 1, "{text}");
    }

    #[test]
    fn episodes_report_similarity_and_the_original_text() {
        let text = episodes(
            "deploys",
            &[episode("abc123", "We ran cargo dist and it broke.")],
        );
        assert!(text.contains("session abc123"), "{text}");
        assert!(text.contains("archive log #42"), "{text}");
        assert!(text.contains("similarity 0.68"), "{text}");
        assert!(text.contains("We ran cargo dist and it broke."), "{text}");
    }

    #[test]
    fn long_episode_content_is_clipped() {
        let long = "x".repeat(MAX_EPISODE_CHARS * 2);
        let text = episodes("deploys", &[episode("abc123", &long)]);
        assert!(text.contains("[…]"), "{text}");
        assert!(text.chars().count() < long.chars().count());
    }

    #[test]
    fn query_result_separates_entities_from_fragments() {
        let result = QueryResult {
            entities: vec![scored("Rust", 0.9, MatchSource::Semantic)],
            episodes: vec![episode("abc123", "some talk")],
        };
        let text = query_result("rust", &result);
        assert!(text.contains("1 entity:"), "{text}");
        assert!(text.contains("1 conversation fragment:"), "{text}");
    }

    #[test]
    fn empty_query_result_explains_the_two_possibilities() {
        let result = QueryResult {
            entities: Vec::new(),
            episodes: Vec::new(),
        };
        let text = query_result("nothing", &result);
        assert!(text.contains("recall_status"), "{text}");
    }

    fn leaf(name: &str) -> TraversalNode {
        TraversalNode {
            entity: EntitySummary {
                id: json!(format!("entity:{name}")),
                name: name.to_string(),
                entity_type: EntityType::Tool,
                abstract_text: format!("{name} is a thing."),
            },
            edges: Vec::new(),
        }
    }

    #[test]
    fn a_lone_entity_says_so_instead_of_printing_an_empty_tree() {
        let text = traversal("Rust", 2, &leaf("Rust"));
        assert!(text.contains("no relationships recorded"), "{text}");
        assert!(text.contains("Rust is a thing."), "{text}");
    }

    #[test]
    fn uncertain_edges_get_a_legend() {
        let mut root = leaf("Rust");
        root.edges.push(TraversalEdge {
            rel_type: "USES".into(),
            direction: "->".into(),
            target: leaf("Cargo"),
            valid_from: json!("2026-01-01T00:00:00Z"),
            valid_until: None,
            confidence: 0.62,
        });
        let text = traversal("Rust", 1, &root);
        assert!(text.contains("[62%]"), "{text}");
        assert!(text.contains("accumulated confidence"), "{text}");
    }

    #[test]
    fn certain_edges_get_no_legend() {
        let mut root = leaf("Rust");
        root.edges.push(TraversalEdge {
            rel_type: "USES".into(),
            direction: "->".into(),
            target: leaf("Cargo"),
            valid_from: json!("2026-01-01T00:00:00Z"),
            valid_until: None,
            confidence: 1.0,
        });
        let text = traversal("Rust", 1, &root);
        assert!(!text.contains("accumulated confidence"), "{text}");
    }

    #[test]
    fn status_reports_counts_and_flags_an_empty_graph() {
        let empty = GraphStats {
            entity_count: 0,
            relationship_count: 0,
            episode_count: 0,
            entity_type_counts: Default::default(),
        };
        let text = status(&empty);
        assert!(text.contains("0 entities"), "{text}");
        assert!(text.contains("The graph is empty"), "{text}");
    }

    #[test]
    fn status_flags_episodes_without_entities() {
        let stats = GraphStats {
            entity_count: 0,
            relationship_count: 0,
            episode_count: 120,
            entity_type_counts: Default::default(),
        };
        let text = status(&stats);
        assert!(text.contains("never distilled"), "{text}");
        assert!(text.contains("recall_episodes"), "{text}");
    }

    #[test]
    fn status_lists_types_by_descending_count() {
        let mut counts = std::collections::HashMap::new();
        counts.insert("tool".to_string(), 3);
        counts.insert("project".to_string(), 9);
        let stats = GraphStats {
            entity_count: 12,
            relationship_count: 4,
            episode_count: 1,
            entity_type_counts: counts,
        };
        let text = status(&stats);
        assert!(text.contains("By type: project 9, tool 3."), "{text}");
        assert!(text.contains("1 conversation episode."), "{text}");
    }

    #[test]
    fn results_stay_inside_the_character_budget() {
        let long = "y".repeat(MAX_RESULT_CHARS * 2);
        let clipped = budget(long);
        assert!(clipped.contains("result truncated"));
        assert!(clipped.chars().count() < MAX_RESULT_CHARS + 100);
    }

    #[test]
    fn clip_respects_character_boundaries() {
        let text = "é".repeat(10);
        assert_eq!(clip(&text, 3), "ééé […]");
        assert_eq!(clip(&text, 50), text);
    }
}
