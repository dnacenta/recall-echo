// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Human correction, end to end through the daemon.
//!
//! Three properties, in order of how much they matter:
//!
//! - **A correction is evidence.** `--wrong` moves the Beta counts and the
//!   posterior mean, and the move survives the daemon and a reopen.
//! - **Nothing is damaged on a guess.** A name that does not resolve, and an
//!   entity whose claims are ambiguous, both come back with candidates and
//!   leave the store byte-identical.
//! - **Destruction is confirmed.** An unconfirmed forget reports what would go
//!   and removes nothing.
//!
//! Fixtures are raw SurrealQL: correction is graph writes and arithmetic, so no
//! embedding model is involved.

mod common;

use common::daemon::{self, Edge, Fixture};
use recall_echo::graph::correct::{CorrectTarget, Correction, CorrectionReport};
use recall_echo::graph::store::Db;
use recall_echo::serve::{CorrectArgs, Request};
use recall_echo::serve_client;
use std::path::Path;
use surrealdb::Surreal;

/// Weight of one user-authored observation at the default provenance weights.
const USER_WEIGHT: f64 = 0.8;

// ── Fixtures ─────────────────────────────────────────────────────────────

/// D uses Vim (strongly held), and Vim is configured by dotfiles.
async fn seed_two_claims(db: &Surreal<Db>) -> (String, String) {
    daemon::create_entity(db, "entity:d", "D", "person").await;
    daemon::create_entity(db, "entity:vim", "Vim", "tool").await;
    daemon::create_entity(db, "entity:dotfiles", "dotfiles", "project").await;

    let uses = daemon::create_edge(
        db,
        &Edge {
            from: "entity:d",
            to: "entity:vim",
            rel_type: "USES",
            alpha: 9.0,
            beta: 1.0,
            self_reinforcements: 0,
        },
    )
    .await;
    let configured = daemon::create_edge(
        db,
        &Edge {
            from: "entity:vim",
            to: "entity:dotfiles",
            rel_type: "CONFIGURED_BY",
            alpha: 6.0,
            beta: 4.0,
            self_reinforcements: 2,
        },
    )
    .await;
    (uses, configured)
}

/// Only the first claim: an entity with exactly one thing said about it.
async fn seed_one_claim(db: &Surreal<Db>) -> String {
    daemon::create_entity(db, "entity:d", "D", "person").await;
    daemon::create_entity(db, "entity:vim", "Vim", "tool").await;
    daemon::create_edge(db, &Edge::default()).await
}

async fn correct(
    memory_dir: &Path,
    target: CorrectTarget,
    correction: Correction,
) -> CorrectionReport {
    let request = Request::Correct(CorrectArgs { target, correction });
    let data = serve_client::execute(memory_dir, &request)
        .await
        .expect("correction request");
    serde_json::from_value(data).expect("decode correction report")
}

fn entity(name: &str) -> CorrectTarget {
    CorrectTarget::Entity {
        name: name.to_string(),
    }
}

fn edge(from: &str, rel_type: &str, to: &str) -> CorrectTarget {
    CorrectTarget::Edge {
        from: from.to_string(),
        rel_type: rel_type.to_string(),
        to: to.to_string(),
    }
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ── Contradiction ────────────────────────────────────────────────────────

#[tokio::test]
async fn contradicting_a_relationship_lowers_confidence_and_persists() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    let uses = seed_one_claim(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::Contradicted { edges } = report else {
        panic!("expected a contradiction, got {report:?}");
    };
    assert_eq!(edges.len(), 1);
    let correction = &edges[0];

    // The user's word is one observation at user weight, not a decree: alpha
    // is untouched, beta grows by the weight, and the mean follows.
    assert!(approx(correction.confidence_before, 0.9));
    assert!(approx(correction.evidence_before, 10.0));
    assert!(
        approx(correction.edge.confidence, 9.0 / (10.0 + USER_WEIGHT)),
        "got {}",
        correction.edge.confidence
    );
    assert!(approx(correction.edge.evidence, 10.0 + USER_WEIGHT));
    assert_eq!(correction.edge.from, "D");
    assert_eq!(correction.edge.to, "Vim");

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    let (confidence, alpha, beta) = daemon::edge_state(&db, &uses)
        .await
        .expect("the edge is still there");
    assert!(approx(alpha, 9.0), "corroboration is not erased: {alpha}");
    assert!(approx(beta, 1.0 + USER_WEIGHT), "got {beta}");
    assert!(approx(confidence, 9.0 / (10.0 + USER_WEIGHT)));
    daemon::close(db).await;
}

#[tokio::test]
async fn saying_it_twice_is_twice_the_evidence() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    let uses = seed_one_claim(&db).await;
    daemon::close(db).await;

    for _ in 0..2 {
        correct(
            &fixture.memory_dir,
            edge("D", "USES", "Vim"),
            Correction::Wrong { all_edges: false },
        )
        .await;
    }

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    let (confidence, _, beta) = daemon::edge_state(&db, &uses).await.expect("edge");
    assert!(approx(beta, 1.0 + 2.0 * USER_WEIGHT), "got {beta}");
    assert!(confidence < 9.0 / (10.0 + USER_WEIGHT));
    daemon::close(db).await;
}

#[tokio::test]
async fn an_entity_with_one_claim_needs_no_disambiguation() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    let uses = seed_one_claim(&db).await;
    daemon::close(db).await;

    // Correcting the entity means correcting what is claimed about it — and
    // with one claim there is nothing to choose between.
    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    let CorrectionReport::Contradicted { edges } = report else {
        panic!("expected a contradiction, got {report:?}");
    };
    assert_eq!(edges[0].edge.id, uses);
}

#[tokio::test]
async fn an_entity_with_several_claims_asks_which_and_changes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    let before = daemon::edges(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::Ambiguous { entity, edges } = report else {
        panic!("expected disambiguation, got {report:?}");
    };
    assert_eq!(entity, "Vim");
    assert_eq!(edges.len(), 2, "both claims are offered: {edges:?}");
    assert!(
        edges.iter().any(|edge| edge.rel_type == "USES")
            && edges.iter().any(|edge| edge.rel_type == "CONFIGURED_BY"),
        "{edges:?}"
    );
    // The one carrying self-authored corroboration says so, unprompted.
    assert_eq!(
        edges
            .iter()
            .find(|edge| edge.rel_type == "CONFIGURED_BY")
            .map(|edge| edge.self_reinforcements),
        Some(2)
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(
        daemon::edges(&db).await,
        before,
        "a refusal to guess must not move a single count"
    );
    daemon::close(db).await;
}

#[tokio::test]
async fn asking_for_all_of_them_contradicts_every_claim() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Wrong { all_edges: true },
    )
    .await;
    let CorrectionReport::Contradicted { edges } = report else {
        panic!("expected a contradiction, got {report:?}");
    };
    assert_eq!(edges.len(), 2);
    assert!(
        edges
            .iter()
            .all(|edge| edge.edge.confidence < edge.confidence_before),
        "{edges:?}"
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    for (id, _, _, beta) in daemon::edges(&db).await {
        assert!(
            beta > 1.0,
            "every claim took a hit — {id} is at beta {beta}"
        );
    }
    daemon::close(db).await;
}

#[tokio::test]
async fn an_entity_nothing_is_claimed_about_has_nothing_to_contradict() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    daemon::create_entity(&db, "entity:lonely", "Lonely", "concept").await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Lonely"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    assert!(
        matches!(report, CorrectionReport::NothingToCorrect { ref entity } if entity == "Lonely"),
        "{report:?}"
    );
}

// ── Refusing to guess ────────────────────────────────────────────────────

#[tokio::test]
async fn a_name_that_does_not_resolve_lists_the_closest_and_changes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    let before = daemon::edges(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("dotfile"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::UnknownEntity { query, candidates } = report else {
        panic!("expected a refusal, got {report:?}");
    };
    assert_eq!(query, "dotfile");
    assert_eq!(
        candidates.first().map(|entity| entity.name.as_str()),
        Some("dotfiles"),
        "the near-miss is offered, not applied: {candidates:?}"
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await, before);
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn a_name_close_to_nothing_offers_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("zzzzqqqq"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    let CorrectionReport::UnknownEntity { candidates, .. } = report else {
        panic!("expected a refusal, got {report:?}");
    };
    assert!(candidates.is_empty(), "{candidates:?}");
}

#[tokio::test]
async fn a_relationship_that_does_not_exist_reports_the_ones_that_do() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "HATES", "Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::NoSuchEdge {
        from,
        to,
        rel_type,
        existing,
    } = report
    else {
        panic!("expected a refusal, got {report:?}");
    };
    assert_eq!(
        (from.as_str(), rel_type.as_str(), to.as_str()),
        ("D", "HATES", "Vim")
    );
    assert_eq!(
        existing
            .iter()
            .map(|edge| edge.rel_type.as_str())
            .collect::<Vec<_>>(),
        vec!["USES"],
        "what does connect them is worth saying"
    );
}

/// The graph stored the claim one way round; the human need not know which.
#[tokio::test]
async fn a_relationship_named_backwards_still_resolves() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_one_claim(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("Vim", "uses", "D"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    assert!(
        matches!(report, CorrectionReport::Contradicted { ref edges } if edges.len() == 1),
        "{report:?}"
    );
}

// ── Forgetting ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unconfirmed_forget_reports_what_would_go_and_removes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: false },
    )
    .await;

    let CorrectionReport::Planned { removal } = report else {
        panic!("expected a plan, got {report:?}");
    };
    assert_eq!(
        removal.entity.as_ref().map(|entity| entity.name.as_str()),
        Some("Vim")
    );
    assert_eq!(
        removal.edges.len(),
        2,
        "the plan names every relationship going with it: {:?}",
        removal.edges
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 2, "nothing was removed");
    assert!(daemon::entity_names(&db).await.contains(&"Vim".to_string()));
    daemon::close(db).await;
}

#[tokio::test]
async fn a_confirmed_forget_removes_the_entity_and_its_relationships() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let CorrectionReport::Removed { removal } = report else {
        panic!("expected a removal, got {report:?}");
    };
    assert_eq!(removal.edges.len(), 2);

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert!(
        daemon::edges(&db).await.is_empty(),
        "an entity's edges go with it"
    );
    assert_eq!(
        daemon::entity_names(&db).await,
        vec!["D".to_string(), "dotfiles".to_string()],
        "its neighbours stay"
    );
    daemon::close(db).await;
}

#[tokio::test]
async fn forgetting_one_relationship_leaves_both_entities() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let CorrectionReport::Removed { removal } = report else {
        panic!("expected a removal, got {report:?}");
    };
    assert!(removal.entity.is_none());
    assert_eq!(removal.edges.len(), 1);

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 1, "only that claim went");
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn forgetting_a_name_that_does_not_resolve_removes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vimm"),
        Correction::Forget { confirmed: true },
    )
    .await;
    assert!(
        matches!(report, CorrectionReport::UnknownEntity { .. }),
        "{report:?}"
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    assert_eq!(daemon::edges(&db).await.len(), 2);
    daemon::close(db).await;
}
