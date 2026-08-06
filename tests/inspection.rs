// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inspection, end to end through the daemon.
//!
//! What a person is owed by an overview: their own entities back, grouped and
//! counted; an honest split of how firmly the relationships are held; the
//! least certain ones named; and the coherence tally shown wherever it is
//! non-zero, because "believed because I kept saying it" is the one thing a
//! confidence number cannot tell you on its own.
//!
//! Fixtures are raw SurrealQL and the overview is a projection, so no embedding
//! model is loaded. The `--about` form runs the ordinary hybrid query and is
//! covered where retrieval is covered; what is asserted here is the surface.

mod common;

use common::daemon::{self, Edge, Fixture};
use recall_echo::graph::inspect::MemoryOverview;
use recall_echo::graph::store::Db;
use recall_echo::inspect_cli::render_overview;
use recall_echo::serve::{OverviewArgs, Request};
use recall_echo::serve_client;
use std::path::Path;
use surrealdb::Surreal;

async fn overview(memory_dir: &Path) -> MemoryOverview {
    let request = Request::Overview(OverviewArgs { per_type: 3 });
    let data = serve_client::execute(memory_dir, &request)
        .await
        .expect("overview request");
    serde_json::from_value(data).expect("decode overview")
}

/// Two people, three tools, and four claims of varying quality.
async fn seed(db: &Surreal<Db>) {
    daemon::create_entity(db, "entity:d", "D", "person").await;
    daemon::create_entity(db, "entity:echo", "Echo", "person").await;
    daemon::create_entity(db, "entity:vim", "Vim", "tool").await;
    daemon::create_entity(db, "entity:nixos", "NixOS", "tool").await;
    daemon::create_entity(db, "entity:rust", "Rust", "tool").await;

    // Firmly held.
    daemon::create_edge(
        db,
        &Edge {
            from: "entity:d",
            to: "entity:rust",
            rel_type: "LEARNS",
            alpha: 19.0,
            beta: 1.0,
            self_reinforcements: 0,
        },
    )
    .await;
    // Held mostly because the agent kept saying so.
    daemon::create_edge(
        db,
        &Edge {
            from: "entity:echo",
            to: "entity:nixos",
            rel_type: "USES",
            alpha: 17.0,
            beta: 3.0,
            self_reinforcements: 23,
        },
    )
    .await;
    // Barely believed.
    daemon::create_edge(
        db,
        &Edge {
            from: "entity:d",
            to: "entity:vim",
            rel_type: "PREFERS",
            alpha: 3.0,
            beta: 7.0,
            self_reinforcements: 1,
        },
    )
    .await;
}

#[tokio::test]
async fn an_overview_returns_the_graph_grouped_and_counted() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed(&db).await;
    daemon::close(db).await;

    let overview = overview(&fixture.memory_dir).await;

    assert_eq!(overview.stats.entity_count, 5);
    assert_eq!(overview.stats.relationship_count, 3);

    // Most populous type first, so the first thing read is the biggest thing
    // known.
    let types: Vec<&str> = overview
        .groups
        .iter()
        .map(|group| group.entity_type.as_str())
        .collect();
    assert_eq!(types, vec!["tool", "person"]);

    let tools = &overview.groups[0];
    assert_eq!(
        tools.count, 3,
        "the count is of the type, not of the listing"
    );
    assert_eq!(tools.top.len(), 3);
    let mut names: Vec<&str> = tools.top.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["NixOS", "Rust", "Vim"]);
}

#[tokio::test]
async fn an_overview_is_honest_about_how_sure_it_is() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed(&db).await;
    daemon::close(db).await;

    let overview = overview(&fixture.memory_dir).await;

    assert_eq!(overview.confidence.total(), 3);
    assert_eq!(overview.confidence.strong, 2, "0.95 and 0.85");
    assert_eq!(overview.confidence.doubtful, 1, "0.30");

    // Not-strong edges are named, strongest-held ones are not: the point of
    // the section is where to look, not a second copy of the graph.
    let uncertain: Vec<&str> = overview
        .uncertain
        .iter()
        .map(|edge| edge.rel_type.as_str())
        .collect();
    assert_eq!(uncertain, vec!["PREFERS"]);
    assert_eq!(overview.uncertain[0].from, "D", "names, not record ids");
    assert_eq!(overview.uncertain[0].to, "Vim");
}

#[tokio::test]
async fn an_overview_surfaces_the_coherence_tally() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed(&db).await;
    daemon::close(db).await;

    let overview = overview(&fixture.memory_dir).await;

    // Both self-reinforced edges are listed, the loudest first; the edge
    // nobody repeated is not.
    let tallies: Vec<i64> = overview
        .self_reinforced
        .iter()
        .map(|edge| edge.self_reinforcements)
        .collect();
    assert_eq!(tallies, vec![23, 1], "{:?}", overview.self_reinforced);
    assert!(
        !overview
            .self_reinforced
            .iter()
            .any(|edge| edge.rel_type == "LEARNS"),
        "an edge nobody repeated is not a coherence risk"
    );

    let text = render_overview(&overview);
    assert!(text.contains("self×23"), "{text}");
    assert!(
        text.contains("repetition is coherence, not evidence"),
        "the tally means nothing without the sentence: {text}"
    );
}

#[tokio::test]
async fn an_empty_graph_says_it_is_empty() {
    let fixture = Fixture::new();

    let overview = overview(&fixture.memory_dir).await;
    assert_eq!(overview.stats.entity_count, 0);
    assert!(overview.groups.is_empty());
    assert_eq!(overview.confidence.total(), 0);

    let text = render_overview(&overview);
    assert!(text.contains("Nothing yet"), "{text}");
    assert!(!text.contains("firmly held"), "{text}");
}

#[tokio::test]
async fn entities_without_relationships_are_reported_as_unconnected() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    daemon::create_entity(&db, "entity:d", "D", "person").await;
    daemon::create_entity(&db, "entity:vim", "Vim", "tool").await;
    daemon::close(db).await;

    let overview = overview(&fixture.memory_dir).await;
    assert_eq!(overview.stats.entity_count, 2);
    assert_eq!(overview.stats.relationship_count, 0);
    assert_eq!(overview.confidence.total(), 0);
    assert!(overview.uncertain.is_empty());
    assert!(overview.self_reinforced.is_empty());

    let text = render_overview(&overview);
    assert!(text.contains("No relationships yet"), "{text}");
    assert!(text.contains("Vim"), "{text}");
}
