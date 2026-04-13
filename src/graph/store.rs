//! SurrealDB store — embedded (kv-surrealkv) or server (WebSocket).

#[cfg(all(feature = "embedded", feature = "server"))]
compile_error!("Features `embedded` and `server` are mutually exclusive. Choose one.");

use surrealdb::Surreal;

use super::error::GraphError;

#[cfg(feature = "embedded")]
use std::path::Path;

#[cfg(feature = "embedded")]
use surrealdb::engine::local::SurrealKv;

#[cfg(feature = "embedded")]
pub type Db = surrealdb::engine::local::Db;

#[cfg(feature = "server")]
pub type Db = surrealdb::engine::remote::ws::Client;

/// Connection config for server mode.
#[cfg(feature = "server")]
#[derive(Clone)]
pub struct ServerConfig {
    pub url: String,
    pub username: String,
    pub password: String,
    pub namespace: String,
    pub database: String,
}

#[cfg(feature = "server")]
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

/// Open (or create) a SurrealDB embedded store at the given path.
#[cfg(feature = "embedded")]
pub async fn open(path: &Path) -> Result<Surreal<Db>, GraphError> {
    let surreal_path = path.join("surreal");
    std::fs::create_dir_all(&surreal_path)?;

    let path_str = surreal_path.to_str().ok_or_else(|| {
        GraphError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "graph store path contains non-UTF8 characters",
        ))
    })?;
    let db: Surreal<Db> = Surreal::new::<SurrealKv>(path_str).await?;
    db.use_ns("recall").use_db("graph").await?;

    Ok(db)
}

/// Connect to a SurrealDB server over WebSocket.
#[cfg(feature = "server")]
pub async fn connect(config: &ServerConfig) -> Result<Surreal<Db>, GraphError> {
    let db = Surreal::new::<surrealdb::engine::remote::ws::Ws>(&config.url).await?;
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
