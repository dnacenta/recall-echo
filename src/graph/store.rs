//! SurrealDB embedded store — open, schema init.

use std::path::Path;

use surrealdb::engine::local::SurrealKv;
use surrealdb::Surreal;

use super::error::GraphError;

pub type Db = surrealdb::engine::local::Db;

const NAMESPACE: &str = "recall";
const DATABASE: &str = "graph";

/// Open (or create) a SurrealDB embedded store at the given path.
pub async fn open(path: &Path) -> Result<Surreal<Db>, GraphError> {
    let surreal_path = path.join("surreal");
    std::fs::create_dir_all(&surreal_path)?;

    let db: Surreal<Db> = Surreal::new::<SurrealKv>(surreal_path.to_str().unwrap()).await?;
    db.use_ns(NAMESPACE).use_db(DATABASE).await?;

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
        DEFINE FIELD IF NOT EXISTS confidence      ON relates_to TYPE float DEFAULT 1.0;
        DEFINE FIELD IF NOT EXISTS last_reinforced ON relates_to TYPE datetime DEFAULT time::now();
        DEFINE FIELD IF NOT EXISTS source          ON relates_to TYPE option<string>;

        -- Backfill: set last_reinforced = valid_from on relationships that predate the field
        UPDATE relates_to SET last_reinforced = valid_from WHERE last_reinforced IS NONE;

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

        -- Backfill: set extracted = false on episodes that predate the field
        UPDATE episode SET extracted = false WHERE extracted IS NONE;

        DEFINE INDEX IF NOT EXISTS episode_session ON episode FIELDS session_id;
        DEFINE INDEX IF NOT EXISTS episode_time    ON episode FIELDS timestamp;
        DEFINE INDEX IF NOT EXISTS episode_vector  ON episode FIELDS embedding HNSW DIMENSION 384 DIST COSINE;
        "#,
    )
    .await?
    .check()?;

    Ok(())
}
