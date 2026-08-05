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
pub const SCHEMA_VERSION: i64 = 1;

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

/// True if a SurrealDB error indicates the embedded store's process-exclusive
/// file lock is held by another process.
fn is_lock_error(err: &surrealdb::Error) -> bool {
    is_lock_message(&err.to_string().to_lowercase())
}

fn is_lock_message(msg: &str) -> bool {
    msg.contains("lock") && (msg.contains("already") || msg.contains("held"))
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
                     backend allows one process at a time — retried {} times. \
                     Another recall-echo command (or the serve daemon) is using it; \
                     wait for it to finish, or use server mode to share the store.",
                    surreal_path.display(),
                    LOCK_RETRY_ATTEMPTS
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
pub async fn init_schema(db: &Surreal<Db>) -> Result<MigrationReport, GraphError> {
    define_schema(db).await?;
    migrate(db).await
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
        DEFINE INDEX IF NOT EXISTS episode_vector  ON episode FIELDS embedding HNSW DIMENSION 384 DIST COSINE;

        DEFINE TABLE IF NOT EXISTS contributed_to SCHEMAFULL TYPE RELATION;
        DEFINE FIELD IF NOT EXISTS outcome_result ON contributed_to TYPE string;
        DEFINE FIELD IF NOT EXISTS was_used       ON contributed_to TYPE bool DEFAULT true;
        DEFINE FIELD IF NOT EXISTS session_id     ON contributed_to TYPE string;
        DEFINE FIELD IF NOT EXISTS timestamp      ON contributed_to TYPE datetime DEFAULT time::now();

        DEFINE INDEX IF NOT EXISTS ct_session ON contributed_to FIELDS session_id;

        DEFINE TABLE IF NOT EXISTS meta SCHEMAFULL;
        DEFINE FIELD IF NOT EXISTS schema_version ON meta TYPE int DEFAULT 0;
        "#,
    )
    .await?
    .check()?;

    Ok(())
}

/// What one migration pass did. `edges_backfilled` is zero on an already
/// current store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Schema version the store was at when the pass started.
    pub from_version: i64,
    /// Schema version the store is at now.
    pub to_version: i64,
    /// Number of edges that gained evidence counts in this pass.
    pub edges_backfilled: u64,
}

impl MigrationReport {
    /// True when this pass actually moved the store forward.
    #[must_use]
    pub fn ran(&self) -> bool {
        self.from_version < self.to_version
    }
}

/// Bring the store up to [`SCHEMA_VERSION`].
///
/// Crash-only: the backfill runs *before* the version marker is written, and
/// only touches edges that still lack evidence (`alpha IS NONE`). An
/// interrupted pass therefore leaves a store that re-opens, finishes the
/// remaining edges, and never counts an edge twice.
async fn migrate(db: &Surreal<Db>) -> Result<MigrationReport, GraphError> {
    let from_version = read_schema_version(db).await?;
    if from_version >= SCHEMA_VERSION {
        return Ok(MigrationReport {
            from_version,
            to_version: from_version,
            edges_backfilled: 0,
        });
    }

    let edges_backfilled = backfill_edge_evidence(db).await?;
    write_schema_version(db, SCHEMA_VERSION).await?;

    Ok(MigrationReport {
        from_version,
        to_version: SCHEMA_VERSION,
        edges_backfilled,
    })
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
            UPDATE relates_to SET
                alpha = confidence * $concentration,
                beta = (1 - confidence) * $concentration,
                self_reinforcements = 0
            WHERE alpha IS NONE
            RETURN id
            "#,
        )
        .bind(("concentration", PRIOR_CONCENTRATION))
        .await?;

    let updated: Vec<serde_json::Value> = super::deserialize_take(&mut response, 0)?;
    Ok(updated.len() as u64)
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

async fn write_schema_version(db: &Surreal<Db>, version: i64) -> Result<(), GraphError> {
    db.query("UPSERT type::record($id) SET schema_version = $version")
        .bind(("id", META_RECORD.to_string()))
        .bind(("version", version))
        .await?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_message_detected() {
        assert!(is_lock_message(
            "database: the database at /x/surreal/lock is already locked by another process"
        ));
        assert!(is_lock_message("file lock held by another process"));
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
