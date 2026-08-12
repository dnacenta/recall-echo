// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `recall-echo what-do-you-know` — memory, read out loud.
//!
//! Everything else in the CLI answers a question the user already knew to ask.
//! This answers the one they have before they know anything: *what do you think
//! you know?* It is deliberately readable rather than complete — a person
//! should be able to scan it in ten seconds and think "yes, that's right" or
//! "no, that's wrong", and the second thought needs somewhere to go, so every
//! rendering points at `graph correct`.
//!
//! Nothing here retrieves. The overview is a projection of the store and the
//! `--about` form is the ordinary hybrid query; this module only decides how
//! the answer reads. Rendering builds a string and printing emits it, so what
//! a person sees is what a test can assert.

use std::fmt::Write as _;
use std::path::Path;

use crate::error::RecallError;
use crate::graph::edge_view::EdgeView;
use crate::graph::inspect::{
    ConfidenceSummary, MemoryOverview, TopicEntity, TopicReport, DOUBTFUL_CONFIDENCE,
    STRONG_CONFIDENCE,
};
use crate::graph::types::MatchSource;
use crate::serve::{AboutArgs, OverviewArgs, Request};
use crate::serve_client;

const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Longest abstract shown whole. Anything longer is a paragraph, and this is a
/// summary.
const MAX_ABSTRACT_CHARS: usize = 140;

/// Report what memory holds — everything, or one subject.
pub async fn run(
    memory_dir: &Path,
    about: Option<&str>,
    per_type: usize,
) -> Result<(), RecallError> {
    require_graph(memory_dir)?;

    let text = match about {
        Some(topic) => {
            let request = Request::About(AboutArgs {
                topic: topic.to_string(),
                limit: per_type,
            });
            let report: TopicReport =
                serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;
            render_topic(&report)
        }
        None => {
            let request = Request::Overview(OverviewArgs { per_type });
            let overview: MemoryOverview =
                serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;
            render_overview(&overview)
        }
    };

    print!("{text}");
    Ok(())
}

fn require_graph(memory_dir: &Path) -> Result<(), RecallError> {
    if memory_dir.join("graph").exists() {
        return Ok(());
    }
    Err(RecallError::NotInitialized(
        "Graph store not initialized. Run `recall-echo graph init` first.".into(),
    ))
}

// ── Overview ─────────────────────────────────────────────────────────────

/// Everything memory holds, as a person reads it.
#[must_use]
pub fn render_overview(overview: &MemoryOverview) -> String {
    let stats = &overview.stats;
    let mut out = format!("{BOLD}What I know{RESET}\n");

    if stats.entity_count == 0 && stats.episode_count == 0 {
        let _ = writeln!(out, "\n  {YELLOW}Nothing yet.{RESET}");
        let _ = writeln!(
            out,
            "  {DIM}Memory fills when sessions end. Once conversations are archived, \
             `recall-echo graph extract --all` distils them into what you see here.{RESET}"
        );
        return out;
    }

    let _ = writeln!(
        out,
        "\n  {} entities · {} relationships · {} conversation fragments",
        stats.entity_count, stats.relationship_count, stats.episode_count
    );

    if stats.entity_count == 0 {
        let _ = writeln!(
            out,
            "\n  {YELLOW}Conversations are stored but nothing has been distilled from them \
             yet.{RESET}"
        );
        let _ = writeln!(out, "  {DIM}recall-echo graph extract --all{RESET}");
        return out;
    }

    write_groups(&mut out, overview);
    write_confidence(&mut out, &overview.confidence);
    write_edge_section(&mut out, "Least certain", &overview.uncertain, None);
    write_edge_section(
        &mut out,
        "Believed partly because I kept saying it",
        &overview.self_reinforced,
        Some(
            "self×N counts corroborations I produced myself. They are kept out of the \
             confidence above — repetition is coherence, not evidence.",
        ),
    );

    let _ = writeln!(
        out,
        "\n  {DIM}Wrong about something? recall-echo graph correct \"<name>\" --wrong{RESET}"
    );
    out
}

fn write_groups(out: &mut String, overview: &MemoryOverview) {
    for group in &overview.groups {
        let _ = writeln!(
            out,
            "\n  {BOLD}{}{RESET} {DIM}({}){RESET}",
            group.entity_type, group.count
        );
        for entity in &group.top {
            let _ = writeln!(
                out,
                "    {BOLD}{}{RESET} — {}",
                entity.name,
                clip(&entity.abstract_text)
            );
        }
        let listed = group.top.len() as u64;
        if group.count > listed {
            let _ = writeln!(out, "    {DIM}… and {} more{RESET}", group.count - listed);
        }
    }
}

fn write_confidence(out: &mut String, summary: &ConfidenceSummary) {
    if summary.total() == 0 {
        let _ = writeln!(
            out,
            "\n  {YELLOW}No relationships yet{RESET} — I know these things but have not \
             connected them."
        );
        return;
    }

    let _ = writeln!(out, "\n  {BOLD}How sure I am{RESET}");
    let _ = writeln!(
        out,
        "    {} of {} relationships firmly held {DIM}(≥{:.0}%){RESET}, {} uncertain, \
         {} doubtful {DIM}(<{:.0}%){RESET}",
        summary.strong,
        summary.total(),
        STRONG_CONFIDENCE * 100.0,
        summary.uncertain,
        summary.doubtful,
        DOUBTFUL_CONFIDENCE * 100.0,
    );
    if is_mostly_unsure(summary) {
        let _ = writeln!(
            out,
            "    {DIM}Most of what I hold is unsettled — treat it as a starting point, not as \
             fact.{RESET}"
        );
    }
}

/// True when the graph believes less than half of what it holds firmly.
fn is_mostly_unsure(summary: &ConfidenceSummary) -> bool {
    summary.total() > 0 && summary.strong * 2 < summary.total()
}

fn write_edge_section(out: &mut String, heading: &str, edges: &[EdgeView], note: Option<&str>) {
    if edges.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n  {BOLD}{heading}{RESET}");
    for edge in edges {
        write_edge_line(out, edge);
    }
    if let Some(note) = note {
        let _ = writeln!(out, "    {DIM}{note}{RESET}");
    }
}

fn write_edge_line(out: &mut String, edge: &EdgeView) {
    let _ = writeln!(
        out,
        "    {} {CYAN}—[{}]→{RESET} {}  {} {DIM}({:.0}%, evidence {:.1}){RESET}{}",
        edge.from,
        edge.rel_type,
        edge.to,
        certainty(edge.confidence),
        edge.confidence * 100.0,
        edge.evidence,
        coherence_tag(edge),
    );
}

// ── One subject ──────────────────────────────────────────────────────────

/// What memory holds about one subject, as a person reads it.
#[must_use]
pub fn render_topic(report: &TopicReport) -> String {
    let mut out = format!("{BOLD}What I know about \"{}\"{RESET}\n", report.topic);

    if report.entities.is_empty() {
        let _ = writeln!(out, "\n  {YELLOW}Nothing distilled about that.{RESET}");
        let _ = writeln!(
            out,
            "  {DIM}The raw conversations may still hold it: \
             recall-echo graph query \"{}\" --episodes{RESET}",
            report.topic
        );
        return out;
    }

    for entity in &report.entities {
        write_topic_entity(&mut out, entity);
    }

    let _ = writeln!(
        out,
        "\n  {DIM}Wrong about something? recall-echo graph correct \"<from>\" \"<REL>\" \
         \"<to>\" --wrong{RESET}"
    );
    out
}

fn write_topic_entity(out: &mut String, entity: &TopicEntity) {
    let _ = writeln!(
        out,
        "\n  {BOLD}{}{RESET} {DIM}— {} · {} (score {:.2}){RESET}",
        entity.entity.name,
        entity.entity.entity_type,
        how_it_was_found(&entity.source),
        entity.score,
    );
    let _ = writeln!(out, "    {}", clip(&entity.entity.abstract_text));

    if entity.edges.is_empty() {
        let _ = writeln!(out, "    {DIM}Nothing else is recorded about it.{RESET}");
        return;
    }
    for edge in &entity.edges {
        write_edge_line(out, edge);
    }
    if entity.edges_omitted > 0 {
        let _ = writeln!(
            out,
            "    {DIM}… and {} more relationships{RESET}",
            entity.edges_omitted
        );
    }
}

fn how_it_was_found(source: &MatchSource) -> String {
    match source {
        MatchSource::Semantic => "matched directly".to_string(),
        MatchSource::Keyword => "matched by keyword".to_string(),
        MatchSource::Graph { parent, rel_type } => format!("reached from {parent} via {rel_type}"),
    }
}

// ── Wording ──────────────────────────────────────────────────────────────

/// How a posterior mean reads out loud.
///
/// The bands are the ones [`crate::graph::inspect`] counts by, so the summary
/// line and the per-edge wording can never disagree.
#[must_use]
pub fn certainty(confidence: f64) -> &'static str {
    match confidence {
        c if c >= 0.9 => "near-certain",
        c if c >= STRONG_CONFIDENCE => "confident",
        c if c >= DOUBTFUL_CONFIDENCE => "unsure",
        _ => "doubtful",
    }
}

/// The "some of this is me agreeing with myself" marker, when there is one.
fn coherence_tag(edge: &EdgeView) -> String {
    if edge.self_reinforcements > 0 {
        format!(" {YELLOW}self×{}{RESET}", edge.self_reinforcements)
    } else {
        String::new()
    }
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_ABSTRACT_CHARS {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(MAX_ABSTRACT_CHARS).collect();
    clipped.push_str(" […]");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::inspect::{KnownEntity, TypeGroup};
    use crate::graph::types::{EntityDetail, EntityType, GraphStats};

    fn stats(entities: u64, relationships: u64, episodes: u64) -> GraphStats {
        GraphStats {
            entity_count: entities,
            relationship_count: relationships,
            episode_count: episodes,
            entity_type_counts: Default::default(),
            ..Default::default()
        }
    }

    fn edge(confidence: f64, self_reinforcements: i64) -> EdgeView {
        EdgeView {
            id: "relates_to:1".into(),
            from: "Echo".into(),
            to: "NixOS".into(),
            rel_type: "USES".into(),
            description: None,
            confidence,
            evidence: 12.4,
            self_reinforcements,
            superseded: false,
        }
    }

    fn overview_with(groups: Vec<TypeGroup>, stats: GraphStats) -> MemoryOverview {
        MemoryOverview {
            stats,
            groups,
            confidence: ConfidenceSummary::default(),
            uncertain: Vec::new(),
            self_reinforced: Vec::new(),
        }
    }

    fn one_group() -> Vec<TypeGroup> {
        vec![TypeGroup {
            entity_type: "project".into(),
            count: 4,
            top: vec![KnownEntity {
                id: "entity:recall-echo".into(),
                name: "recall-echo".into(),
                entity_type: "project".into(),
                abstract_text: "Persistent memory with a knowledge graph.".into(),
                access_count: 9,
                utility_score: 0.8,
            }],
        }]
    }

    #[test]
    fn an_empty_graph_says_so_and_says_what_fills_it() {
        let text = render_overview(&overview_with(Vec::new(), stats(0, 0, 0)));
        assert!(text.contains("Nothing yet."), "{text}");
        assert!(text.contains("graph extract --all"), "{text}");
        // Confidence over nothing is not a number worth printing.
        assert!(!text.contains("firmly held"), "{text}");
    }

    #[test]
    fn conversations_without_entities_point_at_extraction() {
        let text = render_overview(&overview_with(Vec::new(), stats(0, 0, 120)));
        assert!(text.contains("120 conversation fragments"), "{text}");
        assert!(text.contains("nothing has been distilled"), "{text}");
    }

    #[test]
    fn entities_without_relationships_say_they_are_unconnected() {
        let text = render_overview(&overview_with(one_group(), stats(4, 0, 12)));
        assert!(text.contains("recall-echo"), "{text}");
        assert!(text.contains("… and 3 more"), "{text}");
        assert!(text.contains("No relationships yet"), "{text}");
        assert!(!text.contains("firmly held"), "{text}");
    }

    #[test]
    fn a_self_reinforced_edge_shows_its_tally_and_what_it_means() {
        let mut overview = overview_with(one_group(), stats(4, 3, 12));
        overview.confidence = ConfidenceSummary {
            strong: 3,
            uncertain: 0,
            doubtful: 0,
        };
        overview.self_reinforced = vec![edge(0.88, 23)];
        let text = render_overview(&overview);

        assert!(text.contains("self×23"), "{text}");
        assert!(
            text.contains("repetition is coherence, not evidence"),
            "{text}"
        );
        assert!(text.contains("—[USES]→"), "{text}");
        assert!(text.contains("NixOS"), "{text}");
    }

    #[test]
    fn an_edge_nobody_repeated_carries_no_tally() {
        let mut overview = overview_with(one_group(), stats(4, 1, 12));
        overview.confidence = ConfidenceSummary {
            strong: 0,
            uncertain: 0,
            doubtful: 1,
        };
        overview.uncertain = vec![edge(0.31, 0)];
        let text = render_overview(&overview);

        assert!(text.contains("Least certain"), "{text}");
        assert!(text.contains("doubtful"), "{text}");
        assert!(!text.contains("self×"), "{text}");
        // Believing less than half of itself is worth admitting.
        assert!(text.contains("Most of what I hold is unsettled"), "{text}");
    }

    #[test]
    fn every_overview_offers_the_way_to_correct_it() {
        let text = render_overview(&overview_with(one_group(), stats(4, 0, 1)));
        assert!(text.contains("graph correct"), "{text}");
    }

    fn topic_entity(edges: Vec<EdgeView>, omitted: usize) -> TopicEntity {
        TopicEntity {
            entity: EntityDetail {
                id: serde_json::json!("entity:nixos"),
                name: "NixOS".into(),
                entity_type: EntityType::Tool,
                abstract_text: "Declarative Linux distribution.".into(),
                overview: String::new(),
                attributes: None,
                access_count: 3,
                utility_score: 0.5,
                updated_at: serde_json::json!("2026-05-01T09:15:30Z"),
                source: None,
            },
            score: 0.81,
            source: MatchSource::Semantic,
            edges,
            edges_omitted: omitted,
        }
    }

    #[test]
    fn a_topic_reads_as_claims_with_certainty_attached() {
        let report = TopicReport {
            topic: "nixos".into(),
            entities: vec![topic_entity(vec![edge(0.92, 4)], 3)],
        };
        let text = render_topic(&report);

        assert!(text.contains("What I know about \"nixos\""), "{text}");
        assert!(text.contains("NixOS"), "{text}");
        assert!(text.contains("matched directly"), "{text}");
        assert!(text.contains("near-certain"), "{text}");
        assert!(text.contains("self×4"), "{text}");
        assert!(text.contains("… and 3 more relationships"), "{text}");
    }

    #[test]
    fn a_subject_with_no_entities_points_at_the_raw_conversations() {
        let text = render_topic(&TopicReport {
            topic: "docker".into(),
            entities: Vec::new(),
        });
        assert!(text.contains("Nothing distilled"), "{text}");
        assert!(text.contains("--episodes"), "{text}");
    }

    #[test]
    fn an_entity_with_no_relationships_says_that_plainly() {
        let text = render_topic(&TopicReport {
            topic: "nixos".into(),
            entities: vec![topic_entity(Vec::new(), 0)],
        });
        assert!(text.contains("Nothing else is recorded about it"), "{text}");
    }

    #[test]
    fn certainty_matches_the_bands_the_summary_counts_by() {
        assert_eq!(certainty(1.0), "near-certain");
        assert_eq!(certainty(STRONG_CONFIDENCE), "confident");
        assert_eq!(certainty(DOUBTFUL_CONFIDENCE), "unsure");
        assert_eq!(certainty(0.1), "doubtful");
    }

    #[test]
    fn a_graph_believing_less_than_half_of_itself_says_so() {
        assert!(is_mostly_unsure(&ConfidenceSummary {
            strong: 4,
            uncertain: 5,
            doubtful: 1,
        }));
        assert!(!is_mostly_unsure(&ConfidenceSummary {
            strong: 6,
            uncertain: 4,
            doubtful: 0,
        }));
        assert!(!is_mostly_unsure(&ConfidenceSummary::default()));
    }

    #[test]
    fn long_abstracts_are_clipped_on_a_character_boundary() {
        let long = "é".repeat(MAX_ABSTRACT_CHARS * 2);
        let clipped = clip(&long);
        assert!(clipped.ends_with(" […]"));
        assert!(clipped.chars().count() < long.chars().count());
        assert_eq!(clip("  short  "), "short");
    }

    #[test]
    fn only_a_non_zero_tally_is_shown() {
        assert!(coherence_tag(&edge(0.88, 0)).is_empty());
        assert!(coherence_tag(&edge(0.88, 23)).contains("self×23"));
    }
}
