// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SurrealDB store — embedded (kv-surrealkv) or server (WebSocket), selected
//! at runtime via the `[graph] mode` config key (`embedded` | `server`).
//!
//! Both engines are compiled in by default and dispatched through
//! `surrealdb::engine::any`, so switching backends is a config change,
//! not a rebuild.
//!
//! Concurrency: the embedded SurrealKV backend takes a process-exclusive
//! file lock. Concurrent access from a second process fails with
//! [`GraphError::Locked`] after a bounded retry. Server mode (or the serve
//! daemon) is the supported way to share one store between processes.

use std::path::Path;
use std::time::Duration;

use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use super::confidence::PRIOR_CONCENTRATION;
use super::error::GraphError;

pub type Db = Any;

/// Schema version this build writes. Bumped by every migration.
///
/// - `0` — pre-Phase-1: edges carry a bare `confidence` mean.
/// - `1` — edges carry persisted Beta evidence (`alpha`, `beta`) and a
///   `self_reinforcements` coherence counter.
/// - `2` — every episode carries a concrete `extracted` value, so the
///   extraction scan can be served by the `episode_extracted` index.
pub const SCHEMA_VERSION: i64 = 2;

/// Record ID of the singleton row holding graph-wide metadata.
const META_RECORD: &str = "meta:schema";

/// How many times to retry opening an embedded store that is locked by
/// another process, and the base backoff between attempts (doubled each try).
///
/// The common case is a daemon that has just been asked to stop and is
/// releasing the store, which takes single-digit milliseconds: start far
/// below that and spend the same total budget (~3.8s) on more attempts.
const LOCK_RETRY_ATTEMPTS: u32 = 8;
const LOCK_RETRY_BASE: Duration = Duration::from_millis(15);

/// How many episodes one backfill statement rewrites. A migration is the one
/// place a whole table is written at once, and an unbounded write transaction
/// is what exhausts SurrealKV's memtable arena on a large store — so the
/// backfill is a loop of bounded, independently committed batches.
const BACKFILL_BATCH: usize = 1000;

/// How many times a migration pass is retried when the store answers with a
/// transaction conflict, and the base backoff between attempts (doubled each
/// try).
///
/// Embedded stores take a process-exclusive lock, so this only matters in
/// server mode, where two processes can open the same database at once. Both
/// backfills are idempotent and the version marker only moves forward, so the
/// loser of a race re-reads the version and finds the work already done.
const MIGRATION_RETRY_ATTEMPTS: u32 = 4;
const MIGRATION_RETRY_BASE: Duration = Duration::from_millis(40);

/// Connection config for server mode.
#[derive(Clone)]
pub struct ServerConfig {
    pub url: String,
    pub username: String,
    pub password: String,
    pub namespace: String,
    pub database: String,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("namespace", &self.namespace)
            .field("database", &self.database)
            .finish()
    }
}

/// Total time [`open`] spends waiting for another process to release the
/// embedded store's lock, doubling the base backoff each attempt.
fn lock_retry_budget() -> Duration {
    (1..=LOCK_RETRY_ATTEMPTS)
        .map(|n| LOCK_RETRY_BASE * 2u32.pow(n - 1))
        .sum()
}

/// True if a SurrealDB error indicates the embedded store's process-exclusive
/// file lock is held by another process.
fn is_lock_error(err: &surrealdb::Error) -> bool {
    is_lock_message(&err.to_string().to_lowercase())
}

fn is_lock_message(msg: &str) -> bool {
    msg.contains("lock") && (msg.contains("already") || msg.contains("held"))
}

/// True if a graph error is an optimistic-transaction conflict — two writers
/// touched the same keys and one has to go again. Retryable by definition;
/// anything else is not.
fn is_retryable_conflict(err: &GraphError) -> bool {
    match err {
        GraphError::Db(inner) => is_conflict_message(&inner.to_string().to_lowercase()),
        _ => false,
    }
}

fn is_conflict_message(msg: &str) -> bool {
    msg.contains("conflict") || msg.contains("please retry")
}

/// Open (or create) a SurrealDB embedded store at the given path.
///
/// Retries briefly if another process holds the store lock, then fails with
/// [`GraphError::Locked`] carrying an actionable message.
pub async fn open(path: &Path) -> Result<Surreal<Db>, GraphError> {
    let surreal_path = path.join("surreal");
    std::fs::create_dir_all(&surreal_path)?;

    let path_str = surreal_path.to_str().ok_or_else(|| {
        GraphError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "graph store path contains non-UTF8 characters",
        ))
    })?;

    let endpoint = format!("surrealkv://{path_str}");
    let mut attempt: u32 = 0;
    let db: Surreal<Db> = loop {
        match surrealdb::engine::any::connect(&endpoint).await {
            Ok(db) => break db,
            Err(e) if is_lock_error(&e) && attempt < LOCK_RETRY_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(LOCK_RETRY_BASE * 2u32.pow(attempt - 1)).await;
            }
            Err(e) if is_lock_error(&e) => {
                return Err(GraphError::Locked(format!(
                    "graph store at {} is locked by another process. The embedded \
                     backend allows one process at a time — retried {} times over \
                     ~{}s. Another recall-echo command (or the serve daemon) is \
                     using it — a first open after an upgrade may be running a \
                     schema migration, which on a large store takes longer than \
                     that budget. Wait for it to finish, or use server mode to \
                     share the store.",
                    surreal_path.display(),
                    LOCK_RETRY_ATTEMPTS,
                    lock_retry_budget().as_secs_f32().round()
                )));
            }
            Err(e) => return Err(e.into()),
        }
    };
    db.use_ns("recall").use_db("graph").await?;

    Ok(db)
}

/// Accept a server URL with or without a scheme.
///
/// Before the runtime-backend change, server mode always used the WebSocket
/// connector, so configs in the wild carry bare `host:port`. `engine::any`
/// dispatches on the scheme and rejects those with "Invalid URL", which turns
/// a working config into a hard failure on upgrade. A schemeless value keeps
/// meaning what it always meant.
fn normalize_server_url(url: &str) -> String {
    const SCHEMES: [&str; 6] = [
        "ws://",
        "wss://",
        "http://",
        "https://",
        "surrealkv://",
        "mem://",
    ];
    if SCHEMES.iter().any(|s| url.starts_with(s)) {
        url.to_string()
    } else {
        format!("ws://{url}")
    }
}

/// Connect to a SurrealDB server (e.g. `ws://localhost:8787`).
pub async fn connect(config: &ServerConfig) -> Result<Surreal<Db>, GraphError> {
    let db = surrealdb::engine::any::connect(normalize_server_url(&config.url)).await?;
    db.signin(surrealdb::opt::auth::Database {
        namespace: config.namespace.clone(),
        database: config.database.clone(),
        username: config.username.clone(),
        password: config.password.clone(),
    })
    .await?;
    db.use_ns(&config.namespace)
        .use_db(&config.database)
        .await?;

    Ok(db)
}

/// Initialize the graph schema, then bring the store up to
/// [`SCHEMA_VERSION`]. Idempotent — safe to call on every open.
///
/// The version marker is read *before* any other definition is applied, so a
/// store written by a newer build is refused rather than reshaped by an older
/// one: the marker decides which migrations have run, and a build that cannot
/// read it has no business writing to the store.
pub async fn init_schema(db: &Surreal<Db>) -> Result<MigrationReport, GraphError> {
    define_meta(db).await?;
    refuse_newer_store(read_schema_version(db).await?)?;
    define_schema(db).await?;
    migrate_with_retries(db).await
}

/// Declare the metadata table alone, so the schema version can be read before
/// anything else is touched.
async fn define_meta(db: &Surreal<Db>) -> Result<(), GraphError> {
    db.query(
        r#"
        DEFINE TABLE IF NOT EXISTS meta SCHEMAFULL;
        DEFINE FIELD IF NOT EXISTS schema_version ON meta TYPE int DEFAULT 0;
        "#,
    )
    .await?
    .check()?;

    Ok(())
}

/// Refuse a store written by a build that knows more migrations than this one.
///
/// Opening it would run this build's read and write paths against a shape it
/// has never seen — and, worse, leave the marker claiming the newer version
/// while older code edits the data underneath it.
fn refuse_newer_store(from_version: i64) -> Result<(), GraphError> {
    if from_version > SCHEMA_VERSION {
        return Err(GraphError::Migration(format!(
            "this store is at schema version {from_version}, and this build of \
             recall-echo only knows version {SCHEMA_VERSION}. It was written by a \
             newer release; upgrade recall-echo (`recall-echo update`) rather than \
             opening it with this one."
        )));
    }
    Ok(())
}

/// Declare tables, fields and indexes. Every statement is `IF NOT EXISTS`.
async fn define_schema(db: &Surreal<Db>) -> Result<(), GraphError> {
    db.query(
        r#"
        DEFINE TABLE IF NOT EXISTS entity SCHEMAFULL;
        DEFINE FIELD IF NOT EXISTS name         ON entity TYPE string;
        DEFINE FIELD IF NOT EXISTS entity_type  ON entity TYPE string;
        DEFINE FIELD IF NOT EXISTS abstract     ON entity TYPE string;
        DEFINE FIELD IF NOT EXISTS overview     ON entity TYPE string;
        DEFINE FIELD IF NOT EXISTS content      ON entity TYPE option<string>;
        DEFINE FIELD IF NOT EXISTS attributes ON entity TYPE option<object> FLEXIBLE;
        DEFINE FIELD IF NOT EXISTS embedding    ON entity TYPE option<array<float>>;
        DEFINE FIELD IF NOT EXISTS mutable      ON entity TYPE bool DEFAULT true;
        DEFINE FIELD IF NOT EXISTS access_count ON entity TYPE int DEFAULT 0;
        DEFINE FIELD IF NOT EXISTS utility_score    ON entity TYPE float DEFAULT 0.5;
        DEFINE FIELD IF NOT EXISTS utility_updates  ON entity TYPE int DEFAULT 0;
        DEFINE FIELD IF NOT EXISTS created_at   ON entity TYPE datetime DEFAULT time::now();
        DEFINE FIELD IF NOT EXISTS updated_at   ON entity TYPE datetime DEFAULT time::now();
        DEFINE FIELD IF NOT EXISTS source       ON entity TYPE option<string>;

        DEFINE INDEX IF NOT EXISTS entity_name   ON entity FIELDS name;
        DEFINE INDEX IF NOT EXISTS entity_type   ON entity FIELDS entity_type;
        DEFINE INDEX IF NOT EXISTS entity_vector ON entity FIELDS embedding HNSW DIMENSION 384 DIST COSINE;

        -- Pipeline attribute indexes
        DEFINE INDEX IF NOT EXISTS entity_pipeline_stage  ON entity FIELDS attributes.pipeline_stage;
        DEFINE INDEX IF NOT EXISTS entity_pipeline_status ON entity FIELDS attributes.pipeline_status;

        DEFINE TABLE IF NOT EXISTS relates_to SCHEMAFULL TYPE RELATION;
        DEFINE FIELD IF NOT EXISTS rel_type    ON relates_to TYPE string;
        DEFINE FIELD IF NOT EXISTS description ON relates_to TYPE option<string>;
        DEFINE FIELD IF NOT EXISTS valid_from  ON relates_to TYPE datetime DEFAULT time::now();
        DEFINE FIELD IF NOT EXISTS valid_until ON relates_to TYPE option<datetime>;
        DEFINE FIELD IF NOT EXISTS confidence  ON relates_to TYPE float DEFAULT 1.0;
        -- Persisted Beta evidence. `option` because edges written before
        -- schema version 1 have none until the backfill reaches them.
        DEFINE FIELD IF NOT EXISTS alpha       ON relates_to TYPE option<float>;
        DEFINE FIELD IF NOT EXISTS beta        ON relates_to TYPE option<float>;
        DEFINE FIELD IF NOT EXISTS self_reinforcements ON relates_to TYPE option<int>;
        DEFINE FIELD IF NOT EXISTS last_reinforced ON relates_to TYPE option<datetime>;
        DEFINE FIELD IF NOT EXISTS source      ON relates_to TYPE option<string>;

        DEFINE INDEX IF NOT EXISTS rel_type_idx ON relates_to FIELDS rel_type;

        DEFINE TABLE IF NOT EXISTS episode SCHEMAFULL;
        DEFINE FIELD IF NOT EXISTS session_id  ON episode TYPE string;
        DEFINE FIELD IF NOT EXISTS timestamp   ON episode TYPE datetime DEFAULT time::now();
        DEFINE FIELD IF NOT EXISTS abstract    ON episode TYPE string;
        DEFINE FIELD IF NOT EXISTS overview    ON episode TYPE option<string>;
        DEFINE FIELD IF NOT EXISTS content     ON episode TYPE option<string>;
        DEFINE FIELD IF NOT EXISTS embedding   ON episode TYPE option<array<float>>;
        DEFINE FIELD IF NOT EXISTS log_number  ON episode TYPE option<int>;
        DEFINE FIELD IF NOT EXISTS extracted  ON episode TYPE bool DEFAULT false;
        -- How many times retrieval has returned this episode. Absent on
        -- episodes written before the field existed; read paths resolve that
        -- to zero, which is also what it means. No backfill, no version bump.
        DEFINE FIELD IF NOT EXISTS access_count ON episode TYPE option<int>;
        -- Authorship class: 'external' | 'user' | 'self'. `option` with no
        -- default on purpose — an absent value means an episode written
        -- before provenance existed, and reads resolve that to 'self'. No
        -- backfill, so no schema version bump: the absent case is already
        -- the conservative one.
        DEFINE FIELD IF NOT EXISTS provenance ON episode TYPE option<string>;

        DEFINE INDEX IF NOT EXISTS episode_session ON episode FIELDS session_id;
        DEFINE INDEX IF NOT EXISTS episode_time    ON episode FIELDS timestamp;
        -- Every archive-keyed episode read and write matches on log_number:
        -- marking a log extracted, and fetching one episode by log. Without
        -- this they are full table scans, paid once per archive — which on a
        -- backlog drain is the dominant cost of extraction, not the poll.
        -- `log_number` is `option<int>` and concrete or NONE on every row,
        -- so no backfill and no version bump.
        DEFINE INDEX IF NOT EXISTS episode_log     ON episode FIELDS log_number;
        DEFINE INDEX IF NOT EXISTS episode_vector  ON episode FIELDS embedding HNSW DIMENSION 384 DIST COSINE;

        DEFINE TABLE IF NOT EXISTS contributed_to SCHEMAFULL TYPE RELATION;
        DEFINE FIELD IF NOT EXISTS outcome_result ON contributed_to TYPE string;
        DEFINE FIELD IF NOT EXISTS was_used       ON contributed_to TYPE bool DEFAULT true;
        DEFINE FIELD IF NOT EXISTS session_id     ON contributed_to TYPE string;
        DEFINE FIELD IF NOT EXISTS timestamp      ON contributed_to TYPE datetime DEFAULT time::now();

        DEFINE INDEX IF NOT EXISTS ct_session ON contributed_to FIELDS session_id;
        "#,
    )
    .await?
    .check()?;

    Ok(())
}

/// What one migration pass did. Both counts are zero on an already current
/// store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrationReport {
    /// Schema version the store was at when the pass started.
    pub from_version: i64,
    /// Schema version the store is at now.
    pub to_version: i64,
    /// Number of edges that gained evidence counts in this pass.
    pub edges_backfilled: u64,
    /// Number of episodes that gained an `extracted` value in this pass.
    pub episodes_backfilled: u64,
}

impl MigrationReport {
    /// True when this pass actually moved the store forward.
    #[must_use]
    pub fn ran(&self) -> bool {
        self.from_version < self.to_version
    }

    /// One line naming the version step and what each backfill touched, for
    /// the notice every entry point prints after a pass that [`ran`].
    ///
    /// [`ran`]: MigrationReport::ran
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "graph schema migrated v{} → v{} ({} edges, {} episodes backfilled)",
            self.from_version, self.to_version, self.edges_backfilled, self.episodes_backfilled
        )
    }
}

/// Bring the store up to [`SCHEMA_VERSION`], retrying a bounded number of
/// times if another writer conflicts with this one.
///
/// Embedded stores are protected by a process-exclusive file lock, so a race
/// is only reachable in server mode. Nothing here needs a lock of its own:
/// every backfill only touches rows that still lack the value it writes, the
/// version marker only ever moves forward, and the pass re-reads the version
/// on each attempt — so the loser of a race finds the work already done
/// instead of redoing or undoing it. What it must not do is turn a transient
/// conflict into a failed open.
async fn migrate_with_retries(db: &Surreal<Db>) -> Result<MigrationReport, GraphError> {
    let mut attempt: u32 = 0;
    loop {
        match migrate(db).await {
            Ok(report) => return Ok(report),
            Err(err) if is_retryable_conflict(&err) && attempt < MIGRATION_RETRY_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(MIGRATION_RETRY_BASE * 2u32.pow(attempt - 1)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// One migration pass.
///
/// Crash-only: every backfill runs *before* the version marker is written and
/// only touches rows that still lack the value it writes (`alpha IS NONE`,
/// `extracted IS NONE`). An interrupted pass therefore leaves a store that
/// re-opens, finishes the remainder, and never counts a row twice.
///
/// Each backfill is gated on the version that introduced it rather than on
/// `from_version` being exactly the previous one, so a version-0 store runs
/// both passes in a single open.
///
/// An error here is not swallowed: with the version-2 backfill unfinished,
/// an episode whose `extracted` is still absent is invisible to the
/// extraction scan, so the store must refuse to open rather than silently
/// drop pending archives.
async fn migrate(db: &Surreal<Db>) -> Result<MigrationReport, GraphError> {
    let from_version = read_schema_version(db).await?;
    if from_version >= SCHEMA_VERSION {
        define_extracted_index(db).await?;
        return Ok(MigrationReport {
            from_version,
            to_version: from_version,
            edges_backfilled: 0,
            episodes_backfilled: 0,
        });
    }

    let edges_backfilled = if from_version < 1 {
        backfill_edge_evidence(db).await?
    } else {
        0
    };
    let episodes_backfilled = if from_version < 2 {
        backfill_episode_extracted(db).await?
    } else {
        0
    };
    define_extracted_index(db).await?;
    write_schema_version(db, SCHEMA_VERSION).await?;

    Ok(MigrationReport {
        from_version,
        to_version: SCHEMA_VERSION,
        edges_backfilled,
        episodes_backfilled,
    })
}

/// Give every episode written before the `extracted` field existed the value
/// its absence already meant: not extracted.
///
/// `DEFAULT` applies at creation, not retroactively, so those rows carry no
/// value at all, and in SurrealDB `NONE != false`. Resolving that in the
/// query — `(extracted ?? false) != true` — is correct but un-indexable, and
/// the background worker runs it once per poll interval for as long as the
/// machine is quiet. This turns a permanent full-table scan into an index
/// lookup, once.
///
/// Batched on purpose. Every command's open path runs this, and rewriting a
/// whole episode table in one transaction is the shape that exhausts
/// SurrealKV's memtable arena; each batch of [`BACKFILL_BATCH`] is its own
/// statement, and therefore its own transaction. Re-runnable at any point:
/// `WHERE extracted IS NONE` never selects a row twice.
///
/// Two guards, because a backfill that reported success while leaving legacy
/// rows behind would strand them forever — the scan this migration enables
/// cannot see them at all. The loop cannot run longer than the number of
/// rows it found to do, and it ends with a post-condition: if a single
/// episode is still without a value, this returns `Err` and the version
/// marker is never written.
async fn backfill_episode_extracted(db: &Surreal<Db>) -> Result<u64, GraphError> {
    let outstanding = count_absent_extracted(db).await?;
    if outstanding == 0 {
        return Ok(0);
    }

    let mut backfilled = 0u64;
    while backfilled < outstanding {
        let touched = backfill_extracted_batch(db).await?;
        if touched == 0 {
            break;
        }
        backfilled += touched;
    }

    let remaining = count_absent_extracted(db).await?;
    if remaining > 0 {
        return Err(GraphError::Migration(format!(
            "{remaining} of {outstanding} episodes still have no `extracted` value \
             after the backfill. The extraction scan cannot see an episode without \
             that value, so the store is not being opened with the migration half \
             done."
        )));
    }

    Ok(backfilled)
}

/// Give up to [`BACKFILL_BATCH`] episodes a value, and say how many.
///
/// Two statements rather than one `UPDATE … WHERE`, because SurrealDB has no
/// `LIMIT` on `UPDATE`: select the ids, update exactly those, count them
/// server-side. `RETURN NONE` on the update and a `count()` on the way out
/// keep the touched records from crossing the wire, let alone a `Vec`.
///
/// `check()` first: a per-statement failure — a row that cannot satisfy the
/// SCHEMAFULL definition, say — is reported against the statement, not the
/// call, and taking only the count would step straight over it.
async fn backfill_extracted_batch(db: &Surreal<Db>) -> Result<u64, GraphError> {
    let mut response = db
        .query(
            "LET $batch = (SELECT VALUE id FROM episode WHERE extracted IS NONE LIMIT $limit);
             UPDATE $batch SET extracted = false RETURN NONE;
             RETURN count($batch);",
        )
        .bind(("limit", BACKFILL_BATCH as i64))
        .await?
        .check()?;

    let counted: Option<i64> = super::deserialize_take_opt(&mut response, 2)?;
    Ok(counted.unwrap_or(0).max(0) as u64)
}

/// Declare the index that serves the extraction scan.
///
/// Lives here rather than in [`define_schema`] because of when it runs: built
/// *before* the version-2 backfill it is maintained row by row through a
/// whole-table rewrite, which on a 40k-episode store costs 3.6× what building
/// it once over finished data does. `IF NOT EXISTS`, and called on the
/// no-migration path too, so a store that loses the index still regains it on
/// the next open.
///
/// The backfill therefore runs unindexed — a one-time scan, against a
/// one-time cost it more than repays.
async fn define_extracted_index(db: &Surreal<Db>) -> Result<(), GraphError> {
    db.query("DEFINE INDEX IF NOT EXISTS episode_extracted ON episode FIELDS extracted")
        .await?
        .check()?;

    Ok(())
}

/// How many episodes still carry no `extracted` value.
///
/// Served by the `episode_extracted` index once it exists (`IndexCountScan`
/// on SurrealDB 3.2.4), which is what makes it cheap enough for the status
/// path of a healthy store. During the version-2 migration the index does not
/// exist yet and this is a table scan — run exactly twice there, once to size
/// the work and once to prove it finished.
pub(crate) async fn count_absent_extracted(db: &Surreal<Db>) -> Result<u64, GraphError> {
    #[derive(serde::Deserialize)]
    struct CountRow {
        count: u64,
    }

    let mut response = db
        .query("SELECT count() AS count FROM episode WHERE extracted IS NONE GROUP ALL")
        .await?;
    let rows: Vec<CountRow> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.first().map(|r| r.count).unwrap_or(0))
}

/// Give every evidence-less edge the Beta counts implied by its stored mean.
///
/// `alpha = confidence · C`, `beta = (1 − confidence) · C` with
/// `C = PRIOR_CONCENTRATION`: the mean is preserved exactly, and the edge
/// gains the honest low concentration of something never actually counted.
///
/// A single re-runnable statement — `WHERE alpha IS NONE` makes re-entry a
/// no-op for edges that already have evidence.
async fn backfill_edge_evidence(db: &Surreal<Db>) -> Result<u64, GraphError> {
    let mut response = db
        .query(
            r#"
            LET $stale = (SELECT VALUE id FROM relates_to WHERE alpha IS NONE);
            UPDATE $stale SET
                alpha = confidence * $concentration,
                beta = (1 - confidence) * $concentration,
                self_reinforcements = 0
            RETURN NONE;
            RETURN count($stale);
            "#,
        )
        .bind(("concentration", PRIOR_CONCENTRATION))
        .await?
        .check()?;

    let counted: Option<i64> = super::deserialize_take_opt(&mut response, 2)?;
    Ok(counted.unwrap_or(0).max(0) as u64)
}

/// Read the store's schema version. An absent meta record means version 0 —
/// a store written before versioning existed.
async fn read_schema_version(db: &Surreal<Db>) -> Result<i64, GraphError> {
    let mut response = db
        .query("SELECT schema_version FROM type::record($id)")
        .bind(("id", META_RECORD.to_string()))
        .await?;

    #[derive(serde::Deserialize)]
    struct VersionRow {
        schema_version: i64,
    }

    let rows: Vec<VersionRow> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.first().map(|r| r.schema_version).unwrap_or(0))
}

/// Move the version marker forward, never back.
///
/// The `WHERE` makes the write a compare-and-set: a pass that finishes after
/// a newer one — the tail of a race in server mode, or a stale process waking
/// up — cannot lower the version another build has already claimed. On a
/// store with no marker at all the `UPSERT` creates it.
async fn write_schema_version(db: &Surreal<Db>, version: i64) -> Result<(), GraphError> {
    db.query(
        "UPSERT type::record($id) SET schema_version = $version WHERE schema_version < $version",
    )
    .bind(("id", META_RECORD.to_string()))
    .bind(("version", version))
    .await?
    .check()?;
    Ok(())
}

/// Forget which migrations have run, so the next [`init_schema`] runs them
/// all again. Backs `recall-echo graph migrate --force`, the repair for a
/// store whose marker claims work that did not actually land.
pub(crate) async fn clear_schema_version(db: &Surreal<Db>) -> Result<(), GraphError> {
    db.query("DELETE type::record($id)")
        .bind(("id", META_RECORD.to_string()))
        .await?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_both_backfills() {
        let report = MigrationReport {
            from_version: 0,
            to_version: 2,
            edges_backfilled: 3,
            episodes_backfilled: 5000,
        };
        assert_eq!(
            report.summary(),
            "graph schema migrated v0 → v2 (3 edges, 5000 episodes backfilled)"
        );
        assert!(report.ran());
    }

    #[test]
    fn lock_message_detected() {
        assert!(is_lock_message(
            "database: the database at /x/surreal/lock is already locked by another process"
        ));
        assert!(is_lock_message("file lock held by another process"));
    }

    #[test]
    fn conflict_messages_are_worth_retrying() {
        assert!(is_conflict_message(
            "there was a read or write conflict with another transaction"
        ));
        assert!(is_conflict_message("transaction conflict: please retry"));
    }

    #[test]
    fn other_failures_are_not_retried() {
        // A schema violation is the same on every attempt; retrying it only
        // delays the error the caller has to see.
        assert!(!is_conflict_message(
            "couldn't coerce value for field `abstract`: expected `string` but found `none`"
        ));
        assert!(!is_conflict_message("the table 'episode' does not exist"));
        assert!(!is_retryable_conflict(&GraphError::Migration(
            "1 episode still has no `extracted` value".into()
        )));
    }

    #[test]
    fn a_newer_store_is_refused_and_an_older_one_is_not() {
        assert!(refuse_newer_store(SCHEMA_VERSION + 1).is_err());
        assert!(refuse_newer_store(SCHEMA_VERSION).is_ok());
        assert!(refuse_newer_store(0).is_ok());

        let message = refuse_newer_store(SCHEMA_VERSION + 1)
            .expect_err("must refuse")
            .to_string();
        assert!(
            message.contains(&(SCHEMA_VERSION + 1).to_string()),
            "{message}"
        );
        assert!(message.contains(&SCHEMA_VERSION.to_string()), "{message}");
    }

    #[test]
    fn non_lock_messages_pass_through() {
        assert!(!is_lock_message("connection refused"));
        assert!(!is_lock_message("lockstep protocol mismatch")); // 'lock' without already/held
        assert!(!is_lock_message("table entity already exists"));
    }
}

#[cfg(test)]
mod url_compat_tests {
    use super::normalize_server_url;

    #[test]
    fn schemeless_urls_keep_meaning_websocket() {
        assert_eq!(
            normalize_server_url("127.0.0.1:8787"),
            "ws://127.0.0.1:8787"
        );
        assert_eq!(normalize_server_url("db.local:8000"), "ws://db.local:8000");
    }

    #[test]
    fn explicit_schemes_pass_through_untouched() {
        for url in [
            "ws://127.0.0.1:8787",
            "wss://db.example:443",
            "http://localhost:8000",
            "surrealkv:///var/lib/store",
            "mem://",
        ] {
            assert_eq!(normalize_server_url(url), url);
        }
    }
}
