//! Schema-migration tests for persisted edge evidence (Phase 1, increment 1).
//!
//! Three properties are under assertion:
//!
//! - **AC3** — opening a pre-Phase-1 store backfills `alpha`/`beta` from the
//!   stored mean without changing a single mean, and a second open migrates
//!   nothing.
//! - **AC10** — a backfill interrupted part-way (the process dies between the
//!   `UPDATE` and the version marker) completes on the next open, and the
//!   edges it already reached are not counted twice.
//! - **AC1 (persistence half)** — evidence accumulated through the write path
//!   survives a close/reopen cycle, and the posterior keeps narrowing.
//!
//! Every fixture here is built with raw SurrealQL rather than `GraphMemory`,
//! so no embedding model is needed: confidence is arithmetic on edges.

use std::path::Path;

use recall_echo::graph::confidence::{
    Evidence, Provenance, ProvenanceWeights, DEFAULT_EVIDENCE_WEIGHT, PRIOR_CONCENTRATION,
};
use recall_echo::graph::crud;
use recall_echo::graph::store::{self, Db, MigrationReport, SCHEMA_VERSION};
use recall_echo::graph::types::{NewRelationship, Relationship};
use surrealdb::Surreal;
use tempfile::TempDir;

const ALICE: &str = "entity:alice";
const BOB: &str = "entity:bob";
const CARLA: &str = "entity:carla";

const META_RECORD: &str = "meta:schema";

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ── Fixture plumbing ─────────────────────────────────────────────────

/// Open the store at `path` and run schema init + migration, as every
/// entry point into the graph does.
async fn open_store(path: &Path) -> (Surreal<Db>, MigrationReport) {
    let db = store::open(path).await.expect("failed to open store");
    let report = store::init_schema(&db)
        .await
        .expect("failed to init schema");
    (db, report)
}

/// Close the store so the embedded backend releases its process lock.
async fn close(db: Surreal<Db>) {
    drop(db);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

/// Write an edge the way a pre-Phase-1 build did: a bare `confidence` mean,
/// no evidence counts at all.
async fn create_legacy_edge(
    db: &Surreal<Db>,
    from_id: &str,
    to_id: &str,
    rel_type: &str,
    confidence: f64,
) {
    db.query(
        r#"
        LET $from = type::record($from_id);
        LET $to = type::record($to_id);
        RELATE $from -> relates_to -> $to SET
            rel_type = $rel_type,
            description = 'legacy edge',
            valid_from = time::now(),
            valid_until = NONE,
            confidence = $confidence,
            last_reinforced = time::now(),
            source = 'fixture'
        "#,
    )
    .bind(("from_id", from_id.to_string()))
    .bind(("to_id", to_id.to_string()))
    .bind(("rel_type", rel_type.to_string()))
    .bind(("confidence", confidence))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create legacy edge {rel_type}: {e}"));
}

/// Drop the schema-version marker, leaving a store shaped exactly like one
/// written before versioning existed.
async fn strip_version_marker(db: &Surreal<Db>) {
    db.query("DELETE type::record($id)")
        .bind(("id", META_RECORD.to_string()))
        .await
        .and_then(surrealdb::IndexedResults::check)
        .expect("failed to strip version marker");
}

/// Apply the backfill formula to one edge by hand — the state a migration
/// interrupted after touching some rows leaves behind.
async fn backfill_one_edge(db: &Surreal<Db>, rel_id: &str) {
    db.query(
        r#"UPDATE type::record($id) SET
               alpha = confidence * $concentration,
               beta = (1 - confidence) * $concentration,
               self_reinforcements = 0"#,
    )
    .bind(("id", rel_id.to_string()))
    .bind(("concentration", PRIOR_CONCENTRATION))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to pre-backfill {rel_id}: {e}"));
}

async fn edges_by_type(db: &Surreal<Db>) -> Vec<(String, Relationship)> {
    let mut rels = crud::list_all_relationships(db)
        .await
        .expect("failed to list relationships");
    rels.sort_by(|a, b| a.rel_type.cmp(&b.rel_type));
    rels.into_iter().map(|r| (r.rel_type.clone(), r)).collect()
}

async fn edge(db: &Surreal<Db>, rel_type: &str) -> Relationship {
    edges_by_type(db)
        .await
        .into_iter()
        .find(|(t, _)| t == rel_type)
        .unwrap_or_else(|| panic!("no edge of type {rel_type}"))
        .1
}

/// An empty graph directory that lives as long as the returned `TempDir`.
fn new_graph_dir() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("failed to create graph dir");
    (dir, graph_path)
}

/// Three entities and three evidence-less edges, spanning the confidence range.
async fn seed_legacy_edges(db: &Surreal<Db>) {
    create_entity(db, ALICE, "Alice").await;
    create_entity(db, BOB, "Bob").await;
    create_entity(db, CARLA, "Carla").await;

    create_legacy_edge(db, ALICE, BOB, "CERTAIN", 1.0).await;
    create_legacy_edge(db, ALICE, CARLA, "INFERRED", 0.6).await;
    create_legacy_edge(db, BOB, CARLA, "SPECULATIVE", 0.3).await;
}

/// A closed store holding three legacy edges and no version marker — what a
/// pre-Phase-1 build leaves on disk.
async fn legacy_store() -> (TempDir, std::path::PathBuf) {
    let (dir, graph_path) = new_graph_dir();

    let (db, _) = open_store(&graph_path).await;
    seed_legacy_edges(&db).await;
    strip_version_marker(&db).await;
    close(db).await;

    (dir, graph_path)
}

// ── AC3: idempotent, non-destructive migration ───────────────────────

#[tokio::test]
async fn legacy_edges_gain_evidence_with_means_preserved() {
    let (_dir, graph_path) = legacy_store().await;

    let (db, report) = open_store(&graph_path).await;

    assert!(report.ran(), "migration should have run: {report:?}");
    assert_eq!(report.from_version, 0);
    assert_eq!(report.to_version, SCHEMA_VERSION);
    assert_eq!(report.edges_backfilled, 3, "all legacy edges: {report:?}");

    for (rel_type, expected_mean) in [("CERTAIN", 1.0), ("INFERRED", 0.6), ("SPECULATIVE", 0.3)] {
        let rel = edge(&db, rel_type).await;

        assert_eq!(
            rel.confidence, expected_mean,
            "{rel_type}: mean must be preserved exactly"
        );

        let evidence = rel.evidence();
        assert!(
            approx(evidence.concentration(), PRIOR_CONCENTRATION),
            "{rel_type}: concentration {} != {PRIOR_CONCENTRATION}",
            evidence.concentration()
        );
        assert!(
            approx(evidence.mean(), expected_mean),
            "{rel_type}: counts must reproduce the mean, got {}",
            evidence.mean()
        );
        assert_eq!(
            rel.self_reinforcements,
            Some(0),
            "{rel_type}: coherence counter starts empty"
        );
    }

    close(db).await;
}

#[tokio::test]
async fn reopening_a_migrated_store_migrates_nothing() {
    let (_dir, graph_path) = legacy_store().await;

    let (db, first) = open_store(&graph_path).await;
    assert!(first.ran());
    let before: Vec<_> = edges_by_type(&db)
        .await
        .into_iter()
        .map(|(t, r)| (t, r.confidence, r.alpha, r.beta))
        .collect();
    close(db).await;

    let (db, second) = open_store(&graph_path).await;

    assert!(!second.ran(), "second open must be a no-op: {second:?}");
    assert_eq!(second.from_version, SCHEMA_VERSION);
    assert_eq!(second.edges_backfilled, 0);

    let after: Vec<_> = edges_by_type(&db)
        .await
        .into_iter()
        .map(|(t, r)| (t, r.confidence, r.alpha, r.beta))
        .collect();
    assert_eq!(before, after, "a no-op migration must not touch evidence");

    close(db).await;
}

#[tokio::test]
async fn fresh_store_is_current_after_first_open() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("failed to create graph dir");

    let (db, first) = open_store(&graph_path).await;
    assert_eq!(first.to_version, SCHEMA_VERSION);
    assert_eq!(
        first.edges_backfilled, 0,
        "nothing to backfill on an empty store"
    );
    close(db).await;

    let (db, second) = open_store(&graph_path).await;
    assert!(!second.ran(), "fresh store migrates once: {second:?}");
    close(db).await;
}

// ── AC10: interrupted migration ──────────────────────────────────────

#[tokio::test]
async fn interrupted_backfill_completes_without_double_counting() {
    // A process killed mid-backfill: one edge already carries evidence, the
    // rest do not, and the version marker was never written.
    let (_dir, graph_path) = new_graph_dir();

    let (db, _) = open_store(&graph_path).await;
    seed_legacy_edges(&db).await;
    let half_done = edge(&db, "INFERRED").await;
    backfill_one_edge(&db, &half_done.id_string()).await;
    strip_version_marker(&db).await;
    close(db).await;

    let (db, report) = open_store(&graph_path).await;

    assert!(report.ran(), "migration must resume: {report:?}");
    assert_eq!(
        report.edges_backfilled, 2,
        "only the untouched edges are backfilled: {report:?}"
    );

    let resumed = edge(&db, "INFERRED").await;
    assert_eq!(resumed.confidence, 0.6, "mean untouched by the second pass");
    assert!(
        approx(resumed.evidence().concentration(), PRIOR_CONCENTRATION),
        "already-migrated edge must not be counted twice, got {}",
        resumed.evidence().concentration()
    );
    assert!(approx(resumed.evidence().alpha(), 6.0));
    assert!(approx(resumed.evidence().beta(), 4.0));

    let finished = edge(&db, "SPECULATIVE").await;
    assert!(approx(finished.evidence().alpha(), 3.0));
    assert!(approx(finished.evidence().beta(), 7.0));

    close(db).await;
}

// ── AC1: evidence persists across reopen ─────────────────────────────

#[tokio::test]
async fn accumulated_evidence_survives_reopen_and_keeps_narrowing() {
    let (_dir, graph_path) = legacy_store().await;

    // Migrate, then corroborate five times through the real write path.
    let (db, _) = open_store(&graph_path).await;
    corroborate(&db, "INFERRED", 5).await;
    close(db).await;

    let (db, _) = open_store(&graph_path).await;
    let after_five = edge(&db, "INFERRED").await;
    assert!(
        approx(after_five.evidence().alpha(), 11.0),
        "5 observations on top of alpha=6, got {}",
        after_five.evidence().alpha()
    );
    assert!(approx(after_five.evidence().beta(), 4.0));
    assert!(
        approx(after_five.confidence, 11.0 / 15.0),
        "stored mean tracks the counts, got {}",
        after_five.confidence
    );

    corroborate(&db, "INFERRED", 45).await;
    close(db).await;

    let (db, _) = open_store(&graph_path).await;
    let after_fifty = edge(&db, "INFERRED").await;
    assert!(approx(after_fifty.evidence().alpha(), 56.0));
    assert!(
        after_fifty.evidence().variance() < after_five.evidence().variance(),
        "50 observations {} must be tighter than 5 {}",
        after_fifty.evidence().variance(),
        after_five.evidence().variance()
    );

    // A store already at the prior concentration is never re-primed.
    let untouched = edge(&db, "SPECULATIVE").await;
    assert!(approx(
        untouched.evidence().concentration(),
        PRIOR_CONCENTRATION
    ));

    close(db).await;
}

/// Record `times` corroborations on an edge through the write path the
/// ingest pipeline uses, at the provenance-blind reference weight.
async fn corroborate(db: &Surreal<Db>, rel_type: &str, times: usize) {
    let weights = ProvenanceWeights::uniform(DEFAULT_EVIDENCE_WEIGHT);
    for _ in 0..times {
        let rel = edge(db, rel_type).await;
        let mut evidence = rel.edge_evidence();
        evidence.corroborate(Provenance::External, &weights);
        crud::reinforce_relationship(db, &rel.id_string(), evidence)
            .await
            .expect("failed to reinforce");
    }
}

// ── New edges are born migrated ──────────────────────────────────────

#[tokio::test]
async fn new_edges_are_created_with_evidence() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("failed to create graph dir");

    let (db, _) = open_store(&graph_path).await;
    create_entity(&db, ALICE, "Alice").await;
    create_entity(&db, BOB, "Bob").await;

    let requested: f32 = 0.6;
    crud::add_relationship(
        &db,
        NewRelationship {
            from_entity: "Alice".to_string(),
            to_entity: "Bob".to_string(),
            rel_type: "INFERRED".to_string(),
            description: None,
            confidence: Some(requested),
            source: Some("fixture".to_string()),
        },
    )
    .await
    .expect("failed to create relationship");

    let created = edge(&db, "INFERRED").await;
    assert_eq!(
        created.confidence, requested as f64,
        "creation stores the requested mean unchanged"
    );
    assert_eq!(
        created.evidence(),
        Evidence::from_prior(requested as f64),
        "a new edge sits at the prior"
    );
    assert_eq!(created.self_reinforcements, Some(0));
    assert!((created.evidence().alpha() - 6.0).abs() < 1e-6);
    assert!((created.evidence().beta() - 4.0).abs() < 1e-6);

    close(db).await;
}
