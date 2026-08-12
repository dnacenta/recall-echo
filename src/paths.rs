// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Path resolution utilities for recall-echo.
//!
//! Supports two modes:
//! 1. **Entity mode** (pulse-null) — entity_root/memory/ layout
//! 2. **Claude mode** (standalone) — ~/.claude/ layout for Claude Code hooks

use std::path::PathBuf;

use crate::error::RecallError;

/// Returns the default entity root directory.
///
/// Resolution order:
/// 1. RECALL_ECHO_HOME env var (explicit override)
/// 2. Current working directory (for pulse-null entities)
pub fn entity_root() -> Result<PathBuf, RecallError> {
    if let Ok(p) = std::env::var("RECALL_ECHO_HOME") {
        return Ok(PathBuf::from(p));
    }
    std::env::current_dir().map_err(RecallError::from)
}

/// Returns the memory directory: {entity_root}/memory/
pub fn memory_dir() -> Result<PathBuf, RecallError> {
    Ok(entity_root()?.join("memory"))
}

pub fn memory_file() -> Result<PathBuf, RecallError> {
    Ok(memory_dir()?.join("MEMORY.md"))
}

pub fn ephemeral_file() -> Result<PathBuf, RecallError> {
    Ok(memory_dir()?.join("EPHEMERAL.md"))
}

pub fn archive_index() -> Result<PathBuf, RecallError> {
    Ok(memory_dir()?.join("ARCHIVE.md"))
}

pub fn conversations_dir() -> Result<PathBuf, RecallError> {
    Ok(memory_dir()?.join("conversations"))
}

pub fn config_file() -> Result<PathBuf, RecallError> {
    Ok(memory_dir()?.join(".recall-echo.toml"))
}

/// Returns the Claude Code base directory (~/.claude/).
///
/// Used when recall-echo is invoked as a Claude Code hook (archive-session,
/// checkpoint). The memory layout inside ~/.claude/ mirrors the entity layout:
/// ~/.claude/conversations/, ~/.claude/ARCHIVE.md, ~/.claude/EPHEMERAL.md, etc.
pub fn claude_dir() -> Result<PathBuf, RecallError> {
    let home = dirs::home_dir()
        .ok_or_else(|| RecallError::Other("Could not determine home directory".into()))?;
    Ok(home.join(".claude"))
}

/// Base directory for hook-driven writes (`archive-session`, `checkpoint`).
///
/// With an explicit entity root, data lives in the entity layout at
/// `<root>/memory` — unless the root itself carries a claude-style layout
/// (`<root>/conversations` with no `<root>/memory/conversations`), which is
/// what a standalone `~/.claude` install looks like. Without a root, the
/// legacy behavior: `~/.claude` itself. Mirrors the read-side resolution in
/// `graph_cli::find_conversations_dir`.
pub fn hook_base_dir(entity_root: Option<&std::path::Path>) -> Result<PathBuf, RecallError> {
    match entity_root {
        Some(root) => {
            let memory = root.join("memory");
            if memory.join("conversations").exists() {
                Ok(memory)
            } else if root.join("conversations").exists() {
                Ok(root.to_path_buf())
            } else {
                // Nothing initialized yet — name the entity layout, so the
                // "run init first" error points where init would write.
                Ok(memory)
            }
        }
        None => claude_dir(),
    }
}

/// Expand a leading `~/` to the home directory. Other paths pass through.
#[must_use]
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// Detect Claude Code installation.
/// Returns Some(~/.claude/) if it exists, None otherwise.
#[must_use]
pub fn detect_claude_code() -> Option<PathBuf> {
    // Overridable so tests (and sandboxed runs) never touch the real
    // ~/.claude — hook installation writes settings.json unconditionally,
    // and a test that installs hooks would otherwise repoint the user's
    // live hooks at the test binary.
    if let Some(dir) = std::env::var_os(CLAUDE_DIR_ENV) {
        let claude = PathBuf::from(dir);
        return claude.exists().then_some(claude);
    }
    let home = dirs::home_dir()?;
    let claude = home.join(".claude");
    if claude.exists() {
        Some(claude)
    } else {
        None
    }
}

/// Overrides the Claude Code configuration directory (`~/.claude`).
pub const CLAUDE_DIR_ENV: &str = "RECALL_ECHO_CLAUDE_DIR";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_base_prefers_the_entity_layout() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("memory/conversations")).unwrap();
        let base = hook_base_dir(Some(tmp.path())).unwrap();
        assert_eq!(base, tmp.path().join("memory"));
    }

    #[test]
    fn hook_base_accepts_a_claude_style_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("conversations")).unwrap();
        let base = hook_base_dir(Some(tmp.path())).unwrap();
        assert_eq!(base, tmp.path());
    }

    #[test]
    fn hook_base_names_the_entity_layout_when_uninitialized() {
        let tmp = tempfile::tempdir().unwrap();
        let base = hook_base_dir(Some(tmp.path())).unwrap();
        assert_eq!(base, tmp.path().join("memory"));
    }
}
