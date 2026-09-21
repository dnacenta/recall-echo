// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Path resolution utilities for recall-echo.
//!
//! Supports two modes:
//! 1. **Entity mode** (pulse-null) — entity_root/memory/ layout
//! 2. **Claude mode** (standalone) — ~/.claude/ layout for Claude Code hooks

use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::RecallError;

/// Returns the default entity root directory for a flagless command.
///
/// Resolution order (the same chain the capture hooks use, see
/// [`resolve_root_source`]):
/// 1. `RECALL_ECHO_HOME` (explicit override)
/// 2. the cwd, when it carries an initialised layout (pulse-null entities)
/// 3. the root `recall-echo init` persisted, when it is still trusted:
///    absolute, exists, a directory this user owns that nobody else can
///    write into (see [`trusted_root`])
/// 4. the cwd — so the existing "run init first" errors keep naming the
///    directory the user is in
///
/// A persisted root that is no longer trusted is ignored with one stderr
/// warning per process naming the path and the reason.
pub fn entity_root() -> Result<PathBuf, RecallError> {
    Ok(entity_root_described()?.0)
}

/// [`entity_root`] plus a short label saying which arm chose it, for
/// commands that should tell the user which store they are looking at.
pub fn entity_root_described() -> Result<(PathBuf, &'static str), RecallError> {
    let (source, cwd) = resolve_from_environment();
    let label = source.label();
    let (root, stale) = source.into_command_root(cwd)?;
    if let Some(stale) = stale {
        warn_stale_persisted_root(&stale);
    }
    Ok((root, label))
}

/// A persisted root that was refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleRoot {
    path: PathBuf,
    reason: String,
}

fn warn_stale_persisted_root(stale: &StaleRoot) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // `{:?}` escapes control characters: the path comes from a file.
        eprintln!(
            "recall-echo: ignoring persisted entity root {:?}: it {}; re-run `recall-echo init`",
            stale.path, stale.reason
        );
    });
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

// ── Where `init` writes outside the store ────────────────────────────────

/// The recall-echo binary that hooks and MCP registrations should point at.
#[must_use]
pub(crate) fn recall_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "recall-echo".into())
}

/// True when `exe` sits in a Cargo build directory.
///
/// Such a binary is a test harness or a working copy: pinning a user's hooks,
/// MCP registrations or global entity-root pointer to it would break the
/// moment the tree is cleaned.
///
/// The test is the *shape* Cargo produces, not the string `target`, because
/// `CARGO_TARGET_DIR` renames that directory freely (#59): the binary's parent
/// is `debug`/`release`, or `deps` inside one. A substring match would both
/// miss `/home/d/build/debug/recall-echo` and libel
/// `/opt/apps/release/deps/bin/recall-echo`.
#[must_use]
pub(crate) fn is_build_dir(exe: &str) -> bool {
    fn dir_name(dir: Option<&Path>) -> Option<&str> {
        dir?.file_name()?.to_str()
    }
    let parent = Path::new(exe).parent();
    match dir_name(parent) {
        Some("debug" | "release") => true,
        Some("deps") => matches!(
            dir_name(parent.and_then(Path::parent)),
            Some("debug" | "release")
        ),
        _ => false,
    }
}

/// Environment variables an agent CLI reads to find its own user config, and
/// where each one sits relative to a home directory (`None` = the home itself).
///
/// `claude mcp add` writes `~/.claude.json`, `codex mcp add` writes
/// `~/.codex/config.toml`: recall-echo never opens those files, so the only
/// way to redirect them is the child's environment.
const AGENT_CONFIG_ENV: [(&str, Option<&str>); 4] = [
    ("HOME", None),
    ("XDG_CONFIG_HOME", Some(".config")),
    ("CLAUDE_CONFIG_DIR", Some(".claude")),
    ("CODEX_HOME", Some(".codex")),
];

/// What became of the persisted entity-root pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistOutcome {
    /// Written to this file.
    Written(PathBuf),
    /// Deliberately not written, and why — phrased to follow "Skipped … — ".
    Skipped(&'static str),
}

/// The global destinations `init` writes to, resolved once and passed down.
///
/// `init` touches three things nobody named on the command line: the Claude
/// Code hook file (`~/.claude/settings.json`), the persisted entity root
/// (`$XDG_CONFIG_HOME/recall-echo/entity-root`), and — through the agent CLIs
/// it shells out to — their own user config. Resolving those paths inside each
/// writer left a caller no way to redirect them, and the test suite rewrote the
/// developer's real configuration on every run (#59). They are an input now,
/// together with the binary path those files are written to point at.
///
/// Construct with [`ConfigRoots::from_env`] in production and
/// [`ConfigRoots::sandboxed`] anywhere the writes must not escape a directory.
#[derive(Debug, Clone)]
pub struct ConfigRoots {
    claude_dir: Option<PathBuf>,
    /// The persisted-pointer destination, or why there is none.
    entity_root_file: Result<PathBuf, &'static str>,
    agent_home: Option<PathBuf>,
    recall_bin: String,
    spawns_agents: bool,
}

impl ConfigRoots {
    /// The real user's configuration: `~/.claude` when Claude Code is
    /// installed (or `RECALL_ECHO_CLAUDE_DIR` when set), the XDG state file,
    /// and the ambient environment for spawned agent CLIs.
    ///
    /// A binary in a build directory persists no pointer: `cargo run -- init
    /// /tmp/scratch` from a checkout would otherwise repoint every flagless
    /// command at `/tmp/scratch` for good.
    #[must_use]
    pub fn from_env() -> Self {
        let recall_bin = recall_binary();
        let entity_root_file = if is_build_dir(&recall_bin) {
            Err("running from a build directory")
        } else {
            entity_root_state_file().ok_or("there is no home directory to persist into")
        };
        Self {
            claude_dir: detect_claude_code(),
            entity_root_file,
            agent_home: None,
            recall_bin,
            spawns_agents: true,
        }
    }

    /// Every destination under `dir`, which is created if it does not exist.
    ///
    /// `<dir>/.claude` is created 0700, so hook installation is exercised
    /// rather than skipped for want of a Claude Code install to detect, and
    /// the hooks point at `<dir>/bin/recall-echo` — a production-shaped path,
    /// so no guard fires and the writers really run. Nothing here spawns an
    /// agent CLI or downloads a model (see [`ConfigRoots::spawns_agents`]).
    pub fn sandboxed(dir: &Path) -> Result<Self, RecallError> {
        let claude_dir = dir.join(".claude");
        std::fs::create_dir_all(&claude_dir)?;
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            claude_dir: Some(claude_dir),
            entity_root_file: Ok(dir.join(".config").join("recall-echo").join("entity-root")),
            agent_home: Some(dir.to_path_buf()),
            recall_bin: dir.join("bin").join("recall-echo").display().to_string(),
            spawns_agents: false,
        })
    }

    /// The same roots with no Claude Code installed — the branch where hooks
    /// have nowhere to go.
    #[must_use]
    pub fn without_claude_code(mut self) -> Self {
        self.claude_dir = None;
        self
    }

    /// The Claude Code directory hooks belong in, when there is one.
    #[must_use]
    pub fn claude_dir(&self) -> Option<&Path> {
        self.claude_dir.as_deref()
    }

    /// The file the entity-root pointer is written to, when one is written.
    #[must_use]
    pub fn entity_root_file(&self) -> Option<&Path> {
        self.entity_root_file.as_deref().ok()
    }

    /// The binary path written into hooks and MCP registrations.
    #[must_use]
    pub fn recall_bin(&self) -> &str {
        &self.recall_bin
    }

    /// Whether this configuration may reach out to the machine: run an agent
    /// CLI's `mcp add`, or download the embedding model. False for a sandbox,
    /// where the point is that nothing outside `dir` happens at all.
    #[must_use]
    pub fn spawns_agents(&self) -> bool {
        self.spawns_agents
    }

    /// Environment an agent CLI must run under to keep its config writes
    /// inside these roots. Empty for [`ConfigRoots::from_env`]: a real
    /// registration wants the user's real config.
    #[must_use]
    pub fn agent_env(&self) -> Vec<(&'static str, PathBuf)> {
        let Some(home) = &self.agent_home else {
            return Vec::new();
        };
        AGENT_CONFIG_ENV
            .iter()
            .map(|(key, subdir)| {
                let value = subdir.map_or_else(|| home.clone(), |sub| home.join(sub));
                (*key, value)
            })
            .collect()
    }

    /// Persist `root` as the default entity root for flagless invocations.
    pub fn persist_entity_root(&self, root: &Path) -> Result<PersistOutcome, RecallError> {
        match &self.entity_root_file {
            Ok(file) => {
                persist_entity_root_to(file, root)?;
                Ok(PersistOutcome::Written(file.clone()))
            }
            Err(why) => Ok(PersistOutcome::Skipped(why)),
        }
    }
}

fn persist_entity_root_to(
    file: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), RecallError> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
        // The file steers every flagless command: nobody else gets to read
        // where the store is, let alone rewrite it.
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
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
    use std::io::Write as _;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    out.write_all(format!("{}\n", absolute.display()).as_bytes())?;
    out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
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
enum RootSource {
    /// `RECALL_ECHO_HOME` — taken as given, no existence check.
    Env(PathBuf),
    /// The cwd carries an initialised layout (pulse-null entities run with
    /// cwd = entity home).
    Cwd(PathBuf),
    /// The root a previous `init` persisted, canonical, and still trusted.
    Persisted(PathBuf),
    /// Nothing pinned. `stale` names a persisted root that was refused, so
    /// the caller can say so instead of silently reading a directory `init`
    /// never wrote to (#59 leaves `/tmp` paths behind).
    Unpinned { stale: Option<StaleRoot> },
}

impl RootSource {
    /// The root a command uses, plus the stale persisted root to warn about.
    /// Nothing pinned falls back to the cwd, exactly as 4.4.0 did, so every
    /// existing "run init first" error keeps its shape.
    fn into_command_root(
        self,
        cwd: Option<PathBuf>,
    ) -> Result<(PathBuf, Option<StaleRoot>), RecallError> {
        match self {
            Self::Env(p) | Self::Cwd(p) | Self::Persisted(p) => Ok((p, None)),
            Self::Unpinned { stale } => {
                let cwd = cwd.ok_or_else(|| {
                    RecallError::Other("Could not determine the current directory".into())
                })?;
                Ok((cwd, stale))
            }
        }
    }

    /// The hook side: pinned or not, no fallback — the caller decides what
    /// "nothing pinned" means — plus the stale root to warn about.
    fn into_hook_root(self) -> (Option<PathBuf>, Option<StaleRoot>) {
        match self {
            Self::Env(p) | Self::Cwd(p) | Self::Persisted(p) => (Some(p), None),
            Self::Unpinned { stale } => (None, stale),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Env(_) => "from RECALL_ECHO_HOME",
            Self::Cwd(_) => "the current directory",
            Self::Persisted(_) => "persisted by `recall-echo init`",
            Self::Unpinned { .. } => "the current directory (nothing pinned)",
        }
    }
}

/// The pure resolution behind [`entity_root`] and [`hook_entity_root`]:
///
/// 1. `env_home` (`RECALL_ECHO_HOME`) when non-blank,
/// 2. `cwd` when `initialised(cwd)`,
/// 3. `persisted()` when `trusted` accepts it (yielding its canonical path),
/// 4. otherwise [`RootSource::Unpinned`], carrying the refused persisted
///    root and the reason if there was one.
///
/// Takes every input as a parameter — and the persisted root lazily, so the
/// file is only read when the first two arms lose — so tests never touch the
/// real environment or the real persisted file.
fn resolve_root_source(
    env_home: Option<&str>,
    cwd: Option<&Path>,
    persisted: impl FnOnce() -> Option<PathBuf>,
    initialised: impl Fn(&Path) -> bool,
    trusted: impl Fn(&Path) -> Result<PathBuf, String>,
) -> RootSource {
    if let Some(home) = env_home {
        if !home.trim().is_empty() {
            return RootSource::Env(PathBuf::from(home));
        }
    }
    if let Some(dir) = cwd {
        if initialised(dir) {
            return RootSource::Cwd(dir.to_path_buf());
        }
    }
    match persisted() {
        Some(root) => match trusted(&root) {
            Ok(canonical) => RootSource::Persisted(canonical),
            Err(reason) => RootSource::Unpinned {
                stale: Some(StaleRoot { path: root, reason }),
            },
        },
        None => RootSource::Unpinned { stale: None },
    }
}

/// Whether a persisted root may steer a command: it must be absolute, exist,
/// resolve to a real directory owned by this user that no other user can
/// write into. Returns the canonical path. The persisted file is plain text
/// the test suite has been known to overwrite with a `/tmp` path (#59) — a
/// directory anyone can recreate there must never win.
fn trusted_root(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("is not an absolute path".into());
    }
    let canonical =
        std::fs::canonicalize(root).map_err(|err| format!("cannot be resolved ({err})"))?;
    let meta = std::fs::symlink_metadata(&canonical)
        .map_err(|err| format!("cannot be inspected ({err})"))?;
    if !meta.is_dir() {
        return Err("is not a directory".into());
    }
    let owner = crate::serve_security::current_uid().map_err(|err| err.to_string())?;
    if meta.uid() != owner {
        return Err(format!("is owned by uid {}, not {owner}", meta.uid()));
    }
    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(format!(
            "is writable by other users (mode {:04o})",
            mode & 0o7777
        ));
    }
    Ok(canonical)
}

/// [`resolve_root_source`] over the real environment, cwd and persisted
/// file. The cwd is read once and handed back so the fallback can never
/// disagree with the directory that was probed.
fn resolve_from_environment() -> (RootSource, Option<PathBuf>) {
    let env_home = std::env::var("RECALL_ECHO_HOME").ok();
    let cwd = std::env::current_dir().ok();
    let source = resolve_root_source(
        env_home.as_deref(),
        cwd.as_deref(),
        persisted_entity_root,
        looks_initialized,
        trusted_root,
    );
    (source, cwd)
}

/// Entity root for a capture hook (`archive-session`, `checkpoint`,
/// `consume`) that may not have received an explicit `--entity-root`.
///
/// Resolution order: the explicit flag, then [`resolve_root_source`] — the
/// same chain the commands use. `None` means nothing is pinned anywhere —
/// the caller falls back to the legacy `~/.claude` and should say so out
/// loud rather than no-op silently. A persisted root that is no longer
/// trusted counts as nothing pinned, and is named on stderr.
#[must_use]
pub fn hook_entity_root(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let (source, _) = resolve_from_environment();
    let (root, stale) = source.into_hook_root();
    if let Some(stale) = stale {
        warn_stale_persisted_root(&stale);
    }
    root
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
                 nothing usable persisted by `recall-echo init`) — falling back to ~/.claude. \
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

    fn no_persisted() -> Option<PathBuf> {
        None
    }

    fn persisted(p: &Path) -> impl FnOnce() -> Option<PathBuf> + '_ {
        move || Some(p.to_path_buf())
    }

    fn trust_all(p: &Path) -> Result<PathBuf, String> {
        Ok(p.to_path_buf())
    }

    fn trust_none(_: &Path) -> Result<PathBuf, String> {
        Err("is not trusted".into())
    }

    #[test]
    fn env_home_wins_over_everything_without_checks() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(
            Some("/env"),
            Some(cwd),
            persisted(Path::new("/persisted")),
            |_| false,
            trust_none,
        );
        assert_eq!(source, RootSource::Env(PathBuf::from("/env")));
    }

    #[test]
    fn empty_env_home_is_unset() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(Some("  "), Some(cwd), no_persisted, |_| true, trust_all);
        assert_eq!(source, RootSource::Cwd(cwd.to_path_buf()));
    }

    #[test]
    fn initialised_cwd_beats_persisted() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(
            None,
            Some(cwd),
            persisted(Path::new("/persisted")),
            |_| true,
            trust_all,
        );
        assert_eq!(source, RootSource::Cwd(cwd.to_path_buf()));
    }

    #[test]
    fn persisted_file_is_not_read_when_cwd_wins() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(
            None,
            Some(cwd),
            || panic!("persisted root read although the cwd won"),
            |_| true,
            trust_all,
        );
        assert_eq!(source, RootSource::Cwd(cwd.to_path_buf()));
    }

    #[test]
    fn trusted_persisted_root_is_used_canonically() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(
            None,
            Some(cwd),
            persisted(Path::new("/persisted")),
            |_| false,
            |p| Ok(p.join("canonical")),
        );
        assert_eq!(
            source,
            RootSource::Persisted(PathBuf::from("/persisted/canonical"))
        );
    }

    #[test]
    fn untrusted_persisted_root_is_reported_not_used() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(
            None,
            Some(cwd),
            persisted(Path::new("/tmp/.tmpGone")),
            |_| false,
            trust_none,
        );
        assert_eq!(
            source,
            RootSource::Unpinned {
                stale: Some(StaleRoot {
                    path: PathBuf::from("/tmp/.tmpGone"),
                    reason: "is not trusted".into(),
                }),
            }
        );
    }

    #[test]
    fn nothing_pinned_is_unpinned() {
        let cwd = Path::new("/cwd");
        let source = resolve_root_source(None, Some(cwd), no_persisted, |_| false, trust_all);
        assert_eq!(source, RootSource::Unpinned { stale: None });
        let source = resolve_root_source(None, None, no_persisted, |_| true, trust_all);
        assert_eq!(source, RootSource::Unpinned { stale: None });
    }

    #[test]
    fn source_mappings_agree_whenever_pinned() {
        let cwd = Path::new("/cwd");
        let sources = [
            RootSource::Env(PathBuf::from("/env")),
            RootSource::Cwd(cwd.to_path_buf()),
            RootSource::Persisted(PathBuf::from("/persisted")),
            RootSource::Unpinned { stale: None },
            RootSource::Unpinned {
                stale: Some(StaleRoot {
                    path: PathBuf::from("/gone"),
                    reason: "is gone".into(),
                }),
            },
        ];
        for source in sources {
            let (hook, hook_stale) = source.clone().into_hook_root();
            let (command, command_stale) =
                source.into_command_root(Some(cwd.to_path_buf())).unwrap();
            assert_eq!(hook_stale, command_stale);
            match hook {
                Some(root) => assert_eq!(command, root),
                None => assert_eq!(command, cwd),
            }
        }
    }

    #[test]
    fn unpinned_without_a_cwd_is_an_error() {
        let source = RootSource::Unpinned { stale: None };
        assert!(source.into_command_root(None).is_err());
    }

    #[test]
    fn trusted_root_refuses_relative_missing_and_shared_dirs() {
        assert!(trusted_root(Path::new("relative"))
            .unwrap_err()
            .contains("absolute"));

        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");
        assert!(trusted_root(&gone).unwrap_err().contains("resolved"));

        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(trusted_root(&shared)
            .unwrap_err()
            .contains("writable by other users"));

        let file = tmp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(trusted_root(&file).unwrap_err().contains("not a directory"));

        let mine = tmp.path().join("mine");
        std::fs::create_dir(&mine).unwrap();
        std::fs::set_permissions(&mine, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            trusted_root(&mine).unwrap(),
            std::fs::canonicalize(&mine).unwrap()
        );
    }

    /// Every destination a sandboxed `init` can write to lives under the
    /// directory handed in — the property the whole of #59 rests on.
    #[test]
    fn sandboxed_roots_point_at_the_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = ConfigRoots::sandboxed(tmp.path()).unwrap();

        for path in [roots.claude_dir(), roots.entity_root_file()] {
            let path = path.expect("a destination inside the sandbox");
            assert!(path.starts_with(tmp.path()), "{}", path.display());
        }
        for (key, value) in roots.agent_env() {
            assert!(
                value.starts_with(tmp.path()),
                "{key} escapes the sandbox: {}",
                value.display()
            );
        }
        assert_eq!(roots.agent_env().len(), AGENT_CONFIG_ENV.len());
        assert!(
            Path::new(roots.recall_bin()).starts_with(tmp.path()),
            "{}",
            roots.recall_bin()
        );
        assert!(!roots.spawns_agents(), "a sandbox runs nothing");
        assert!(roots.claude_dir().unwrap().is_dir(), "created eagerly");
        assert_eq!(
            std::fs::metadata(roots.claude_dir().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    /// The persisted-root writer takes its destination from the roots, so a
    /// test cannot reach `~/.config/recall-echo/entity-root` even by accident.
    #[test]
    fn persisting_through_sandboxed_roots_leaves_the_real_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let real = entity_root_state_file().expect("a real state file path");
        let before = std::fs::read(&real).ok();

        let root = tmp.path().join("entity");
        std::fs::create_dir_all(&root).unwrap();
        let roots = ConfigRoots::sandboxed(tmp.path()).unwrap();
        let written = roots.persist_entity_root(&root).unwrap();

        let file = roots
            .entity_root_file()
            .expect("a destination")
            .to_path_buf();
        assert_eq!(written, PersistOutcome::Written(file.clone()));
        assert!(file.starts_with(tmp.path()), "{}", file.display());
        assert_eq!(
            persisted_entity_root_from(&file).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        assert_eq!(
            std::fs::read(&real).ok(),
            before,
            "{} changed",
            real.display()
        );
    }

    /// A binary in a build directory is a working copy: `cargo run -- init`
    /// must not repoint the pointer every flagless command resolves through.
    /// The test binary is itself such a path, so this also proves the suite
    /// cannot reach the real file through `from_env`.
    #[test]
    fn from_env_in_a_build_directory_persists_nowhere() {
        let roots = ConfigRoots::from_env(); // sanctioned: read-only from_env (RE-59)
        assert!(
            is_build_dir(roots.recall_bin()),
            "test binary is not build-shaped: {}",
            roots.recall_bin()
        );
        assert_eq!(roots.entity_root_file(), None);

        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            roots.persist_entity_root(tmp.path()).unwrap(),
            PersistOutcome::Skipped("running from a build directory")
        );
        assert!(roots.spawns_agents(), "production roots still reach out");
        assert!(
            roots.agent_env().is_empty(),
            "a real registration inherits the user's environment"
        );
    }

    /// The shape Cargo produces, whatever `CARGO_TARGET_DIR` calls the
    /// directory — and nothing that merely contains those words.
    #[test]
    fn build_directories_are_recognised_by_shape() {
        for exe in [
            "/opt/recall-echo/target/debug/recall-echo",
            "/opt/recall-echo/target/release/recall-echo",
            "/opt/shared/target/debug/deps/recall_echo-1a2b3c",
            "/home/d/.cache/rust/release/deps/recall_echo-1a2b3c",
            "/home/d/build/debug/recall-echo",
        ] {
            assert!(is_build_dir(exe), "{exe}");
        }
        for exe in [
            "/usr/local/bin/recall-echo",
            "/home/d/.cargo/bin/recall-echo",
            "/opt/apps/release/deps/bin/recall-echo",
            "/home/d/debug/bin/recall-echo",
            "/tmp/.tmpAbC123/bin/recall-echo",
            "recall-echo",
        ] {
            assert!(!is_build_dir(exe), "{exe}");
        }
    }

    /// Hooks have nowhere to go when Claude Code is not installed.
    #[test]
    fn roots_without_claude_code_name_no_hook_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = ConfigRoots::sandboxed(tmp.path())
            .unwrap()
            .without_claude_code();
        assert_eq!(roots.claude_dir(), None);
        assert!(roots.entity_root_file().is_some(), "the pointer still goes");
    }

    #[test]
    fn persisted_file_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("entity");
        std::fs::create_dir_all(&root).unwrap();
        let file = tmp.path().join("config").join("entity-root");
        persist_entity_root_to(&file, &root).unwrap();
        let dir_mode = std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        assert_eq!(file_mode & 0o777, 0o600);
    }
}
