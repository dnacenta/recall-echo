// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Contradiction and the decay clock.
//!
//! Confidence is read through temporal decay, anchored on `last_reinforced`.
//! Corroboration moves that anchor — the belief was just seen again. A
//! contradiction must not, or denying a claim would *raise* the number the
//! read paths score on: a stale edge stored at 0.6 and decayed to 0.3 comes
//! back undecayed at 0.545, more visible after being denied than before.
//!
//! The property under assertion is the one a user can feel: **telling memory
//! something is wrong never makes it more confident.** It is pinned on both
//! write paths — the human correction in `graph correct`, and the extraction
//! path, whose observation direction is a value passed to the write rather than
//! an assumption baked into it.
//!
//! Fixtures are raw SurrealQL: decay is arithmetic on an edge, so no embedding
//! model is loaded.

use std::path::Path;

use recall_echo::graph::confidence::{
    effective_confidence, EdgeEvidence, Evidence, Observation, Provenance, ProvenanceWeights,
};
use recall_echo::graph::crud;
use recall_echo::graph::store::{self, Db};
use recall_echo::graph::types::Relationship;
use surrealdb::Surreal;
use tempfile::TempDir;

const ALICE: &str = "entity:alice";
const BOB: &str = "entity:bob";
const CLAIM: &str = "INFERRED";

/// The stored mean of the fixture edge: an Inferred fact at the prior.
const CLAIM_MEAN: f64 = 0.6;

/// Two half-lives of neglect — long enough that an anchor reset is worth more
/// than the contradiction takes away.
const STALE_DAYS: i64 = 180;

// ── Fixture plumbing ─────────────────────────────────────────────────

async fn open_store(path: &Path) -> Surreal<Db> {
    let db = store::open(path).await.expect("failed to open store");
    store::init_schema(&db)
        .await
        .expect("failed to init schema");
    db
}

async fn close(db: Surreal<Db>) {
    drop(db);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

fn new_graph_dir() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("failed to create graph dir");
    (dir, graph_path)
}

async fn create_entity(db: &Surreal<Db>, id: &str, name: &str) {
    db.query(
        r#"CREATE type::record($id) SET
               name = $name,
               entity_type = 'concept',
               abstract = $name,
               overview = '',
               mutable = true,
               access_count = 0,
               created_at = time::now(),
               updated_at = time::now()"#,
    )
    .bind(("id", id.to_string()))
    .bind(("name", name.to_string()))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create entity {id}: {e}"));
}

/// An edge last seen `STALE_DAYS` ago: believed at the prior, and decayed.
async fn create_stale_edge(db: &Surreal<Db>) {
    let evidence = Evidence::from_prior(CLAIM_MEAN);
    let stale = (chrono::Utc::now() - chrono::Duration::days(STALE_DAYS)).to_rfc3339();

    db.query(
        r#"
        LET $from = type::record($from_id);
        LET $to = type::record($to_id);
        RELATE $from -> relates_to -> $to SET
            rel_type = $rel_type,
            description = 'fixture edge',
            valid_from = type::datetime($stale),
            valid_until = NONE,
            confidence = $confidence,
            alpha = $alpha,
            beta = $beta,
            self_reinforcements = 0,
            last_reinforced = type::datetime($stale),
            source = 'fixture'
        "#,
    )
    .bind(("from_id", ALICE.to_string()))
    .bind(("to_id", BOB.to_string()))
    .bind(("rel_type", CLAIM.to_string()))
    .bind(("stale", stale))
    .bind(("confidence", CLAIM_MEAN))
    .bind(("alpha", evidence.alpha()))
    .bind(("beta", evidence.beta()))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create edge: {e}"));
}

async fn seed_stale_claim(db: &Surreal<Db>) {
    create_entity(db, ALICE, "Alice").await;
    create_entity(db, BOB, "Bob").await;
    create_stale_edge(db).await;
}

async fn edge(db: &Surreal<Db>) -> Relationship {
    crud::list_all_relationships(db)
        .await
        .expect("failed to list relationships")
        .into_iter()
        .find(|r| r.rel_type == CLAIM)
        .expect("fixture edge missing")
}

/// What a read path would score the edge at: the stored mean, decayed.
fn effective(edge: &Relationship) -> f64 {
    effective_confidence(
        edge.confidence,
        edge.last_reinforced.as_ref(),
        &edge.valid_from,
        &chrono::Utc::now(),
    )
}

/// One observation applied through the write path that owns it.
async fn observe(db: &Surreal<Db>, observation: Observation, provenance: Provenance) {
    let weights = ProvenanceWeights::default();
    let existing = edge(db).await;
    let mut evidence: EdgeEvidence = existing.edge_evidence();
    evidence.record(observation, provenance, &weights);
    crud::record_observation(db, &existing.id_string(), evidence, observation)
        .await
        .expect("failed to record observation");
}

// ── The property ─────────────────────────────────────────────────────

/// The correction path: a human saying "that's wrong" must never leave the
/// claim scoring higher than it did.
#[tokio::test]
async fn a_human_contradiction_never_raises_effective_confidence() {
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    seed_stale_claim(&db).await;

    let before = edge(&db).await;
    let effective_before = effective(&before);
    assert!(
        effective_before < before.confidence,
        "the fixture must be decayed for this to mean anything: {effective_before} vs {}",
        before.confidence
    );

    let mut evidence = before.edge_evidence();
    evidence.contradict(Provenance::User, &ProvenanceWeights::default());
    crud::contradict_relationship(&db, &before.id_string(), evidence)
        .await
        .expect("failed to contradict");

    let after = edge(&db).await;
    assert!(
        after.confidence < before.confidence,
        "the posterior mean must fall: {} -> {}",
        before.confidence,
        after.confidence
    );
    assert!(
        effective(&after) < effective_before,
        "a contradiction must not raise what read paths score: {effective_before} -> {}",
        effective(&after)
    );
    assert_eq!(
        after.last_reinforced, before.last_reinforced,
        "the decay anchor must stay where it was"
    );

    close(db).await;
}

/// The extraction path: same property, through the write ingestion uses.
///
/// Extraction only ever corroborates today, so this pins the mechanism rather
/// than a reachable regression — which is the point. The direction of the
/// observation, not the call site, is what decides the anchor.
#[tokio::test]
async fn an_extracted_contradiction_never_raises_effective_confidence() {
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    seed_stale_claim(&db).await;

    let before = edge(&db).await;
    let effective_before = effective(&before);

    // Every class of source, including the one weighted highest.
    for provenance in [
        Provenance::SelfGenerated,
        Provenance::User,
        Provenance::External,
    ] {
        observe(&db, Observation::Contradicting, provenance).await;
        let after = edge(&db).await;
        assert!(
            effective(&after) <= effective_before,
            "{provenance} contradiction raised effective confidence: \
             {effective_before} -> {}",
            effective(&after)
        );
        assert_eq!(
            after.last_reinforced, before.last_reinforced,
            "{provenance} contradiction moved the decay anchor"
        );
    }

    close(db).await;
}

/// The other half of the rule, so the assertions above are not vacuous:
/// corroboration *does* restart decay, which is what makes resetting the
/// anchor on a contradiction such an attractive mistake.
#[tokio::test]
async fn corroboration_still_restarts_the_decay_clock() {
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    seed_stale_claim(&db).await;

    let before = edge(&db).await;
    let effective_before = effective(&before);

    observe(&db, Observation::Corroborating, Provenance::External).await;

    let after = edge(&db).await;
    assert!(
        effective(&after) > effective_before,
        "an edge just seen again should stop decaying: {effective_before} -> {}",
        effective(&after)
    );
    assert_ne!(
        after.last_reinforced, before.last_reinforced,
        "corroboration is what moves the anchor"
    );

    close(db).await;
}
