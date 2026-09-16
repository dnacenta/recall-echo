// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Path resolution utilities for recall-echo.
//!
//! Supports two modes:
//! 1. **Entity mode** (pulse-null) — entity_root/memory/ layout
//! 2. **Claude mode** (standalone) — ~/.claude/ layout for Claude Code hooks

use std::path::{Path, PathBuf};

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

/// The user-level file `init` persists the entity root into, so capture hooks
/// invoked without `--entity-root` still find the store the MCP server was
/// registered with (#46): `$XDG_CONFIG_HOME/recall-echo/entity-root`,
/// defaulting to `~/.config/recall-echo/entity-root`.
fn entity_root_state_file() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()?.join(".config"),
    };
    Some(base.join("recall-echo").join("entity-root"))
}

/// The entity root a previous `init` persisted, if any.
#[must_use]
pub fn persisted_entity_root() -> Option<PathBuf> {
    persisted_entity_root_from(&entity_root_state_file()?)
}

fn persisted_entity_root_from(file: &std::path::Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(file).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Persist `root` as the default entity root for flagless hook invocations.
/// Returns the file written, for the init status line.
pub fn persist_entity_root(root: &std::path::Path) -> Result<PathBuf, RecallError> {
    let file = entity_root_state_file()
        .ok_or_else(|| RecallError::Other("Could not determine home directory".into()))?;
    persist_entity_root_to(&file, root)?;
    Ok(file)
}

fn persist_entity_root_to(
    file: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), RecallError> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Absolute, symlink-resolved when possible: the file outlives the shell
    // (and cwd) that ran init.
    let absolute = std::fs::canonicalize(root).or_else(|_| {
        if root.is_absolute() {
            Ok(root.to_path_buf())
        } else {
            std::env::current_dir().map(|cwd| cwd.join(root))
        }
    })?;
    std::fs::write(file, format!("{}\n", absolute.display())).map_err(RecallError::from)
}

/// Whether `root` carries a layout some `init` (entity or claude-style)
/// already created — the same two shapes `hook_base_dir` routes between.
fn looks_initialized(root: &std::path::Path) -> bool {
    root.join("memory").join("conversations").exists() || root.join("conversations").exists()
}

/// Which arm of the flagless root resolution won.
///
/// Shared by the commands (`entity_root`) and the capture hooks
/// (`hook_entity_root`) so both answer "where is the store?" the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pin {
    /// `RECALL_ECHO_HOME` — taken as given, no existence check.
    Env(PathBuf),
    /// The cwd carries an initialised layout (pulse-null entities run with
    /// cwd = entity home).
    Cwd(PathBuf),
    /// The root a previous `init` persisted, and it still looks initialised.
    Persisted(PathBuf),
    /// Nothing pinned. `stale` names a persisted root that failed the
    /// initialised check, so the caller can say so instead of silently
    /// reading a directory `init` never wrote to (#59 leaves `/tmp` paths).
    None { stale: Option<PathBuf> },
}

/// The pure resolution behind [`entity_root`] and [`hook_entity_root`]:
///
/// 1. `env_home` (`RECALL_ECHO_HOME`) when non-blank,
/// 2. `cwd` when `initialised(cwd)`,
/// 3. `persisted` when `initialised(persisted)`,
/// 4. otherwise [`Pin::None`], carrying a stale `persisted` if there was one.
///
/// Takes every input as a parameter so tests never read the real
/// environment or the real persisted file.
pub(crate) fn entity_root_from(
    env_home: Option<&str>,
    cwd: Option<&Path>,
    persisted: Option<&Path>,
    initialised: impl Fn(&Path) -> bool,
) -> Pin {
    if let Some(home) = env_home {
        if !home.trim().is_empty() {
            return Pin::Env(PathBuf::from(home));
        }
    }
    if let Some(dir) = cwd {
        if initialised(dir) {
            return Pin::Cwd(dir.to_path_buf());
        }
    }
    match persisted {
        Some(root) if initialised(root) => Pin::Persisted(root.to_path_buf()),
        Some(root) => Pin::None {
            stale: Some(root.to_path_buf()),
        },
        None => Pin::None { stale: None },
    }
}

/// [`entity_root_from`] over the real environment, cwd and persisted file.
fn pin_from_environment() -> Pin {
    let env_home = std::env::var("RECALL_ECHO_HOME").ok();
    let cwd = std::env::current_dir().ok();
    let persisted = persisted_entity_root();
    entity_root_from(
        env_home.as_deref(),
        cwd.as_deref(),
        persisted.as_deref(),
        looks_initialized,
    )
}

/// Entity root for a capture hook (`archive-session`, `checkpoint`,
/// `consume`) that may not have received an explicit `--entity-root`.
///
/// Resolution order: the explicit flag, then [`entity_root_from`] — the
/// same chain the commands use. `None` means nothing is pinned anywhere —
/// the caller falls back to the legacy `~/.claude` and should say so out
/// loud rather than no-op silently. A persisted root that no longer looks
/// initialised counts as nothing pinned.
#[must_use]
pub fn hook_entity_root(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    match pin_from_environment() {
        Pin::Env(p) | Pin::Cwd(p) | Pin::Persisted(p) => Some(p),
        Pin::None { .. } => None,
    }
}

/// `hook_base_dir` behind the full flagless resolution, warning loudly on the
/// legacy `~/.claude` fallback instead of silently capturing to the wrong
/// store (#46).
pub fn resolved_hook_base_dir(explicit: Option<&std::path::Path>) -> Result<PathBuf, RecallError> {
    match hook_entity_root(explicit) {
        Some(root) => hook_base_dir(Some(&root)),
        None => {
            eprintln!(
                "recall-echo: no entity root pinned (no --entity-root, RECALL_ECHO_HOME unset, \
                 nothing persisted by `recall-echo init`) — falling back to ~/.claude. \
                 Re-run `recall-echo init` to persist one."
            );
            hook_base_dir(None)
        }
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

    #[test]
    fn persisted_root_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("entity");
        std::fs::create_dir_all(&root).unwrap();
        let file = tmp.path().join("config").join("entity-root");
        persist_entity_root_to(&file, &root).unwrap();
        let read = persisted_entity_root_from(&file).unwrap();
        assert_eq!(read, std::fs::canonicalize(&root).unwrap());
    }

    #[test]
    fn persisted_root_ignores_missing_and_blank_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("entity-root");
        assert_eq!(persisted_entity_root_from(&file), None);
        std::fs::write(&file, "  \n").unwrap();
        assert_eq!(persisted_entity_root_from(&file), None);
    }

    #[test]
    fn explicit_flag_wins_hook_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            hook_entity_root(Some(tmp.path())),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn looks_initialized_recognizes_both_layouts() {
        let entity = tempfile::tempdir().unwrap();
        assert!(!looks_initialized(entity.path()));
        std::fs::create_dir_all(entity.path().join("memory/conversations")).unwrap();
        assert!(looks_initialized(entity.path()));

        let claude = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(claude.path().join("conversations")).unwrap();
        assert!(looks_initialized(claude.path()));
    }

    fn initialised_if(target: &Path) -> impl Fn(&Path) -> bool + '_ {
        move |p| p == target
    }

    #[test]
    fn env_home_wins_over_everything() {
        let cwd = Path::new("/cwd");
        let persisted = Path::new("/persisted");
        let pin = entity_root_from(Some("/env"), Some(cwd), Some(persisted), |_| true);
        assert_eq!(pin, Pin::Env(PathBuf::from("/env")));
    }

    #[test]
    fn empty_env_home_is_unset() {
        let cwd = Path::new("/cwd");
        let pin = entity_root_from(Some("  "), Some(cwd), None, |_| true);
        assert_eq!(pin, Pin::Cwd(cwd.to_path_buf()));
    }

    #[test]
    fn initialised_cwd_beats_persisted() {
        let cwd = Path::new("/cwd");
        let persisted = Path::new("/persisted");
        let pin = entity_root_from(None, Some(cwd), Some(persisted), |_| true);
        assert_eq!(pin, Pin::Cwd(cwd.to_path_buf()));
    }

    #[test]
    fn persisted_root_used_when_cwd_not_initialised() {
        let cwd = Path::new("/cwd");
        let persisted = Path::new("/persisted");
        let pin = entity_root_from(None, Some(cwd), Some(persisted), initialised_if(persisted));
        assert_eq!(pin, Pin::Persisted(persisted.to_path_buf()));
    }

    #[test]
    fn stale_persisted_root_is_reported_not_used() {
        let cwd = Path::new("/cwd");
        let persisted = Path::new("/tmp/.tmpGone");
        let pin = entity_root_from(None, Some(cwd), Some(persisted), |_| false);
        assert_eq!(
            pin,
            Pin::None {
                stale: Some(persisted.to_path_buf())
            }
        );
    }

    #[test]
    fn nothing_pinned_is_none() {
        let cwd = Path::new("/cwd");
        let pin = entity_root_from(None, Some(cwd), None, |_| false);
        assert_eq!(pin, Pin::None { stale: None });
        let pin = entity_root_from(None, None, None, |_| true);
        assert_eq!(pin, Pin::None { stale: None });
    }
}
