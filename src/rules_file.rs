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

use std::path::Path;

/// First line of every file this module writes. A rules file without it is
/// the user's own and is never touched.
pub const MARKER: &str = "<!-- recall-echo:managed — rewritten by `recall-echo init`; delete this line to keep your own edits -->";

const TEMPLATE: &str = include_str!("../templates/recall-echo.md");

/// Render the rules file for the store at `root` (a pulse root, canonical).
///
/// Refuses a root the import line cannot carry: a relative path (an import
/// resolves against the rules file's own directory, not the root), a path
/// that is not UTF-8, or one containing whitespace or control characters
/// (an `@` import ends at the first whitespace). The `Err` is the reason,
/// for `init`'s status line.
pub fn render(root: &Path) -> Result<String, String> {
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

    let memory_dir = format!("{}/memory", root.trim_end_matches('/'));
    Ok(TEMPLATE
        .replace("{{MARKER}}", MARKER)
        .replace("{{MEMORY_IMPORT}}", &format!("@{memory_dir}/MEMORY.md"))
        .replace("{{MEMORY_DIR}}", &memory_dir))
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
    fn render_refuses_relative_root() {
        let err = render(Path::new(".claude")).unwrap_err();
        assert!(err.contains("not absolute"), "{err}");
    }
}
