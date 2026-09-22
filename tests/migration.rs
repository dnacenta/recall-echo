// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Schema-migration tests for persisted edge evidence (Phase 1, increment 1).
//!
//! Three properties are under assertion:
//!
//! - **AC3** — opening a pre-Phase-1 store backfills `alpha`/`beta` from the
//!   stored mean without changing a single mean, and a second open migrates
//!   nothing.
//! - **RE-44 AC2–AC6, AC11–AC14** — opening a store whose episodes predate
//!   the `extracted` field gives every one of them `false` and nothing else,
//!   a version-0 store runs both backfills in one pass, an interrupted
//!   episode backfill resumes without double counting, the backfill spans
//!   more than one batch, a store from a newer build is refused, a row that
//!   cannot be coerced fails the open with the marker unwritten, and
//!   `--force` re-runs a migration the marker claims is done.
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

/// Take the episode table back to the shape it had before the `extracted`
/// field existed, so rows created afterwards carry no value for it —
/// SCHEMAFULL drops what is not defined.
async fn drop_extracted_field(db: &Surreal<Db>) {
    db.query(
        r#"
        REMOVE INDEX IF EXISTS episode_extracted ON episode;
        REMOVE FIELD IF EXISTS extracted ON episode;
        "#,
    )
    .await
    .and_then(surrealdb::IndexedResults::check)
    .expect("failed to downgrade the episode table");
}

async fn create_episode(db: &Surreal<Db>, session: &str, log_number: i64) {
    db.query("CREATE episode SET session_id = $s, abstract = $a, log_number = $ln")
        .bind(("s", session.to_string()))
        .bind(("a", format!("episode {log_number}")))
        .bind(("ln", log_number))
        .await
        .and_then(surrealdb::IndexedResults::check)
        .unwrap_or_else(|e| panic!("failed to create episode {log_number}: {e}"));
}

/// Every field a backfill has no business touching, keyed by record id, so a
/// pass can be compared against itself across the migration (AC2).
async fn episode_fields(db: &Surreal<Db>) -> Vec<(String, String, String, String)> {
    let mut response = db
        .query("SELECT id, session_id, abstract, timestamp, log_number FROM episode ORDER BY log_number")
        .await
        .expect("failed to read episodes");
    let rows: Vec<serde_json::Value> = response.take(0).expect("failed to read episode rows");
    rows.into_iter()
        .map(|row| {
            let text = |key: &str| row[key].to_string();
            (
                text("id"),
                text("session_id"),
                text("abstract"),
                text("timestamp"),
            )
        })
        .collect()
}

/// Every episode as `(log_number, extracted)`, with `None` for an episode
/// that carries no value at all.
async fn episodes_by_log(db: &Surreal<Db>) -> Vec<(i64, Option<bool>)> {
    let mut response = db
        .query("SELECT log_number, extracted FROM episode ORDER BY log_number")
        .await
        .expect("failed to read episodes");
    let rows: Vec<serde_json::Value> = response.take(0).expect("failed to read episode rows");
    rows.into_iter()
        .map(|row| {
            let log_number = row["log_number"]
                .as_i64()
                .expect("episode has a log_number");
            (log_number, row["extracted"].as_bool())
        })
        .collect()
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
    assert_eq!(second.episodes_backfilled, 0);

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
    assert_eq!(
        first.episodes_backfilled, 0,
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

// ── RE-44 AC2-AC6: the episode `extracted` backfill ──────────────────

/// A closed store holding three episodes written before `extracted` existed
/// and one written after, still at schema version 1.
async fn legacy_episode_store() -> (TempDir, std::path::PathBuf) {
    let (dir, graph_path) = new_graph_dir();

    let (db, _) = open_store(&graph_path).await;
    create_episode(&db, "modern", 1).await;
    drop_extracted_field(&db).await;
    for log_number in [2, 3, 4] {
        create_episode(&db, "legacy", log_number).await;
    }
    write_version(&db, 1).await;
    close(db).await;

    (dir, graph_path)
}

async fn read_version(db: &Surreal<Db>) -> i64 {
    let mut response = db
        .query("SELECT schema_version FROM type::record($id)")
        .bind(("id", META_RECORD.to_string()))
        .await
        .expect("failed to read version marker");
    let rows: Vec<serde_json::Value> = response.take(0).expect("version rows");
    rows.first()
        .and_then(|r| r["schema_version"].as_i64())
        .unwrap_or(0)
}

async fn write_version(db: &Surreal<Db>, version: i64) {
    db.query("UPSERT type::record($id) SET schema_version = $v")
        .bind(("id", META_RECORD.to_string()))
        .bind(("v", version))
        .await
        .and_then(surrealdb::IndexedResults::check)
        .expect("failed to write version marker");
}

#[tokio::test]
async fn legacy_episodes_gain_an_extracted_value() {
    let (_dir, graph_path) = legacy_episode_store().await;

    // Snapshot every field the backfill must not touch, through a read that
    // does not run a migration of its own.
    let db = store::open(&graph_path)
        .await
        .expect("failed to open store");
    let before = episode_fields(&db).await;
    close(db).await;

    let (db, report) = open_store(&graph_path).await;

    assert!(report.ran(), "migration should have run: {report:?}");
    assert_eq!(report.from_version, 1);
    assert_eq!(report.to_version, SCHEMA_VERSION);
    assert_eq!(
        report.episodes_backfilled, 3,
        "only the three legacy episodes: {report:?}"
    );
    assert_eq!(
        report.edges_backfilled, 0,
        "a version-1 store has no evidence-less edges left: {report:?}"
    );

    assert_eq!(
        episodes_by_log(&db).await,
        vec![
            (1, Some(false)),
            (2, Some(false)),
            (3, Some(false)),
            (4, Some(false)),
        ],
        "every episode now carries a concrete value"
    );
    assert_eq!(
        episode_fields(&db).await,
        before,
        "the backfill must change nothing but `extracted`"
    );

    // AC3: the second open migrates nothing and touches nothing.
    close(db).await;
    let (db, second) = open_store(&graph_path).await;
    assert!(!second.ran(), "second open must be a no-op: {second:?}");
    assert_eq!(second.episodes_backfilled, 0);
    assert_eq!(episodes_by_log(&db).await.len(), 4);
    close(db).await;
}

#[tokio::test]
async fn an_extracted_episode_is_left_alone() {
    let (_dir, graph_path) = legacy_episode_store().await;

    let (db, _) = open_store(&graph_path).await;
    crud::mark_episodes_extracted(&db, 2)
        .await
        .expect("failed to mark episode extracted");
    close(db).await;

    // Re-open with the marker stripped: the backfill runs again and must see
    // nothing, because `WHERE extracted IS NONE` excludes both true and false.
    let (db, _) = open_store(&graph_path).await;
    strip_version_marker(&db).await;
    close(db).await;

    let (db, report) = open_store(&graph_path).await;
    assert_eq!(
        report.episodes_backfilled, 0,
        "a value already present is never overwritten: {report:?}"
    );
    assert_eq!(
        episodes_by_log(&db).await,
        vec![
            (1, Some(false)),
            (2, Some(true)),
            (3, Some(false)),
            (4, Some(false)),
        ]
    );
    close(db).await;
}

#[tokio::test]
async fn version_zero_store_runs_both_backfills_in_one_pass() {
    let (_dir, graph_path) = new_graph_dir();

    let (db, _) = open_store(&graph_path).await;
    seed_legacy_edges(&db).await;
    drop_extracted_field(&db).await;
    for log_number in [5, 6] {
        create_episode(&db, "legacy", log_number).await;
    }
    strip_version_marker(&db).await;
    close(db).await;

    let (db, report) = open_store(&graph_path).await;

    assert_eq!(report.from_version, 0);
    assert_eq!(report.to_version, SCHEMA_VERSION);
    assert_eq!(report.edges_backfilled, 3, "{report:?}");
    assert_eq!(report.episodes_backfilled, 2, "{report:?}");
    assert_eq!(
        episodes_by_log(&db).await,
        vec![(5, Some(false)), (6, Some(false))]
    );

    close(db).await;
}

#[tokio::test]
async fn interrupted_episode_backfill_completes_without_double_counting() {
    let (_dir, graph_path) = legacy_episode_store().await;

    // A process killed mid-backfill: one legacy episode already has a value,
    // the other two do not, and the version marker still says 1.
    let (db, _) = open_store(&graph_path).await;
    strip_version_marker(&db).await;
    drop_extracted_field(&db).await;
    for log_number in [7, 8] {
        create_episode(&db, "legacy", log_number).await;
    }
    close(db).await;

    let (db, report) = open_store(&graph_path).await;

    assert!(report.ran(), "migration must resume: {report:?}");
    assert_eq!(
        report.episodes_backfilled, 2,
        "only the rows the first pass never reached: {report:?}"
    );
    assert!(
        episodes_by_log(&db)
            .await
            .iter()
            .all(|(_, extracted)| *extracted == Some(false)),
        "every episode ends up false exactly once"
    );

    close(db).await;
}

// ── RE-44 AC11-AC14: batching, refusal, repair ───────────────────────

/// More episodes than one backfill batch, so the loop has to run twice.
/// `BACKFILL_BATCH` is 1000; 1200 rows means two statements, two
/// transactions, and a count that spans both.
#[tokio::test]
async fn a_backfill_larger_than_one_batch_completes_and_counts_every_row() {
    const ROWS: usize = 1_200;

    let (_dir, graph_path) = new_graph_dir();
    let (db, _) = open_store(&graph_path).await;
    drop_extracted_field(&db).await;
    let rows: Vec<String> = (0..ROWS)
        .map(|i| format!("{{ session_id: 'bulk', abstract: 'e{i}', log_number: {i} }}"))
        .collect();
    for chunk in rows.chunks(400) {
        db.query(format!("INSERT INTO episode [{}]", chunk.join(",")))
            .await
            .and_then(surrealdb::IndexedResults::check)
            .expect("bulk insert");
    }
    strip_version_marker(&db).await;
    close(db).await;

    let (db, report) = open_store(&graph_path).await;

    assert_eq!(
        report.episodes_backfilled, ROWS as u64,
        "every row across every batch is counted once: {report:?}"
    );
    assert!(
        episodes_by_log(&db)
            .await
            .iter()
            .all(|(_, extracted)| *extracted == Some(false)),
        "no row is left behind by the batch boundary"
    );

    // The post-condition the loop exits on, asserted directly.
    let mut response = db
        .query("SELECT count() AS n FROM episode WHERE extracted IS NONE GROUP ALL")
        .await
        .expect("absent count");
    let counts: Vec<serde_json::Value> = response.take(0).expect("absent count rows");
    assert!(
        counts.is_empty() || counts[0]["n"].as_u64() == Some(0),
        "no episode may be left without a value: {counts:?}"
    );

    close(db).await;
}

/// A store written by a build that knows more migrations than this one must
/// be refused, not reshaped — the marker is what decides which migrations
/// have run.
#[tokio::test]
async fn a_store_from_a_newer_build_is_refused_by_name() {
    let (_dir, graph_path) = new_graph_dir();
    let (db, _) = open_store(&graph_path).await;
    write_version(&db, SCHEMA_VERSION + 7).await;
    close(db).await;

    let db = store::open(&graph_path)
        .await
        .expect("failed to open store");
    let err = store::init_schema(&db)
        .await
        .expect_err("a newer store must not open");
    let message = err.to_string();

    assert!(
        message.contains(&(SCHEMA_VERSION + 7).to_string()),
        "the error names the store's version: {message}"
    );
    assert!(
        message.contains(&SCHEMA_VERSION.to_string()),
        "the error names this build's version: {message}"
    );
    close(db).await;
}

/// An episode that cannot survive SCHEMAFULL coercion fails the backfill, and
/// a failed backfill must leave the marker alone so the next open tries
/// again. The alternative — writing the marker anyway — would strand the row
/// forever behind a scan that cannot see it.
#[tokio::test]
async fn an_uncoercible_row_fails_the_open_and_leaves_the_marker_alone() {
    let (_dir, graph_path) = new_graph_dir();

    // The episode table as it was before `abstract` was mandatory, holding a
    // row with no abstract at all.
    let db = store::open(&graph_path)
        .await
        .expect("failed to open store");
    db.query(
        r#"
        DEFINE TABLE episode SCHEMAFULL;
        DEFINE FIELD session_id ON episode TYPE string;
        DEFINE FIELD timestamp  ON episode TYPE datetime DEFAULT time::now();
        DEFINE FIELD log_number ON episode TYPE option<int>;
        "#,
    )
    .await
    .and_then(surrealdb::IndexedResults::check)
    .expect("pre-abstract schema");
    db.query("CREATE episode SET session_id = 'ancient', log_number = 1")
        .await
        .and_then(surrealdb::IndexedResults::check)
        .expect("ancient insert");
    close(db).await;

    for attempt in 1..=2 {
        let db = store::open(&graph_path)
            .await
            .expect("failed to open store");
        let err = match store::init_schema(&db).await {
            Ok(report) => panic!("attempt {attempt}: expected failure, got {report:?}"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("abstract"),
            "attempt {attempt}: the error names the field: {err}"
        );
        assert_eq!(
            read_version(&db).await,
            0,
            "attempt {attempt}: a failed migration must not claim to have run"
        );
        close(db).await;
    }
}

/// The state opening cannot repair: the marker says the migration ran, and
/// episodes still have no value. `--force` is the way out.
#[tokio::test]
async fn force_reruns_a_migration_the_marker_claims_is_done() {
    let (_dir, graph_path) = new_graph_dir();

    let (db, _) = open_store(&graph_path).await;
    drop_extracted_field(&db).await;
    for log_number in [1, 2, 3] {
        create_episode(&db, "stranded", log_number).await;
    }
    // The marker already claims version 2 — exactly the shape a backfill that
    // reported success without finishing would leave.
    write_version(&db, SCHEMA_VERSION).await;
    close(db).await;

    // A plain open migrates nothing: there is nothing it believes to do.
    let (db, report) = open_store(&graph_path).await;
    assert!(
        !report.ran(),
        "an open cannot see past the marker: {report:?}"
    );
    assert!(
        episodes_by_log(&db)
            .await
            .iter()
            .all(|(_, extracted)| extracted.is_none()),
        "the rows are still stranded"
    );

    close(db).await;

    // The embedded store takes one process at a time, so the repair runs on
    // its own handle.
    let graph = recall_echo::graph::GraphMemory::open_embedded(&graph_path)
        .await
        .expect("open graph");
    let forced = graph
        .run_migrations(true)
        .await
        .expect("forced migration must run");
    assert_eq!(
        forced.from_version, 0,
        "--force forgets the marker: {forced:?}"
    );
    assert_eq!(forced.episodes_backfilled, 3, "{forced:?}");
    drop(graph);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let (db, after) = open_store(&graph_path).await;
    assert!(
        !after.ran(),
        "the repair left the marker current: {after:?}"
    );
    assert!(
        episodes_by_log(&db)
            .await
            .iter()
            .all(|(_, extracted)| *extracted == Some(false)),
        "every stranded row now has a value"
    );
    close(db).await;
}
