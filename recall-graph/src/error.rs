//! Typed error handling for recall-graph.

/// All errors that recall-graph operations can produce.
#[derive(thiserror::Error, Debug)]
pub enum GraphError {
    #[error("database: {0}")]
    Db(#[from] surrealdb::Error),

    #[error("embedding: {0}")]
    Embed(String),

    #[error("entity not found: {0}")]
    NotFound(String),

    #[error("extraction failed: {0}")]
    Extraction(String),

    #[error("dedup failed: {0}")]
    Dedup(String),

    #[error("llm error: {0}")]
    Llm(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("immutable entity cannot be merged: {0}")]
    ImmutableMerge(String),
}
