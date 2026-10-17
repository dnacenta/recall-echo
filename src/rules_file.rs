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

/// What [`install`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Written fresh, or rewritten because it was ours and out of date.
    Written(PathBuf),
    /// Ours, and already what this root and template render to.
    Unchanged(PathBuf),
    /// Something without the marker is there — the user's file, or a
    /// symlink to one. Left alone; `import_line` is what it would need.
    Unmanaged { path: PathBuf, import_line: String },
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

/// The one line that puts MEMORY.md into every session.
pub fn import_line(root: &Path) -> Result<String, String> {
    memory_dir(root).map(|dir| format!("@{dir}/MEMORY.md"))
}

/// Render the rules file for the store at `root` (a pulse root, canonical).
pub fn render(root: &Path) -> Result<String, String> {
    let memory_dir = memory_dir(root)?;
    Ok(TEMPLATE
        .replace("{{MARKER}}", MARKER)
        .replace("{{MEMORY_IMPORT}}", &format!("@{memory_dir}/MEMORY.md"))
        .replace("{{MEMORY_DIR}}", &memory_dir))
}

/// Write or refresh `<claude_dir>/rules/recall-echo.md` for the store at `root`.
///
/// Absent: written. Ours (first line opens with the marker token): rewritten only when it
/// differs from what renders now. Anything else at that path, a symlink
/// included, belongs to the user and is never replaced.
pub fn install(claude_dir: &Path, root: &Path) -> Outcome {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let content = match render(&root) {
        Ok(content) => content,
        Err(why) => return Outcome::Refused(why),
    };
    let rules_dir = claude_dir.join("rules");
    let path = rules_dir.join(FILE_NAME);

    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            let existing = if meta.file_type().is_symlink() {
                None
            } else {
                match fs::read_to_string(&path) {
                    Ok(existing) => Some(existing),
                    Err(e) => {
                        return Outcome::Failed(format!("could not read {}: {e}", path.display()))
                    }
                }
            };
            match existing {
                Some(existing) if is_managed(&existing) => {
                    if existing == content {
                        return Outcome::Unchanged(path);
                    }
                }
                _ => {
                    let import_line = import_line(&root).unwrap_or_default();
                    return Outcome::Unmanaged { path, import_line };
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Outcome::Failed(format!("could not inspect {}: {e}", path.display())),
    }

    if let Err(e) = fs::create_dir_all(&rules_dir) {
        return Outcome::Failed(format!("could not create {}: {e}", rules_dir.display()));
    }
    match write_atomically(&path, &content) {
        Ok(()) => Outcome::Written(path),
        Err(e) => Outcome::Failed(format!("could not write {}: {e}", path.display())),
    }
}

/// Replace `path` without a window where Claude Code could read it half-written.
fn write_atomically(path: &Path, content: &str) -> io::Result<()> {
    // Pid-suffixed so concurrent inits cannot rename each other's temp.
    let tmp = path.with_extension(format!("md.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
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
        // No other line carries an import either.
        assert_eq!(out.matches('@').count(), 1, "{out}");
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
            for sub in [".claude", "root-a", "root-b"] {
                fs::create_dir_all(tmp.path().join(sub)).unwrap();
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
    }

    #[test]
    fn install_writes_when_absent() {
        let dirs = Dirs::new();
        let outcome = install(&dirs.claude(), &dirs.root("root-a"));
        assert_eq!(outcome, Outcome::Written(dirs.rules()));
        let written = fs::read_to_string(dirs.rules()).unwrap();
        assert_eq!(written, render(&dirs.root("root-a")).unwrap());
    }

    #[test]
    fn install_unchanged_when_managed_and_equal() {
        let dirs = Dirs::new();
        install(&dirs.claude(), &dirs.root("root-a"));
        let outcome = install(&dirs.claude(), &dirs.root("root-a"));
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

        let outcome = install(&dirs.claude(), &dirs.root("root-a"));
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
        let outcome = install(&dirs.claude(), &dirs.root("root-b"));
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

        let outcome = install(&dirs.claude(), &dirs.root("root-a"));
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

    #[cfg(unix)]
    #[test]
    fn install_leaves_a_symlink_alone() {
        let dirs = Dirs::new();
        let target = dirs.tmp.path().join("dotfiles-rules.md");
        // Even a target carrying the marker: the link is the user's choice.
        fs::write(&target, format!("{MARKER}\nold\n")).unwrap();
        fs::create_dir_all(dirs.claude().join("rules")).unwrap();
        std::os::unix::fs::symlink(&target, dirs.rules()).unwrap();

        let outcome = install(&dirs.claude(), &dirs.root("root-a"));
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
    fn install_reports_failure_when_rules_dir_unwritable() {
        let dirs = Dirs::new();
        // A file where the directory should be: fails even when run as root,
        // which permission bits would not.
        fs::write(dirs.claude().join("rules"), "not a directory").unwrap();
        let outcome = install(&dirs.claude(), &dirs.root("root-a"));
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    }

    #[test]
    fn install_refuses_a_root_with_whitespace_and_writes_nothing() {
        let dirs = Dirs::new();
        let spaced = dirs.tmp.path().join("my root");
        fs::create_dir_all(&spaced).unwrap();
        let outcome = install(&dirs.claude(), &spaced);
        assert!(matches!(outcome, Outcome::Refused(_)), "{outcome:?}");
        assert!(!dirs.claude().join("rules").exists());
    }
}
