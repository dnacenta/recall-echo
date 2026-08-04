//! The outcome-feedback tier, wired end to end (Phase 1, increment 3 — AC5).
//!
//! Two properties:
//!
//! - **Passive was-used.** Recording what a session touched creates the
//!   `contributed_to` records a later outcome needs, without judging the
//!   session and without moving a utility score.
//! - **`graph feedback` moves utility.** The CLI verb's request, run through
//!   the daemon exactly as the CLI runs it, changes the stored utility scores
//!   of the entities the session touched.
//!
//! Fixtures are raw SurrealQL — feedback is graph writes and arithmetic, so no
//! embedding model is involved.

use std::path::{Path, PathBuf};
use std::sync::Once;

use recall_echo::graph::store::{self, Db};
use recall_echo::graph::utility::{self, FeedbackReport, OutcomeKind, DEFAULT_UTILITY};
use recall_echo::serve::{FeedbackArgs, Request};
use recall_echo::serve_client;
use surrealdb::Surreal;
use tempfile::TempDir;

const SESSION: &str = "conversation-042";

static BIN_ENV: Once = Once::new();

/// Point the daemon client at the binary cargo just built.
fn use_test_binary() {
    BIN_ENV.call_once(|| {
        std::env::set_var(
            serve_client::DAEMON_BIN_ENV,
            env!("CARGO_BIN_EXE_recall-echo"),
        );
    });
}

/// A memory directory with a graph store and its own daemon socket.
struct Fixture {
    _dir: TempDir,
    memory_dir: PathBuf,
    graph_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        use_test_binary();

        let dir = TempDir::new().expect("temp dir");
        let memory_dir = dir.path().join("e").join("memory");
        let graph_dir = memory_dir.join("graph");
        std::fs::create_dir_all(&graph_dir).expect("memory dir");

        std::fs::write(
            memory_dir.join(".recall-echo.toml"),
            format!(
                "[serve]\nsocket_path = \"{}\"\nidle_timeout_secs = 120\n",
                dir.path().join("g.sock").display()
            ),
        )
        .expect("write config");

        Self {
            _dir: dir,
            memory_dir,
            graph_dir,
        }
    }

    /// Open the store directly. Only valid while no daemon holds it.
    async fn open(&self) -> Surreal<Db> {
        let db = store::open(&self.graph_dir).await.expect("open store");
        store::init_schema(&db).await.expect("init schema");
        db
    }
}

/// Release the embedded store's process lock.
async fn close(db: Surreal<Db>) {
    drop(db);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

async fn create_entity(db: &Surreal<Db>, id: &str, name: &str, source: &str) {
    db.query(
        r#"CREATE type::record($id) SET
               name = $name,
               entity_type = 'concept',
               abstract = $name,
               overview = '',
               mutable = true,
               access_count = 0,
               utility_score = $utility,
               utility_updates = 0,
               created_at = time::now(),
               updated_at = time::now(),
               source = $source"#,
    )
    .bind(("id", id.to_string()))
    .bind(("name", name.to_string()))
    .bind(("utility", DEFAULT_UTILITY))
    .bind(("source", source.to_string()))
    .await
    .and_then(surrealdb::IndexedResults::check)
    .unwrap_or_else(|e| panic!("failed to create entity {id}: {e}"));
}

/// The `contributed_to` records of one session, as (entity, result, was_used).
async fn contribution_records(db: &Surreal<Db>, session_id: &str) -> Vec<(String, String, bool)> {
    let mut response = db
        .query(
            r#"SELECT type::string(in) AS entity, outcome_result, was_used
               FROM contributed_to WHERE session_id = $sid"#,
        )
        .bind(("sid", session_id.to_string()))
        .await
        .expect("list contribution records");

    let rows: Vec<serde_json::Value> = response.take(0).expect("read contribution records");
    let mut records: Vec<(String, String, bool)> = rows
        .iter()
        .map(|row| {
            (
                row["entity"].as_str().unwrap_or_default().to_string(),
                row["outcome_result"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                row["was_used"].as_bool().unwrap_or_default(),
            )
        })
        .collect();
    records.sort();
    records
}

async fn count_outcome_entities(db: &Surreal<Db>, session_id: &str) -> usize {
    let mut response = db
        .query(
            r#"SELECT id FROM entity
               WHERE entity_type = "outcome" AND attributes.session_id = $sid"#,
        )
        .bind(("sid", session_id.to_string()))
        .await
        .expect("count outcome entities");

    let rows: Vec<serde_json::Value> = response.take(0).expect("read outcome entities");
    rows.len()
}

async fn utility_of(db: &Surreal<Db>, entity_id: &str) -> f64 {
    utility::get_utility_score(db, entity_id)
        .await
        .expect("read utility score")
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// Two entities the session touched, linked passively.
async fn seed_session(db: &Surreal<Db>) {
    create_entity(db, "entity:alpha", "Alpha", SESSION).await;
    create_entity(db, "entity:beta", "Beta", SESSION).await;
    record_session_use(db).await;
}

/// Record the passive was-used link for both seeded entities.
async fn record_session_use(db: &Surreal<Db>) {
    utility::record_session_use(
        db,
        SESSION,
        &["entity:alpha".to_string(), "entity:beta".to_string()],
    )
    .await
    .expect("record session use");
}

// ── Passive was-used ─────────────────────────────────────────────────

#[tokio::test]
async fn session_use_records_edges_without_judging_the_session() {
    let fixture = Fixture::new();
    let db = fixture.open().await;

    seed_session(&db).await;

    assert_eq!(
        contribution_records(&db, SESSION).await,
        vec![
            ("entity:alpha".to_string(), "pending".to_string(), true),
            ("entity:beta".to_string(), "pending".to_string(), true),
        ],
        "every touched entity gets one unadjudicated was-used record"
    );
    assert!(
        approx(utility_of(&db, "entity:alpha").await, DEFAULT_UTILITY),
        "an unjudged session is not evidence of usefulness"
    );

    // The linkage is a set — assert membership, not the store's row order.
    let session = utility::session_entities(&db, SESSION)
        .await
        .expect("resolve session entities");
    let mut retrieved = session.retrieved.clone();
    retrieved.sort();
    let mut used = session.used.clone();
    used.sort();
    assert_eq!(retrieved, vec!["entity:alpha", "entity:beta"]);
    assert_eq!(
        used, retrieved,
        "a passively recorded entity counts as used"
    );

    close(db).await;
}

#[tokio::test]
async fn re_recording_a_session_does_not_duplicate_its_records() {
    let fixture = Fixture::new();
    let db = fixture.open().await;

    seed_session(&db).await;
    record_session_use(&db).await;

    assert_eq!(
        contribution_records(&db, SESSION).await.len(),
        2,
        "one record per entity per session, however often ingest re-runs"
    );
    assert_eq!(count_outcome_entities(&db, SESSION).await, 1);

    close(db).await;
}

#[tokio::test]
async fn sessions_without_records_fall_back_to_authorship() {
    let fixture = Fixture::new();
    let db = fixture.open().await;

    // Ingested before passive recording existed: the entities carry the
    // session as their source, and nothing else links them to it.
    create_entity(&db, "entity:legacy", "Legacy", SESSION).await;

    let session = utility::session_entities(&db, SESSION)
        .await
        .expect("resolve session entities");
    assert_eq!(session.retrieved, vec!["entity:legacy"]);
    assert_eq!(session.used, vec!["entity:legacy"]);

    close(db).await;
}

// ── graph feedback ───────────────────────────────────────────────────

#[tokio::test]
async fn feedback_through_the_daemon_moves_utility_scores() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_session(&db).await;
    let before = utility_of(&db, "entity:alpha").await;
    close(db).await;

    // Exactly the request `graph feedback <session> --outcome success` sends.
    let report = feedback(&fixture.memory_dir, OutcomeKind::Success).await;

    assert_eq!(report.entities_updated, 2, "{:?}", report.errors);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.utilities.len(), 2);
    assert!(
        report.utilities.iter().all(|u| u.utility_score > before),
        "the report shows where every entity landed: {:?}",
        report.utilities
    );

    serve_client::stop_daemon(&fixture.memory_dir)
        .await
        .expect("stop daemon");

    let db = fixture.open().await;
    let after = utility_of(&db, "entity:alpha").await;
    assert!(
        after > before,
        "a successful session raises utility: {before} -> {after}"
    );
    assert_eq!(
        contribution_records(&db, SESSION).await,
        vec![
            ("entity:alpha".to_string(), "success".to_string(), true),
            ("entity:beta".to_string(), "success".to_string(), true),
        ],
        "the pending records are resolved, not duplicated"
    );
    assert_eq!(
        count_outcome_entities(&db, SESSION).await,
        1,
        "one outcome record per session"
    );
    close(db).await;
}

#[tokio::test]
async fn failure_feedback_lowers_utility() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_session(&db).await;
    let before = utility_of(&db, "entity:beta").await;
    close(db).await;

    let report = feedback(&fixture.memory_dir, OutcomeKind::Failed).await;
    assert_eq!(report.entities_updated, 2, "{:?}", report.errors);

    serve_client::stop_daemon(&fixture.memory_dir)
        .await
        .expect("stop daemon");

    let db = fixture.open().await;
    let after = utility_of(&db, "entity:beta").await;
    assert!(
        after < before,
        "a failed session lowers utility: {before} -> {after}"
    );
    close(db).await;
}

#[tokio::test]
async fn feedback_for_an_unknown_session_changes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_session(&db).await;
    close(db).await;

    let report = feedback_for(&fixture.memory_dir, "no-such-session", OutcomeKind::Success).await;
    assert_eq!(report.entities_updated, 0);
    assert!(report.utilities.is_empty());
    assert!(report.outcome_entity_id.is_empty());

    serve_client::stop_daemon(&fixture.memory_dir)
        .await
        .expect("stop daemon");

    let db = fixture.open().await;
    assert!(
        approx(utility_of(&db, "entity:alpha").await, DEFAULT_UTILITY),
        "an outcome for a session we know nothing about touches nothing"
    );
    close(db).await;
}

async fn feedback(memory_dir: &Path, outcome: OutcomeKind) -> FeedbackReport {
    feedback_for(memory_dir, SESSION, outcome).await
}

/// Run the feedback verb's request through the daemon and decode its report.
async fn feedback_for(memory_dir: &Path, session_id: &str, outcome: OutcomeKind) -> FeedbackReport {
    let request = Request::Feedback(FeedbackArgs {
        session_id: session_id.to_string(),
        outcome,
    });
    let data = serve_client::execute(memory_dir, &request)
        .await
        .expect("feedback request");
    serde_json::from_value(data).expect("decode feedback report")
}
