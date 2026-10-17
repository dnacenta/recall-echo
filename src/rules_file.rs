// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Claude Code rules file that puts Layer 1 into every session.
//!
//! Nothing else loads `memory/MEMORY.md` into a Claude Code session (#75).
//! The SessionStart hook cannot carry it: Claude Code inlines hook stdout
//! only up to about 10,000 characters and swaps anything longer for a head
//! preview and a file path, and a curated MEMORY.md is routinely longer.
//! Memory files under `~/.claude/rules/` are not capped that way, and their
//! `@path` imports are inlined whole, on every startup, resume and `/clear`.
//!
//! So `init` writes `~/.claude/rules/recall-echo.md`, rendered from
//! `templates/recall-echo.md` with this store's paths, holding exactly one
//! import: `@<root>/memory/MEMORY.md`. EPHEMERAL.md is deliberately not
//! imported — the `consume` hook already prints it, and a second copy would
//! put it in context twice.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// First line of every file this module writes. A rules file without it is
/// the user's own and is never touched.
pub const MARKER: &str = "<!-- recall-echo:managed — rewritten by `recall-echo init`; delete this line to keep your own edits -->";

/// What identifies [`MARKER`]: the line's fixed opening, so the prose after
/// it can be reworded in a later release without orphaning every file an
/// earlier one wrote.
const MARKER_TOKEN: &str = "<!-- recall-echo:managed";

/// Whether `content` is a file this module wrote.
fn is_managed(content: &str) -> bool {
    content
        .lines()
        .next()
        .is_some_and(|first| first.starts_with(MARKER_TOKEN))
}

/// The file's name under `<claude_dir>/rules/`.
pub const FILE_NAME: &str = "recall-echo.md";

const TEMPLATE: &str = include_str!("../templates/recall-echo.md");

/// What [`install`] did, and anything worth a warning on the way.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    /// Things the user should hear about that did not stop the install.
    pub warnings: Vec<String>,
}

/// What [`install`] did to the rules file.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Written fresh, or rewritten because it was ours and out of date.
    Written(PathBuf),
    /// Ours, and already what this root and template render to.
    Unchanged(PathBuf),
    /// Something without the marker is there — the user's file, a symlink,
    /// or not a regular file at all. Left alone; `import_line` is what it
    /// would need.
    Unmanaged { path: PathBuf, import_line: String },
    /// The user's own file, left alone, which already imports this store's
    /// MEMORY.md — nothing to advise.
    UnmanagedImports(PathBuf),
    /// The root cannot be written into an import line.
    Refused(String),
    /// Reading or writing failed.
    Failed(String),
}

/// `<root>/memory`, once `root` is known to fit in an import line.
///
/// Refuses a relative path (an import resolves against the rules file's own
/// directory, not the root), a path that is not UTF-8, and one containing
/// whitespace or control characters (an `@` import ends at the first
/// whitespace). The `Err` is the reason, for `init`'s status line.
fn memory_dir(root: &Path) -> Result<String, String> {
    if !root.is_absolute() {
        return Err(format!("pulse root {} is not absolute", root.display()));
    }
    let Some(root) = root.to_str() else {
        return Err("pulse root is not valid UTF-8".to_string());
    };
    if root.chars().any(char::is_control) {
        return Err("pulse root contains control characters".to_string());
    }
    if root.chars().any(char::is_whitespace) {
        return Err(format!(
            "pulse root {root} contains whitespace, which an @ import cannot carry"
        ));
    }
    if root.contains('\\') {
        return Err(format!(
            "pulse root {root} contains a backslash, which an @ import may read as an escape"
        ));
    }
    if root.chars().any(is_format_char) {
        return Err(
            "pulse root contains invisible formatting characters (bidi controls, zero-width \
             or tag characters), which would make the import line read differently from \
             what it loads"
                .to_string(),
        );
    }
    Ok(format!("{}/memory", root.trim_end_matches('/')))
}

/// Unicode general category Cf: characters that change how text is shown
/// without being seen themselves.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// The import line for a memory directory `memory_dir` already vetted.
fn import_for(memory_dir: &str) -> String {
    format!("@{memory_dir}/MEMORY.md")
}

/// The template with this store's paths filled in.
fn render_with(memory_dir: &str, import_line: &str) -> String {
    TEMPLATE
        .replace("{{MARKER}}", MARKER)
        .replace("{{MEMORY_IMPORT}}", import_line)
        .replace("{{MEMORY_DIR}}", memory_dir)
}

/// The one line that puts MEMORY.md into every session.
#[cfg(test)]
pub fn import_line(root: &Path) -> Result<String, String> {
    memory_dir(root).map(|dir| import_for(&dir))
}

/// Render the rules file for the store at `root` (a pulse root, canonical).
#[cfg(test)]
pub fn render(root: &Path) -> Result<String, String> {
    let memory_dir = memory_dir(root)?;
    Ok(render_with(&memory_dir, &import_for(&memory_dir)))
}

/// What is at the rules path now.
enum Existing {
    Absent,
    /// A file this module wrote, with its content.
    Managed(String),
    /// The user's: a file without the marker (with its content), a symlink,
    /// or something that is not a regular file (no content).
    Unmanaged(Option<String>),
}

fn classify(path: &Path) -> io::Result<Existing> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Absent),
        Err(e) => return Err(e),
    };
    // A symlink is the user's choice; anything but a regular file (a FIFO
    // would block the read forever) is not ours either.
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Ok(Existing::Unmanaged(None));
    }
    let content = fs::read_to_string(path)?;
    Ok(if is_managed(&content) {
        Existing::Managed(content)
    } else {
        Existing::Unmanaged(Some(content))
    })
}

/// Write or refresh `<claude_dir>/rules/recall-echo.md` for the store at `root`.
///
/// Absent: written. Ours (first line opens with the marker token): rewritten
/// only when it differs from what renders now. Anything else at that path, a
/// symlink included, belongs to the user and is never replaced. The file is
/// owner-only and its directory is created 0700.
pub fn install(claude_dir: &Path, root: &Path) -> Report {
    let mut warnings = Vec::new();
    let outcome = install_into(claude_dir, root, &mut warnings);
    if matches!(outcome, Outcome::Refused(_)) {
        warnings.extend(stale_import(claude_dir, root));
    }
    Report { outcome, warnings }
}

/// A warning when a managed rules file imports some other store's MEMORY.md
/// — for when this run did not, or could not, write it for `root`.
///
/// Only a warning: the file is still serving the store it names, and
/// rewriting or deleting it here could cut that store off.
pub(crate) fn stale_import(claude_dir: &Path, root: &Path) -> Option<String> {
    let rules_dir = claude_dir.join("rules");
    let path = rules_dir.join(FILE_NAME);
    let Ok(Existing::Managed(content)) = classify(&path) else {
        return None;
    };
    let ours = root.join("memory").join("MEMORY.md");
    if imports_file(&content, &rules_dir, &ours) {
        return None;
    }
    let theirs = import_tokens(&content).into_iter().next()?;
    Some(format!(
        "{} still imports {theirs} — Claude Code sessions load that MEMORY.md, not this \
         store's ({}). Re-run init for the store you mean, or delete the file",
        path.display(),
        ours.display()
    ))
}

fn install_into(claude_dir: &Path, root: &Path, warnings: &mut Vec<String>) -> Outcome {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let memory_dir = match memory_dir(&root) {
        Ok(dir) => dir,
        Err(why) => return Outcome::Refused(why),
    };
    if let Err(why) = vet_store(&root) {
        return Outcome::Refused(why);
    }
    let import_line = import_for(&memory_dir);
    let content = render_with(&memory_dir, &import_line);
    let rules_dir = claude_dir.join("rules");
    let path = rules_dir.join(FILE_NAME);

    match classify(&path) {
        Ok(Existing::Absent) => {}
        Ok(Existing::Managed(existing)) if existing == content => return Outcome::Unchanged(path),
        Ok(Existing::Managed(_)) => {}
        Ok(Existing::Unmanaged(Some(existing)))
            if imports_file(
                &existing,
                &rules_dir,
                &root.join("memory").join("MEMORY.md"),
            ) =>
        {
            return Outcome::UnmanagedImports(path)
        }
        Ok(Existing::Unmanaged(_)) => return Outcome::Unmanaged { path, import_line },
        Err(e) => return Outcome::Failed(format!("could not read {}: {e}", path.display())),
    }

    if let Err(e) = ensure_rules_dir(&rules_dir, warnings) {
        return Outcome::Failed(format!("could not create {}: {e}", rules_dir.display()));
    }
    match crate::claude_settings::atomic_replace(&path, &content, false) {
        Ok(()) => Outcome::Written(path),
        Err(e) => Outcome::Failed(format!("could not write {}: {e}", path.display())),
    }
}

/// Why users other than the owner could write a path with this `mode` and
/// group `gid`, or `None` when they cannot.
///
/// Write for everyone is always foreign. Group write is not when the group
/// is the owner's private group (`private_group`): on Ubuntu and Fedora a
/// 0002 umask makes everything a user creates group-writable, and that group
/// holds nobody else.
fn foreign_write(mode: u32, gid: u32, private_group: Option<u32>) -> Option<String> {
    let mode = mode & 0o7777;
    if mode & 0o002 != 0 {
        return Some(format!("is writable by every user (mode {mode:04o})"));
    }
    if mode & 0o020 != 0 && private_group != Some(gid) {
        let group = crate::serve_security::group_name(gid).map_or_else(
            || format!("gid {gid}"),
            |name| format!("group {name} (gid {gid})"),
        );
        return Some(format!(
            "is writable by {group}, which is not your private group (mode {mode:04o})"
        ));
    }
    None
}

/// The checks one path of the store must pass: owned by `uid`, and no write
/// access for anyone else. The refusal names the `chmod` that fixes it.
fn check_private(
    path: &Path,
    meta: &fs::Metadata,
    uid: u32,
    private_group: Option<u32>,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    if meta.uid() != uid {
        return Err(format!(
            "{} is owned by uid {}, not you (uid {uid}) — refusing to import from it",
            path.display(),
            meta.uid()
        ));
    }
    if let Some(reason) = foreign_write(meta.mode(), meta.gid(), private_group) {
        return Err(format!(
            "{} {reason} — refusing to import from it. Fix: chmod g-w,o-w {}",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

/// Whether what the import would load is this user's alone.
///
/// The rules file puts MEMORY.md into every Claude Code session, so anyone
/// who can write the root, `memory/` or MEMORY.md itself — or swap MEMORY.md
/// for a symlink — could put words into every session. The root and
/// `memory/` must be directories, and MEMORY.md (when it exists) a regular
/// file, each owned by this user and writable by nobody else ([`foreign_write`]).
fn vet_store(root: &Path) -> Result<(), String> {
    let uid = crate::serve_security::current_uid().map_err(|e| e.to_string())?;
    let private_group = crate::serve_security::private_group(uid);

    let memory = root.join("memory");
    for dir in [root, memory.as_path()] {
        let canonical = fs::canonicalize(dir).map_err(|e| {
            format!(
                "{} cannot be resolved ({e}) — refusing to import from it",
                dir.display()
            )
        })?;
        let meta = fs::symlink_metadata(&canonical)
            .map_err(|e| format!("cannot inspect {}: {e}", canonical.display()))?;
        if !meta.is_dir() {
            return Err(format!(
                "{} is not a directory — refusing to import from it",
                canonical.display()
            ));
        }
        check_private(&canonical, &meta, uid, private_group)?;
    }

    let memory_md = memory.join("MEMORY.md");
    let meta = match fs::symlink_metadata(&memory_md) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot inspect {}: {e}", memory_md.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink — refusing to import it",
            memory_md.display()
        ));
    }
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file — refusing to import it",
            memory_md.display()
        ));
    }
    check_private(&memory_md, &meta, uid, private_group)
}

/// The `@` import tokens in a memory file, as Claude Code would see them:
/// words opening with `@` followed by a path-ish character (`~`, `/`, `.`
/// or a word character), outside fenced code blocks and inline code spans.
/// Each is returned without its `@`.
pub(crate) fn import_tokens(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut in_fence = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Drop inline code spans: everything between paired backticks.
        let outside_code: String = line
            .split('`')
            .enumerate()
            .filter(|(i, _)| i % 2 == 0)
            .map(|(_, part)| part)
            .collect::<Vec<_>>()
            .join(" ");
        for word in outside_code.split_whitespace() {
            let Some(rest) = word.strip_prefix('@') else {
                continue;
            };
            if rest
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '~' | '/' | '.') || c.is_alphanumeric() || c == '_')
            {
                tokens.push(rest.to_string());
            }
        }
    }
    tokens
}

/// A warning when MEMORY.md carries `@` import lines of its own.
///
/// MEMORY.md is imported into every session, and Claude Code follows the
/// imports inside an imported file, so one `@` line in it — written by the
/// model, or planted — quietly loads another file into every session from
/// then on.
pub(crate) fn hidden_import_warning(memory_md: &str) -> Option<String> {
    let tokens = import_tokens(memory_md);
    if tokens.is_empty() {
        return None;
    }
    let mut shown: Vec<String> = tokens.iter().take(3).map(|t| format!("@{t}")).collect();
    if tokens.len() > 3 {
        shown.push("…".to_string());
    }
    Some(format!(
        "MEMORY.md has {} @ import line(s) ({}) — Claude Code follows them and loads those \
         files into every session. Remove them; MEMORY.md should be plain text",
        tokens.len(),
        shown.join(", ")
    ))
}

/// Whether a memory file in `dir` imports `target`: an `@` token that, with
/// `~/` expanded and a relative path resolved against `dir` (as Claude Code
/// resolves it), names the same file once both sides are canonical.
fn imports_file(content: &str, dir: &Path, target: &Path) -> bool {
    let canonical = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canonical(target);
    import_tokens(content).iter().any(|token| {
        let expanded = PathBuf::from(crate::paths::expand_tilde(token));
        let resolved = if expanded.is_absolute() {
            expanded
        } else {
            dir.join(expanded)
        };
        canonical(&resolved) == target
    })
}

/// Create `rules_dir` owner-only, or vet the one that is there.
///
/// An existing directory is the user's and is not chmod-ed, but one that
/// other users can write into, or that someone else owns, lets them plant
/// rules Claude Code will load into every session — that is worth saying.
fn ensure_rules_dir(rules_dir: &Path, warnings: &mut Vec<String>) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    match fs::metadata(rules_dir) {
        Ok(meta) => {
            let uid = crate::serve_security::current_uid().ok();
            let private_group = uid.and_then(crate::serve_security::private_group);
            if let Some(reason) = foreign_write(meta.mode(), meta.gid(), private_group) {
                warnings.push(format!(
                    "{} {reason} — anyone who can write there can add rules to every \
                     Claude Code session. Fix: chmod g-w,o-w {}",
                    rules_dir.display(),
                    rules_dir.display()
                ));
            }
            if let Some(uid) = uid {
                if meta.uid() != uid {
                    warnings.push(format!(
                        "{} is owned by uid {}, not you (uid {uid})",
                        rules_dir.display(),
                        meta.uid()
                    ));
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(rules_dir),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered() -> String {
        render(Path::new("/home/user/.claude")).unwrap()
    }

    #[test]
    fn render_imports_canonical_memory_md_once() {
        let out = rendered();
        let imports: Vec<&str> = out.lines().filter(|l| l.starts_with('@')).collect();
        assert_eq!(imports, ["@/home/user/.claude/memory/MEMORY.md"]);
        // No other import anywhere, as Claude Code would read the file.
        assert_eq!(import_tokens(&out), ["/home/user/.claude/memory/MEMORY.md"]);
    }

    #[test]
    fn render_marker_is_first_line() {
        assert_eq!(rendered().lines().next(), Some(MARKER));
    }

    #[test]
    fn render_names_4x_paths_and_the_mcp_read_path() {
        let out = rendered();
        for path in [
            "/home/user/.claude/memory/EPHEMERAL.md",
            "/home/user/.claude/memory/conversations/",
            "/home/user/.claude/memory/ARCHIVE.md",
        ] {
            assert!(out.contains(path), "missing {path}");
        }
        assert!(out.contains("recall_query"));
        assert!(!out.contains("{{"), "unrendered placeholder");
    }

    #[test]
    fn render_has_no_pre_4x_paths_or_manual_checkpoint_steps() {
        let out = rendered();
        assert!(!out.contains("~/.claude/EPHEMERAL.md"));
        assert!(!out.contains("~/.claude/conversations"));
        assert!(!out.contains("~/.claude/ARCHIVE.md"));
        assert!(!out.contains("fill in"), "checkpoint writes itself in 4.x");
    }

    #[test]
    fn render_trims_a_trailing_slash() {
        let out = render(Path::new("/srv/pulse/")).unwrap();
        assert!(out.contains("\n@/srv/pulse/memory/MEMORY.md\n"));
    }

    #[test]
    fn render_refuses_whitespace_in_root() {
        let err = render(Path::new("/home/my user/.claude")).unwrap_err();
        assert!(err.contains("whitespace"), "{err}");
    }

    #[test]
    fn render_refuses_control_chars_in_root() {
        let err = render(Path::new("/home/user\n/.claude")).unwrap_err();
        assert!(err.contains("control"), "{err}");
    }

    #[test]
    fn render_refuses_backslash_in_root() {
        let err = render(Path::new("/home/user\\x/.claude")).unwrap_err();
        assert!(err.contains("backslash"), "{err}");
    }

    #[test]
    fn render_refuses_format_chars_in_root() {
        for sneaky in ["\u{202E}", "\u{200B}", "\u{2066}", "\u{FEFF}", "\u{E0041}"] {
            let root = format!("/home/user{sneaky}/.claude");
            let err = render(Path::new(&root)).unwrap_err();
            assert!(err.contains("invisible"), "{sneaky:?}: {err}");
        }
    }

    #[test]
    fn render_accepts_ordinary_non_ascii() {
        assert!(render(Path::new("/home/josé/.claude")).is_ok());
    }

    #[test]
    fn render_refuses_relative_root() {
        let err = render(Path::new(".claude")).unwrap_err();
        assert!(err.contains("not absolute"), "{err}");
    }

    /// A Claude Code dir and two pulse roots, all in one temp dir.
    struct Dirs {
        tmp: tempfile::TempDir,
    }

    impl Dirs {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            for sub in [".claude", "root-a/memory", "root-b/memory"] {
                fs::create_dir_all(tmp.path().join(sub)).unwrap();
            }
            // Independent of the umask: the trust checks refuse group-writable.
            for sub in ["root-a", "root-a/memory", "root-b", "root-b/memory"] {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(tmp.path().join(sub), fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            Self { tmp }
        }
        fn claude(&self) -> PathBuf {
            self.tmp.path().join(".claude")
        }
        fn root(&self, name: &str) -> PathBuf {
            fs::canonicalize(self.tmp.path().join(name)).unwrap()
        }
        fn rules(&self) -> PathBuf {
            self.claude().join("rules").join(FILE_NAME)
        }
        /// Write `root`'s MEMORY.md 0644, whatever the umask.
        fn write_memory_md(&self, root: &str, content: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.root(root).join("memory/MEMORY.md");
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            path
        }
    }

    #[test]
    fn install_writes_when_absent() {
        let dirs = Dirs::new();
        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
        let written = fs::read_to_string(dirs.rules()).unwrap();
        assert_eq!(written, render(&dirs.root("root-a")).unwrap());
    }

    #[test]
    fn install_unchanged_when_managed_and_equal() {
        let dirs = Dirs::new();
        install(&dirs.claude(), &dirs.root("root-a"));
        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(outcome, Outcome::Unchanged(dirs.rules()));
    }

    /// A file an earlier release wrote, whose marker prose has since been
    /// reworded, is still ours.
    #[test]
    fn install_rewrites_a_managed_file_with_reworded_marker_prose() {
        let dirs = Dirs::new();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        fs::write(
            dirs.rules(),
            "<!-- recall-echo:managed — written by an older release -->\nold body\n",
        )
        .unwrap();

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
        assert_eq!(
            fs::read_to_string(dirs.rules()).unwrap(),
            render(&dirs.root("root-a")).unwrap()
        );
    }

    /// Same root, but someone edited below the marker: the file is ours, so
    /// it is brought back to what renders — edits belong above a deleted
    /// marker line, not inside a managed file.
    #[test]
    fn install_rewrites_a_managed_file_edited_below_the_marker() {
        let dirs = Dirs::new();
        install(&dirs.claude(), &dirs.root("root-a"));
        let edited = format!(
            "{}\n- an extra rule\n",
            fs::read_to_string(dirs.rules()).unwrap()
        );
        fs::write(dirs.rules(), &edited).unwrap();

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
        assert_eq!(
            fs::read_to_string(dirs.rules()).unwrap(),
            render(&dirs.root("root-a")).unwrap()
        );
    }

    #[test]
    fn install_rewrites_when_managed_and_root_changed() {
        let dirs = Dirs::new();
        install(&dirs.claude(), &dirs.root("root-a"));
        let outcome = install(&dirs.claude(), &dirs.root("root-b")).outcome;
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
        let written = fs::read_to_string(dirs.rules()).unwrap();
        let line = import_line(&dirs.root("root-b")).unwrap();
        assert!(written.lines().any(|l| l == line), "{written}");
        assert!(!written.contains("root-a"), "{written}");
    }

    #[test]
    fn install_leaves_unmanaged_file_byte_identical() {
        let dirs = Dirs::new();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        fs::write(dirs.rules(), "# my own rules\n").unwrap();

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(
            outcome,
            Outcome::Unmanaged {
                path: dirs.rules(),
                import_line: import_line(&dirs.root("root-a")).unwrap(),
            }
        );
        assert_eq!(
            fs::read_to_string(dirs.rules()).unwrap(),
            "# my own rules\n"
        );
    }

    #[test]
    fn install_reports_a_hand_written_file_that_already_imports_memory_md() {
        let dirs = Dirs::new();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        // Through a symlinked route to the same root: canonicalized, it matches.
        let alias = dirs.tmp.path().join("alias");
        std::os::unix::fs::symlink(dirs.root("root-a"), &alias).unwrap();
        fs::write(
            dirs.rules(),
            format!("# mine\n\n@{}/memory/MEMORY.md\n", alias.display()),
        )
        .unwrap();
        dirs.write_memory_md("root-a", "# Memory\n");

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(outcome, Outcome::UnmanagedImports(dirs.rules()));
    }

    #[test]
    fn an_import_of_another_store_or_inside_code_still_gets_the_advice() {
        let dirs = Dirs::new();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        let ours = dirs.write_memory_md("root-a", "# Memory\n");
        fs::write(
            dirs.rules(),
            format!(
                "@{}/memory/MEMORY.md\n```\n@{}\n```\nsee `@{}`\n",
                dirs.root("root-b").display(),
                ours.display(),
                ours.display()
            ),
        )
        .unwrap();

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert!(matches!(outcome, Outcome::Unmanaged { .. }), "{outcome:?}");
    }

    #[test]
    fn hidden_import_warning_names_imports_outside_code_only() {
        assert_eq!(
            hidden_import_warning("# Memory\n- a fact, mail me at d@x.io\n"),
            None
        );
        assert_eq!(
            hidden_import_warning("see `@~/notes.md`\n```\n@/x.md\n```\n"),
            None
        );
        let warning = hidden_import_warning("- fact\n@~/.ssh/notes.md\n").expect("a warning");
        assert!(warning.contains("@~/.ssh/notes.md"), "{warning}");
        assert!(warning.contains("every session"), "{warning}");
    }

    #[test]
    fn the_template_tells_the_model_to_keep_memory_md_plain() {
        let out = rendered();
        assert!(out.contains("Never put secrets"), "{out}");
        assert!(out.contains("never add `@` lines"), "{out}");
    }

    #[test]
    fn import_tokens_skip_fences_spans_and_email_like_words() {
        let content = "@/a/b.md and @~/c.md\n```\n@/in/fence.md\n```\n`@/in/span.md` x@y.z @ alone\n@./rel.md\n";
        assert_eq!(import_tokens(content), ["/a/b.md", "~/c.md", "./rel.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn install_leaves_a_symlink_alone() {
        let dirs = Dirs::new();
        let target = dirs.tmp.path().join("dotfiles-rules.md");
        // Even a target carrying the marker: the link is the user's choice.
        fs::write(&target, format!("{MARKER}\nold\n")).unwrap();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        std::os::unix::fs::symlink(&target, dirs.rules()).unwrap();

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert!(matches!(outcome, Outcome::Unmanaged { .. }), "{outcome:?}");
        assert!(fs::symlink_metadata(dirs.rules())
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            format!("{MARKER}\nold\n")
        );
    }

    #[test]
    fn install_writes_owner_only_into_a_0700_rules_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dirs = Dirs::new();
        install(&dirs.claude(), &dirs.root("root-a"));
        let file_mode = fs::metadata(dirs.rules()).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(dirs.claude().join("rules"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn install_warns_about_a_rules_dir_others_can_write() {
        use std::os::unix::fs::PermissionsExt;
        let dirs = Dirs::new();
        let rules_dir = dirs.claude().join("rules");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::set_permissions(&rules_dir, fs::Permissions::from_mode(0o777)).unwrap();

        let report = install(&dirs.claude(), &dirs.root("root-a"));
        assert_eq!(report.outcome, Outcome::Written(dirs.rules()));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("writable by every user")),
            "{:?}",
            report.warnings
        );
        // The user's directory is reported, not chmod-ed.
        let mode = fs::metadata(&rules_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o777);
    }

    #[test]
    fn install_never_reads_a_fifo_at_the_rules_path() {
        let dirs = Dirs::new();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        let fifo = std::ffi::CString::new(dirs.rules().to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert!(matches!(outcome, Outcome::Unmanaged { .. }), "{outcome:?}");
    }

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn assert_refused_without_writing(dirs: &Dirs, root: &Path, needle: &str) {
        let outcome = install(&dirs.claude(), root).outcome;
        match outcome {
            Outcome::Refused(why) => assert!(why.contains(needle), "{why}"),
            other => panic!("expected Refused, got {other:?}"),
        }
        assert!(!dirs.rules().exists());
    }

    #[test]
    fn install_refuses_a_root_others_can_write() {
        let dirs = Dirs::new();
        chmod(&dirs.root("root-a"), 0o777);
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "writable by every user");
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "Fix: chmod g-w,o-w");
    }

    #[test]
    fn install_refuses_a_memory_dir_every_user_can_write() {
        let dirs = Dirs::new();
        chmod(&dirs.root("root-a").join("memory"), 0o777);
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "writable by every user");
    }

    #[test]
    fn group_write_is_foreign_unless_the_group_is_the_users_private_one() {
        assert_eq!(foreign_write(0o755, 1000, Some(1000)), None);
        assert_eq!(foreign_write(0o775, 1000, Some(1000)), None);
        assert_eq!(
            foreign_write(0o40775, 1000, Some(1000)),
            None,
            "file-type bits ignored"
        );
        let shared = foreign_write(0o775, 1000, Some(1001)).expect("a shared group");
        assert!(shared.contains("not your private group"), "{shared}");
        assert!(
            foreign_write(0o775, 1000, None).is_some(),
            "no private group known"
        );
        let everyone = foreign_write(0o757, 1000, Some(1000)).expect("others-write");
        assert!(everyone.contains("every user"), "{everyone}");
    }

    /// Ubuntu and Fedora: umask 0002 under a user-private group makes the
    /// whole store group-writable, and that is fine.
    #[test]
    fn install_accepts_a_store_group_writable_by_the_users_private_group() {
        use std::os::unix::fs::MetadataExt;
        let dirs = Dirs::new();
        let uid = crate::serve_security::current_uid().unwrap();
        let root = dirs.root("root-a");
        let Some(private) = crate::serve_security::private_group(uid) else {
            eprintln!("skipped: this account has no private group");
            return;
        };
        if fs::metadata(&root).unwrap().gid() != private {
            eprintln!("skipped: test files are not created in the private group");
            return;
        }
        let memory_md = dirs.write_memory_md("root-a", "# Memory\n");
        chmod(&root, 0o775);
        chmod(&root.join("memory"), 0o775);
        chmod(&memory_md, 0o664);
        let outcome = install(&dirs.claude(), &root).outcome;
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
    }

    #[test]
    fn install_refuses_a_store_group_writable_by_a_shared_group() {
        let dirs = Dirs::new();
        let memory = dirs.root("root-a").join("memory");
        // Needs the right to hand the directory to another group: root can.
        if std::os::unix::fs::chown(&memory, None, Some(65534)).is_err() {
            eprintln!("skipped: cannot chgrp to a shared group here");
            return;
        }
        chmod(&memory, 0o775);
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "not your private group");
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "Fix: chmod g-w,o-w");
    }

    #[test]
    fn install_refuses_a_missing_memory_dir() {
        let dirs = Dirs::new();
        fs::remove_dir(dirs.root("root-a").join("memory")).unwrap();
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "cannot be resolved");
    }

    #[test]
    fn install_refuses_a_symlinked_memory_md() {
        let dirs = Dirs::new();
        let elsewhere = dirs.tmp.path().join("elsewhere.md");
        fs::write(&elsewhere, "planted\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dirs.root("root-a").join("memory/MEMORY.md"))
            .unwrap();
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "is a symlink");
    }

    #[test]
    fn install_refuses_a_memory_md_others_can_write() {
        let dirs = Dirs::new();
        let memory_md = dirs.write_memory_md("root-a", "# Memory\n");
        chmod(&memory_md, 0o666);
        assert_refused_without_writing(&dirs, &dirs.root("root-a"), "writable by every user");
    }

    #[test]
    fn install_accepts_an_ordinary_memory_md() {
        let dirs = Dirs::new();
        let memory_md = dirs.write_memory_md("root-a", "# Memory\n");
        chmod(&memory_md, 0o644);
        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
    }

    #[test]
    fn install_reports_failure_when_rules_dir_unwritable() {
        let dirs = Dirs::new();
        // A file where the directory should be: fails even when run as root,
        // which permission bits would not.
        fs::write(dirs.claude().join("rules"), "not a directory").unwrap();
        let outcome = install(&dirs.claude(), &dirs.root("root-a")).outcome;
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    }

    #[test]
    fn a_refused_install_names_the_store_the_managed_file_still_imports() {
        let dirs = Dirs::new();
        install(&dirs.claude(), &dirs.root("root-a"));
        let spaced = dirs.tmp.path().join("my root");
        fs::create_dir_all(spaced.join("memory")).unwrap();

        let report = install(&dirs.claude(), &spaced);
        assert!(matches!(report.outcome, Outcome::Refused(_)), "{report:?}");
        let warning = report
            .warnings
            .iter()
            .find(|w| w.contains("still imports"))
            .expect("a stale-import warning");
        assert!(warning.contains("root-a"), "{warning}");
        // Left as it was: still serving root-a.
        assert_eq!(
            fs::read_to_string(dirs.rules()).unwrap(),
            render(&dirs.root("root-a")).unwrap()
        );
    }

    #[test]
    fn stale_import_is_silent_for_the_same_store_or_no_managed_file() {
        let dirs = Dirs::new();
        assert_eq!(stale_import(&dirs.claude(), &dirs.root("root-a")), None);
        install(&dirs.claude(), &dirs.root("root-a"));
        assert_eq!(stale_import(&dirs.claude(), &dirs.root("root-a")), None);
        assert!(stale_import(&dirs.claude(), &dirs.root("root-b")).is_some());
    }

    #[test]
    fn install_refuses_a_root_with_whitespace_and_writes_nothing() {
        let dirs = Dirs::new();
        let spaced = dirs.tmp.path().join("my root");
        fs::create_dir_all(&spaced).unwrap();
        let outcome = install(&dirs.claude(), &spaced).outcome;
        assert!(matches!(outcome, Outcome::Refused(_)), "{outcome:?}");
        assert!(!dirs.claude().join("rules").exists());
    }
}
