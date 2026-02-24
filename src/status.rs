use std::fs;
use std::path::Path;

use crate::archive;
use crate::paths;

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

const CHECKPOINT_COMMAND: &str = "recall-echo checkpoint --trigger precompact";
const PROMOTE_COMMAND: &str = "recall-echo promote";

fn run_with_base(base: &Path) -> Result<(), String> {
    if !base.exists() {
        return Err("~/.claude directory not found. Run `recall-echo init` first.".to_string());
    }

    println!("\n{BOLD}recall-echo{RESET} — memory system status\n");

    let memory_path = base.join("memory").join("MEMORY.md");
    let ephemeral_path = base.join("EPHEMERAL.md");
    let memories_dir = base.join("memories");
    let protocol_path = base.join("rules").join("recall-echo.md");
    let settings_path = base.join("settings.json");

    // MEMORY.md
    if memory_path.exists() {
        let content = fs::read_to_string(&memory_path).unwrap_or_default();
        let lines = content.lines().count();
        let pct = (lines * 100) / 200;
        let warn = if lines >= 180 {
            format!(" {YELLOW}⚠ approaching limit{RESET}")
        } else {
            String::new()
        };
        println!("  MEMORY.md:    {lines}/200 lines ({pct}%){warn}");
    } else {
        println!("  MEMORY.md:    {YELLOW}not found{RESET}");
    }

    // EPHEMERAL.md
    if ephemeral_path.exists() {
        let content = fs::read_to_string(&ephemeral_path).unwrap_or_default();
        if content.trim().is_empty() {
            println!("  EPHEMERAL.md: empty (clean)");
        } else {
            println!("  EPHEMERAL.md: has content (pending promotion)");
        }
    } else {
        println!("  EPHEMERAL.md: {YELLOW}not found{RESET}");
    }

    // Archive logs
    if memories_dir.exists() {
        let highest = archive::highest_log_number(&memories_dir);
        let count = count_archive_logs(&memories_dir);
        if count == 0 {
            println!("  Archive logs: none yet");
        } else {
            let latest_path = memories_dir.join(format!("archive-log-{highest:03}.md"));
            let latest_date = if latest_path.exists() {
                let content = fs::read_to_string(&latest_path).unwrap_or_default();
                crate::frontmatter::parse(&content)
                    .map(|fm| fm.date.split('T').next().unwrap_or(&fm.date).to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                "unknown".to_string()
            };
            println!("  Archive logs: {count} logs, latest: {latest_date}");
        }
    } else {
        println!("  Archive logs: {YELLOW}directory not found{RESET}");
    }

    // Protocol
    if protocol_path.exists() {
        println!("  Protocol:     {GREEN}installed{RESET}");
    } else {
        println!("  Protocol:     {YELLOW}not found{RESET} — run recall-echo init");
    }

    // Hooks
    if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        let precompact_ok = content.contains(CHECKPOINT_COMMAND);
        let session_end_ok = content.contains(PROMOTE_COMMAND);
        let has_legacy = content.contains("RECALL-ECHO") && !precompact_ok;

        if has_legacy {
            println!(
                "  Hooks:        {YELLOW}echo (legacy){RESET} ⚠ run recall-echo init to upgrade"
            );
        } else {
            let pc = if precompact_ok {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{YELLOW}⚠{RESET}")
            };
            let se = if session_end_ok {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{YELLOW}⚠{RESET}")
            };
            println!("  Hooks:        PreCompact {pc}  SessionEnd {se}");
        }
    } else {
        println!("  Hooks:        {YELLOW}no settings.json{RESET} — run recall-echo init");
    }

    // Issues summary
    let mut issues = Vec::new();
    if !protocol_path.exists() {
        issues.push("protocol not installed");
    }
    if !memory_path.exists() {
        issues.push("MEMORY.md missing");
    }
    if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        if content.contains("RECALL-ECHO") && !content.contains(CHECKPOINT_COMMAND) {
            issues.push("legacy hook needs upgrade");
        }
        if !content.contains(PROMOTE_COMMAND) {
            issues.push("SessionEnd hook missing — run recall-echo init");
        }
    }

    println!();
    if issues.is_empty() {
        println!("  No issues detected.");
    } else {
        for issue in &issues {
            println!("  {YELLOW}⚠{RESET} {issue}");
        }
    }
    println!();

    Ok(())
}

pub fn run() -> Result<(), String> {
    let claude = paths::claude_dir()?;
    run_with_base(&claude)
}

fn count_archive_logs(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("archive-log-") && name.ends_with(".md")
                })
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn count_logs_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(count_archive_logs(tmp.path()), 0);
    }

    #[test]
    fn count_logs_with_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("archive-log-001.md"), "").unwrap();
        fs::write(tmp.path().join("archive-log-002.md"), "").unwrap();
        fs::write(tmp.path().join("notes.md"), "").unwrap();
        assert_eq!(count_archive_logs(tmp.path()), 2);
    }

    #[test]
    fn status_runs_on_valid_env() {
        let tmp = tempfile::tempdir().unwrap();
        let memories = tmp.path().join("memories");
        let memory_dir = tmp.path().join("memory");
        let rules_dir = tmp.path().join("rules");
        fs::create_dir_all(&memories).unwrap();
        fs::create_dir_all(&memory_dir).unwrap();
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(memory_dir.join("MEMORY.md"), "# Memory\n\nSome content\n").unwrap();
        fs::write(tmp.path().join("EPHEMERAL.md"), "last session").unwrap();
        fs::write(tmp.path().join("ARCHIVE.md"), "# Archive\n").unwrap();
        fs::write(rules_dir.join("recall-echo.md"), "protocol").unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"hooks":{"PreCompact":[{"hooks":[{"type":"command","command":"recall-echo checkpoint --trigger precompact"}]}],"SessionEnd":[{"hooks":[{"type":"command","command":"recall-echo promote"}]}]}}"#,
        )
        .unwrap();

        let result = run_with_base(tmp.path());
        assert!(result.is_ok());
    }

    #[test]
    fn status_warns_on_legacy_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let memories = tmp.path().join("memories");
        let memory_dir = tmp.path().join("memory");
        fs::create_dir_all(&memories).unwrap();
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(memory_dir.join("MEMORY.md"), "# Memory\n\nSome content\n").unwrap();
        fs::write(tmp.path().join("EPHEMERAL.md"), "").unwrap();
        fs::write(
            tmp.path().join("settings.json"),
            r#"{"hooks":{"PreCompact":[{"hooks":[{"type":"command","command":"echo 'RECALL-ECHO: test'"}]}]}}"#,
        )
        .unwrap();

        // Should succeed (just prints warnings)
        let result = run_with_base(tmp.path());
        assert!(result.is_ok());
    }
}
