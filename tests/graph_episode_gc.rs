//! Episode garbage collection (Phase 1, increment 3 — AC4).
//!
//! An episode is collectable only when all four hold: older than the
//! configured age, never retrieved, self-authored, and cited as `source` by no
//! surviving entity or relationship. The dry run must name exactly those
//! episodes; the real run must remove exactly those episodes.
//!
//! Fixtures are raw SurrealQL, so no embedding model is involved — episode
//! collection is arithmetic on timestamps and a set membership test.

use chrono::{Duration, Utc};
use recall_echo::graph::crud;
use recall_echo::graph::gc::{self, GcActionKind, GcConfig};
use recall_echo::graph::store::{self, Db};
use surrealdb::Surreal;
use tempfile::TempDir;

/// A store with the current schema, on a directory that lives as long as the
/// returned `TempDir`.
async fn open_store() -> (TempDir, Surreal<Db>) {
    let dir = TempDir::new().expect("temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("graph dir");

    let db = store::open(&graph_path).await.expect("open store");
    store::init_schema(&db).await.expect("init schema");

    (dir, db)
}

/// Create an episode with an explicit age and authorship class.
async fn create_episode(db: &Surreal<Db>, id: &str, session_id: &str, age_days: i64, class: &str) {
    let timestamp = (Utc::now() - Duration::days(age_days)).to_rfc3339();
    db.query(
        r#"CREATE type::record($id) SET
               session_id = $session_id,
               timestamp = type::datetime($timestamp),
               abstract = $session_id,
               overview = NONE,
               content = 'chunk text',
               log_number = NONE,
               extracted = false,
               provenance = $provenance"#,
    )
    .bind(("id", id.to_string()))
    .bind(("session_id", session_id.to_string()))
    .bind(("timestamp", timestamp))
    .bind(("provenance", class.to_string()))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create episode {id}: {e}"));
}

/// An episode with no `provenance` field at all — written before the class
/// existed.
async fn create_legacy_episode(db: &Surreal<Db>, id: &str, session_id: &str, age_days: i64) {
    let timestamp = (Utc::now() - Duration::days(age_days)).to_rfc3339();
    db.query(
        r#"CREATE type::record($id) SET
               session_id = $session_id,
               timestamp = type::datetime($timestamp),
               abstract = $session_id,
               content = 'chunk text',
               extracted = false"#,
    )
    .bind(("id", id.to_string()))
    .bind(("session_id", session_id.to_string()))
    .bind(("timestamp", timestamp))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create legacy episode {id}: {e}"));
}

async fn create_entity(db: &Surreal<Db>, id: &str, name: &str, source: &str) {
    db.query(
        r#"CREATE type::record($id) SET
               name = $name,
               entity_type = 'concept',
               abstract = $name,
               overview = '',
               mutable = true,
               access_count = 1,
               created_at = time::now(),
               updated_at = time::now(),
               source = $source"#,
    )
    .bind(("id", id.to_string()))
    .bind(("name", name.to_string()))
    .bind(("source", source.to_string()))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create entity {id}: {e}"));
}

/// Episode record IDs still in the store, sorted.
async fn surviving_episode_ids(db: &Surreal<Db>) -> Vec<String> {
    let mut response = db
        .query("SELECT type::string(id) AS id FROM episode")
        .await
        .expect("list episodes");
    let rows: Vec<serde_json::Value> = response.take(0).expect("read episode ids");

    let mut ids: Vec<String> = rows
        .iter()
        .filter_map(|row| row["id"].as_str().map(String::from))
        .collect();
    ids.sort();
    ids
}

/// The episodes a report names as collectable, sorted.
fn named_episodes(report: &gc::GcReport) -> Vec<String> {
    let mut ids: Vec<String> = report
        .actions
        .iter()
        .filter(|a| a.kind == GcActionKind::SpentEpisode)
        .map(|a| a.target_id.clone())
        .collect();
    ids.sort();
    ids
}

fn episode_gc(dry_run: bool) -> GcConfig {
    GcConfig {
        collect_episodes: true,
        dry_run,
        ..Default::default()
    }
}

/// One old self-authored episode per protection class, plus two genuinely
/// spent ones. Every survivor survives for a different reason.
async fn seed_mixed_episodes(db: &Surreal<Db>) {
    create_episode(db, "episode:spent_a", "session-spent-a", 200, "self").await;
    create_episode(db, "episode:spent_b", "session-spent-b", 900, "self").await;
    create_legacy_episode(db, "episode:legacy", "session-legacy", 400).await;

    create_episode(db, "episode:young", "session-young", 10, "self").await;
    create_episode(db, "episode:user", "session-user", 900, "user").await;
    create_episode(db, "episode:external", "session-external", 900, "external").await;
    create_episode(db, "episode:evidence", "session-evidence", 900, "self").await;
    create_episode(db, "episode:read", "session-read", 900, "self").await;

    // The evidence episode's session produced an entity that still exists.
    create_entity(db, "entity:cited", "Cited", "session-evidence").await;
    // The read episode has been returned by retrieval at least once.
    crud::increment_episode_access_counts(db, &["episode:read".to_string()])
        .await
        .expect("increment episode access count");
}

#[tokio::test]
async fn dry_run_names_exactly_the_spent_episodes_and_deletes_nothing() {
    let (_dir, db) = open_store().await;
    seed_mixed_episodes(&db).await;

    let before = surviving_episode_ids(&db).await;
    let report = gc::run_gc(&db, &episode_gc(true)).await.expect("gc");

    assert_eq!(
        named_episodes(&report),
        vec![
            "episode:legacy".to_string(),
            "episode:spent_a".to_string(),
            "episode:spent_b".to_string(),
        ],
        "dry run must name exactly the old, unread, self-authored, uncited episodes"
    );
    assert_eq!(report.spent_episodes, 3);
    assert_eq!(report.episodes_scanned, 8);
    assert!(report.dry_run);
    assert_eq!(
        surviving_episode_ids(&db).await,
        before,
        "a dry run deletes nothing"
    );
}

#[tokio::test]
async fn execute_removes_only_the_named_episodes() {
    let (_dir, db) = open_store().await;
    seed_mixed_episodes(&db).await;

    let report = gc::run_gc(&db, &episode_gc(false)).await.expect("gc");

    assert_eq!(report.spent_episodes, 3);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        surviving_episode_ids(&db).await,
        vec![
            "episode:evidence".to_string(),
            "episode:external".to_string(),
            "episode:read".to_string(),
            "episode:user".to_string(),
            "episode:young".to_string(),
        ],
        "every protected episode survives, every spent one is gone"
    );
}

#[tokio::test]
async fn episodes_are_untouched_unless_collection_is_requested() {
    let (_dir, db) = open_store().await;
    seed_mixed_episodes(&db).await;

    let before = surviving_episode_ids(&db).await;
    let report = gc::run_gc(&db, &GcConfig::default()).await.expect("gc");

    assert_eq!(report.episodes_scanned, 0, "no episode scan without opt-in");
    assert_eq!(report.spent_episodes, 0);
    assert!(named_episodes(&report).is_empty());
    assert_eq!(surviving_episode_ids(&db).await, before);
}

#[tokio::test]
async fn an_episode_whose_only_citation_is_itself_swept_becomes_collectable() {
    // The citing edge is a stale, decayed relationship this same sweep
    // removes: "evidence for a surviving edge" has to mean surviving.
    let (_dir, db) = open_store().await;

    create_episode(&db, "episode:orphaned", "session-doomed", 400, "self").await;
    create_entity(&db, "entity:from", "From", "keep-me").await;
    create_entity(&db, "entity:to", "To", "keep-me").await;

    let valid_from = (Utc::now() - Duration::days(400)).to_rfc3339();
    db.query(
        r#"
        LET $from = type::record('entity:from');
        LET $to = type::record('entity:to');
        RELATE $from -> relates_to -> $to SET
            rel_type = 'DOOMED',
            description = 'cites the session, but not for much longer',
            valid_from = type::datetime($valid_from),
            valid_until = NONE,
            confidence = 0.05,
            alpha = 0.5,
            beta = 9.5,
            self_reinforcements = 0,
            source = 'session-doomed'
        "#,
    )
    .bind(("valid_from", valid_from))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .expect("create doomed edge");

    let report = gc::run_gc(&db, &episode_gc(false)).await.expect("gc");

    assert_eq!(
        report.stale_relationships + report.dead_relationships,
        1,
        "the citing edge is swept: {:?}",
        report.actions
    );
    assert_eq!(
        named_episodes(&report),
        vec!["episode:orphaned".to_string()],
        "with its only citation removed, the episode is spent too"
    );
    assert!(surviving_episode_ids(&db).await.is_empty());
}
