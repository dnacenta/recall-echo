//! Path resolution utilities for recall-echo.
//!
//! Defaults to entity_root/memory/ layout used by pulse-null.
//! Can be overridden with RECALL_ECHO_HOME environment variable.

use std::path::PathBuf;

/// Returns the default entity root directory.
///
/// Resolution order:
/// 1. RECALL_ECHO_HOME env var (explicit override)
/// 2. Current working directory (for pulse-null entities)
pub fn entity_root() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("RECALL_ECHO_HOME") {
        return Ok(PathBuf::from(p));
    }
    std::env::current_dir().map_err(|e| format!("Could not determine working directory: {e}"))
}

/// Returns the memory directory: {entity_root}/memory/
pub fn memory_dir() -> Result<PathBuf, String> {
    Ok(entity_root()?.join("memory"))
}

pub fn memory_file() -> Result<PathBuf, String> {
    Ok(memory_dir()?.join("MEMORY.md"))
}

pub fn ephemeral_file() -> Result<PathBuf, String> {
    Ok(memory_dir()?.join("EPHEMERAL.md"))
}

pub fn archive_index() -> Result<PathBuf, String> {
    Ok(memory_dir()?.join("ARCHIVE.md"))
}

pub fn conversations_dir() -> Result<PathBuf, String> {
    Ok(memory_dir()?.join("conversations"))
}

pub fn config_file() -> Result<PathBuf, String> {
    Ok(memory_dir()?.join(".recall-echo.toml"))
}
