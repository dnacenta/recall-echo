// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Provenance-weighted evidence tests (Phase 1, increment 2).
//!
//! Three properties are under assertion:
//!
//! - **AC2** — corroboration authored by the agent moves the mean by no more
//!   than its weight fraction, lands in the `self_reinforcements` tally, and
//!   is outweighed by a single independent contradiction.
//! - **AC7** — an episode with absent or unrecognised provenance reads back as
//!   `self`, so a legacy store never gains confidence from backfilled data.
//! - **AC8** — weights of (1, 1, 1) reproduce provenance-blind evidence
//!   bit-for-bit: the escape hatch, and the differential-testing lever.
//!
//! Every fixture here is built with raw SurrealQL rather than `GraphMemory`,
//! so no embedding model is needed: evidence is arithmetic on edges.

use std::path::Path;

use recall_echo::graph::confidence::{
    EdgeEvidence, Evidence, Observation, Provenance, ProvenanceWeights, DEFAULT_EVIDENCE_WEIGHT,
    PRIOR_CONCENTRATION,
};
use recall_echo::graph::crud;
use recall_echo::graph::store::{self, Db};
use recall_echo::graph::types::{Episode, Relationship};
use surrealdb::Surreal;
use tempfile::TempDir;

const ALICE: &str = "entity:alice";
const BOB: &str = "entity:bob";

/// The edge every scenario starts from: an Inferred fact at the prior.
const CLAIM: &str = "INFERRED";
const CLAIM_MEAN: f64 = 0.6;

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ── Fixture plumbing ─────────────────────────────────────────────────

async fn open_store(path: &Path) -> Surreal<Db> {
    let db = store::open(path).await.expect("failed to open store");
    store::init_schema(&db)
        .await
        .expect("failed to init schema");
    db
}

/// Close the store so the embedded backend releases its process lock.
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

/// An edge as a current build writes it: mean, counts and an empty coherence
/// tally.
async fn create_edge(db: &Surreal<Db>, confidence: f64) {
    let evidence = Evidence::from_prior(confidence);
    db.query(
        r#"
        LET $from = type::record($from_id);
        LET $to = type::record($to_id);
        RELATE $from -> relates_to -> $to SET
            rel_type = $rel_type,
            description = 'fixture edge',
            valid_from = time::now(),
            valid_until = NONE,
            confidence = $confidence,
            alpha = $alpha,
            beta = $beta,
            self_reinforcements = 0,
            last_reinforced = time::now(),
            source = 'fixture'
        "#,
    )
    .bind(("from_id", ALICE.to_string()))
    .bind(("to_id", BOB.to_string()))
    .bind(("rel_type", CLAIM.to_string()))
    .bind(("confidence", confidence))
    .bind(("alpha", evidence.alpha()))
    .bind(("beta", evidence.beta()))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create edge: {e}"));
}

/// An edge from a store migrated by increment 1 and never observed since: it
/// has counts but no coherence tally at all.
async fn create_edge_without_tally(db: &Surreal<Db>, confidence: f64) {
    create_edge(db, confidence).await;
    db.query("UPDATE relates_to SET self_reinforcements = NONE")
        .await
        .and_then(surrealdb::IndexedResults::check)
        .expect("failed to clear the coherence tally");
}

async fn edge(db: &Surreal<Db>) -> Relationship {
    crud::list_all_relationships(db)
        .await
        .expect("failed to list relationships")
        .into_iter()
        .find(|r| r.rel_type == CLAIM)
        .expect("fixture edge missing")
}

async fn seed_claim(db: &Surreal<Db>) {
    create_entity(db, ALICE, "Alice").await;
    create_entity(db, BOB, "Bob").await;
    create_edge(db, CLAIM_MEAN).await;
}

/// Apply one observation to the fixture edge through the real write path.
///
/// The direction reaches the write as a value, so a contradiction here is
/// stored the way ingestion stores one — counts down, decay anchor untouched.
/// See `tests/decay_anchor.rs` for why that separation exists.
async fn observe(db: &Surreal<Db>, provenance: Provenance, corroborates: bool) {
    let weights = ProvenanceWeights::default();
    let observation = if corroborates {
        Observation::Corroborating
    } else {
        Observation::Contradicting
    };
    let existing = edge(db).await;
    let mut evidence = existing.edge_evidence();
    evidence.record(observation, provenance, &weights);
    crud::record_observation(db, &existing.id_string(), evidence, observation)
        .await
        .expect("failed to record observation");
}

async fn corroborate(db: &Surreal<Db>, provenance: Provenance, times: usize) {
    for _ in 0..times {
        observe(db, provenance, true).await;
    }
}

/// Write an episode the way a store of a given vintage holds it: `None` is a
/// pre-provenance episode, `Some(class)` a stamped one (including classes no
/// build ever wrote).
async fn create_episode(db: &Surreal<Db>, session_id: &str, provenance: Option<&str>) {
    db.query(
        r#"CREATE episode SET
               session_id = $session_id,
               timestamp = time::now(),
               abstract = $session_id,
               overview = NONE,
               content = NONE,
               log_number = NONE,
               provenance = $provenance"#,
    )
    .bind(("session_id", session_id.to_string()))
    .bind(("provenance", provenance.map(str::to_string)))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create episode {session_id}: {e}"));
}

async fn episode(db: &Surreal<Db>, session_id: &str) -> Episode {
    crud::get_episodes_by_session(db, session_id)
        .await
        .expect("failed to read episodes")
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no episode for session {session_id}"))
}

// ── AC8: uniform weights are provenance-blind ────────────────────────

/// A scripted run of observations, mixing classes and directions. Applied
/// once through the provenance-blind path increment 1 shipped, and once
/// through the provenance-aware path with every class weighted the same.
const SCRIPT: &[(Provenance, bool)] = &[
    (Provenance::External, true),
    (Provenance::SelfGenerated, true),
    (Provenance::User, true),
    (Provenance::SelfGenerated, false),
    (Provenance::External, false),
    (Provenance::SelfGenerated, true),
    (Provenance::User, false),
    (Provenance::SelfGenerated, true),
];

/// The script replayed without any notion of provenance — one observation,
/// one count.
fn provenance_blind_replay() -> Evidence {
    let mut evidence = Evidence::from_prior(CLAIM_MEAN);
    for (_, corroborates) in SCRIPT {
        if *corroborates {
            evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
        } else {
            evidence.contradict(DEFAULT_EVIDENCE_WEIGHT);
        }
    }
    evidence
}

/// The script replayed with provenance, under the given weights.
fn provenance_aware_replay(weights: &ProvenanceWeights) -> EdgeEvidence {
    let mut evidence = EdgeEvidence::new(Evidence::from_prior(CLAIM_MEAN), 0);
    for (provenance, corroborates) in SCRIPT {
        if *corroborates {
            evidence.corroborate(*provenance, weights);
        } else {
            evidence.contradict(*provenance, weights);
        }
    }
    evidence
}

#[test]
fn uniform_weights_reproduce_provenance_blind_evidence_bit_for_bit() {
    let blind = provenance_blind_replay();
    let aware = provenance_aware_replay(&ProvenanceWeights::uniform(DEFAULT_EVIDENCE_WEIGHT));

    assert_eq!(
        aware.evidence().alpha().to_bits(),
        blind.alpha().to_bits(),
        "alpha diverged: {} vs {}",
        aware.evidence().alpha(),
        blind.alpha()
    );
    assert_eq!(
        aware.evidence().beta().to_bits(),
        blind.beta().to_bits(),
        "beta diverged: {} vs {}",
        aware.evidence().beta(),
        blind.beta()
    );
    assert_eq!(aware.evidence(), blind);

    // The coherence tally is a separate signal, not part of the posterior:
    // it keeps counting even when the weights stop distinguishing classes.
    assert_eq!(aware.self_reinforcements(), 3);
}

#[test]
fn default_weights_diverge_from_provenance_blind_evidence() {
    // Guards the differential test above from being vacuous: the same script
    // under the shipped defaults must *not* reproduce the blind counts.
    let blind = provenance_blind_replay();
    let aware = provenance_aware_replay(&ProvenanceWeights::default());

    assert!(
        aware.evidence().concentration() < blind.concentration(),
        "self-authored observations must carry less weight: {} vs {}",
        aware.evidence().concentration(),
        blind.concentration()
    );
}

// ── AC2: self-corroboration is capped, counted, and outweighed ───────

#[tokio::test]
async fn self_corroboration_is_bounded_by_its_weight_and_tallied() {
    let (_dir, graph_path) = new_graph_dir();
    let weights = ProvenanceWeights::default();
    let runs = 20;

    let db = open_store(&graph_path).await;
    seed_claim(&db).await;
    let before = edge(&db).await.confidence;
    corroborate(&db, Provenance::SelfGenerated, runs).await;
    close(db).await;

    // Reopening proves the tally and the counts are persisted, not in-memory.
    let db = open_store(&graph_path).await;
    let after = edge(&db).await;

    assert_eq!(
        after.self_reinforcements,
        Some(runs as i64),
        "every self-corroboration must stay visible as coherence"
    );

    let spent = runs as f64 * weights.weight_self;
    assert!(
        approx(
            after.evidence().alpha(),
            CLAIM_MEAN * PRIOR_CONCENTRATION + spent
        ),
        "alpha must grow by the self weight only, got {}",
        after.evidence().alpha()
    );
    assert!(
        approx(
            after.evidence().beta(),
            (1.0 - CLAIM_MEAN) * PRIOR_CONCENTRATION
        ),
        "corroboration must not touch beta, got {}",
        after.evidence().beta()
    );

    // The weight-fraction bound: twenty self-corroborations buy no more than
    // the (20 x 0.05 = 1.0) external observations they are worth.
    let equivalent = {
        let mut evidence = Evidence::from_prior(CLAIM_MEAN);
        evidence.corroborate(weights.weight_external);
        evidence.mean()
    };
    assert!(
        after.confidence > before,
        "coherence still moves the mean a little: {before} -> {}",
        after.confidence
    );
    assert!(
        after.confidence <= equivalent + 1e-9,
        "{runs} self-corroborations moved the mean past their weight fraction: {} > {equivalent}",
        after.confidence
    );

    close(db).await;
}

#[tokio::test]
async fn one_external_contradiction_outweighs_a_run_of_self_corroboration() {
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    seed_claim(&db).await;
    let before = edge(&db).await.confidence;

    corroborate(&db, Provenance::SelfGenerated, 20).await;
    let after_coherence = edge(&db).await.confidence;
    assert!(after_coherence > before);

    observe(&db, Provenance::External, false).await;
    let after_contradiction = edge(&db).await;

    assert!(
        after_contradiction.confidence < before,
        "an independent contradiction must undo the whole coherence run: {} vs {before}",
        after_contradiction.confidence
    );
    assert_eq!(
        after_contradiction.self_reinforcements,
        Some(20),
        "contradiction is not coherence — the tally holds"
    );

    close(db).await;
}

#[tokio::test]
async fn external_corroboration_leaves_the_coherence_tally_alone() {
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    seed_claim(&db).await;
    corroborate(&db, Provenance::External, 3).await;
    corroborate(&db, Provenance::User, 2).await;
    let after = edge(&db).await;

    assert_eq!(after.self_reinforcements, Some(0));
    let weights = ProvenanceWeights::default();
    assert!(
        approx(
            after.evidence().alpha(),
            CLAIM_MEAN * PRIOR_CONCENTRATION
                + 3.0 * weights.weight_external
                + 2.0 * weights.weight_user
        ),
        "alpha must sum the observed weights, got {}",
        after.evidence().alpha()
    );

    close(db).await;
}

#[tokio::test]
async fn an_edge_without_a_tally_starts_counting_from_zero() {
    // AC7 on the edge side: increment 1 wrote `self_reinforcements` as an
    // option, so an untouched edge may still hold NONE.
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    create_entity(&db, ALICE, "Alice").await;
    create_entity(&db, BOB, "Bob").await;
    create_edge_without_tally(&db, CLAIM_MEAN).await;

    let legacy = edge(&db).await;
    assert_eq!(legacy.self_reinforcements, None);
    assert_eq!(legacy.edge_evidence().self_reinforcements(), 0);

    corroborate(&db, Provenance::SelfGenerated, 1).await;
    assert_eq!(edge(&db).await.self_reinforcements, Some(1));

    close(db).await;
}

// ── AC7: unlabelled episodes are the agent's own ─────────────────────

#[tokio::test]
async fn stored_episode_provenance_defaults_to_self() {
    let (_dir, graph_path) = new_graph_dir();

    let db = open_store(&graph_path).await;
    create_episode(&db, "legacy", None).await;
    create_episode(&db, "garbled", Some("mostly-true")).await;
    create_episode(&db, "document", Some("external")).await;
    create_episode(&db, "human", Some("user")).await;
    create_episode(&db, "agent", Some("self")).await;

    assert_eq!(
        episode(&db, "legacy").await.provenance(),
        Provenance::SelfGenerated,
        "a pre-provenance episode never earns full weight"
    );
    assert_eq!(
        episode(&db, "garbled").await.provenance(),
        Provenance::SelfGenerated,
        "an unrecognised class is treated as unlabelled"
    );
    assert_eq!(
        episode(&db, "document").await.provenance(),
        Provenance::External
    );
    assert_eq!(episode(&db, "human").await.provenance(), Provenance::User);
    assert_eq!(
        episode(&db, "agent").await.provenance(),
        Provenance::SelfGenerated
    );

    close(db).await;
}
