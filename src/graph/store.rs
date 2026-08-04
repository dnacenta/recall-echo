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

use super::error::GraphError;

pub type Db = Any;

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

/// Connect to a SurrealDB server (e.g. `ws://localhost:8787`).
pub async fn connect(config: &ServerConfig) -> Result<Surreal<Db>, GraphError> {
    let db = surrealdb::engine::any::connect(&config.url).await?;
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

/// Initialize the graph schema. Idempotent — safe to call on every open.
pub async fn init_schema(db: &Surreal<Db>) -> Result<(), GraphError> {
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

        DEFINE INDEX IF NOT EXISTS episode_session ON episode FIELDS session_id;
        DEFINE INDEX IF NOT EXISTS episode_time    ON episode FIELDS timestamp;
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
