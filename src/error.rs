//! Top-level error type for recall-echo.
//!
//! Unifies error handling across the crate. The graph subsystem has its own
//! `GraphError` which is wrapped here for seamless propagation.

use crate::graph::error::GraphError;

/// All errors that recall-echo operations can produce.
#[derive(thiserror::Error, Debug)]
pub enum RecallError {
    /// I/O errors (file reads, writes, directory operations).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization errors.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML serialization errors.
    #[error("toml: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    /// TOML deserialization errors.
    #[error("toml: {0}")]
    TomlDeserialize(#[from] toml::de::Error),

    /// Configuration errors (missing fields, invalid values).
    #[error("config: {0}")]
    Config(String),

    /// Memory system not initialized or missing required files/directories.
    #[error("{0}")]
    NotInitialized(String),

    /// Graph subsystem errors (wraps GraphError).
    #[error("graph: {0}")]
    Graph(#[from] GraphError),

    /// General errors that don't fit other categories.
    #[error("{0}")]
    Other(String),
}

impl From<String> for RecallError {
    fn from(s: String) -> Self {
        RecallError::Other(s)
    }
}

impl From<&str> for RecallError {
    fn from(s: &str) -> Self {
        RecallError::Other(s.to_string())
    }
}

/// Convenience alias used across non-graph modules.
pub type Result<T> = std::result::Result<T, RecallError>;
