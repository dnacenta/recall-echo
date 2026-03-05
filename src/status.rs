use std::fs;
use std::path::Path;

use crate::config;
use crate::ephemeral;
use crate::paths;

const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

pub fn run() -> Result<(), String> {
    run_with_base(&paths::claude_dir()?)
}

pub fn run_with_base(base: &Path) -> Result<(), String> {
    if !base.exists() {
        return Err("~/.claude directory not found. Run `recall-echo init` first.".to_string());
    }

    let mut issues: Vec<String> = Vec::new();

    // Header
    let overall = if base.join("conversations").exists()
        && base.join("EPHEMERAL.md").exists()
        && base.join("rules/recall-echo.md").exists()
    {
        format!("{GREEN}healthy{RESET}")
    } else {
        issues.push("Run `recall-echo init` to complete setup".to_string());
        format!("{YELLOW}incomplete{RESET}")
    };

    eprintln!("\n{BOLD}recall-echo{RESET} — {overall}\n");

    // MEMORY.md
    let memory_path = base.join("memory/MEMORY.md");
    if memory_path.exists() {
        let lines = fs::read_to_string(&memory_path)
            .unwrap_or_default()
            .lines()
            .count();
        let pct = (lines as f32 / 200.0 * 100.0) as u32;
        let bar = progress_bar(pct, 4);
        let color = if pct > 90 {
            RED
        } else if pct > 70 {
            YELLOW
        } else {
            GREEN
        };
        eprintln!("  MEMORY.md       {color}{lines}/200 lines ({pct}%){RESET}  {bar}");
        if pct > 70 {
            issues.push(format!("MEMORY.md approaching limit ({pct}%)"));
        }
    } else {
        eprintln!("  MEMORY.md       {DIM}not found{RESET}");
        issues.push("MEMORY.md not found".to_string());
    }

    // EPHEMERAL.md
    let cfg = config::load(base);
    let max_entries = cfg.ephemeral.max_entries;
    let ephemeral_path = base.join("EPHEMERAL.md");
    if ephemeral_path.exists() {
        let count = ephemeral::count_entries(&ephemeral_path).unwrap_or(0);
        eprintln!("  EPHEMERAL       {count}/{max_entries} sessions");
    } else {
        eprintln!("  EPHEMERAL       {DIM}not found{RESET}");
    }

    // Archives
    let conversations_dir = base.join("conversations");
    if conversations_dir.exists() {
        let (count, total_bytes) = count_conversations(&conversations_dir);
        let size_str = format_bytes(total_bytes);
        eprintln!("  Archives        {count} conversations ({size_str})");

        if count > 0 {
            let (oldest, newest) = find_date_range(&conversations_dir);
            if let Some(newest) = newest {
                eprintln!("  Last archived   {newest}");
            }
            if let Some(oldest) = oldest {
                eprintln!("  Oldest archive  {oldest}");
            }
        }
    } else {
        eprintln!("  Archives        {DIM}not initialized{RESET}");
    }

    // Protocol
    let protocol_path = base.join("rules/recall-echo.md");
    if protocol_path.exists() {
        eprintln!("  Protocol        {GREEN}installed{RESET}");
    } else {
        eprintln!("  Protocol        {RED}missing{RESET}");
        issues.push("Protocol file missing — run `recall-echo init`".to_string());
    }

    // Hooks
    let settings_path = base.join("settings.json");
    if settings_path.exists() {
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap_or_default())
                .unwrap_or(serde_json::json!({}));

        let has_session_end = has_hook(&settings, "SessionEnd", "recall-echo archive-session");
        let has_precompact = has_hook(&settings, "PreCompact", "recall-echo checkpoint");
        let has_consume = has_hook(&settings, "PreToolUse", "recall-echo consume");
        let has_legacy = has_hook(&settings, "SessionEnd", "recall-echo promote");

        if has_legacy {
            eprintln!(
                "  Hooks           {YELLOW}legacy promote hook{RESET} — run recall-echo init to upgrade"
            );
            issues.push("Legacy promote hook — run `recall-echo init` to migrate".to_string());
        } else {
            let se = if has_session_end {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{RED}✗{RESET}")
            };
            let pc = if has_precompact {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{YELLOW}⚠{RESET}")
            };
            let co = if has_consume {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{YELLOW}⚠{RESET}")
            };
            eprintln!("  Hooks           SessionEnd {se}  PreCompact {pc}  PreToolUse {co}");
        }

        if !has_session_end && !has_legacy {
            issues.push("SessionEnd hook missing — run `recall-echo init`".to_string());
        }
    } else {
        eprintln!("  Hooks           {DIM}no settings.json{RESET}");
    }

    // Issues
    eprintln!();
    if issues.is_empty() {
        eprintln!("  {GREEN}No issues detected.{RESET}");
    } else {
        for issue in &issues {
            eprintln!("  {YELLOW}!{RESET} {issue}");
        }
    }
    eprintln!();

    Ok(())
}

fn has_hook(settings: &serde_json::Value, event: &str, needle: &str) -> bool {
    if let Some(hooks) = settings.get("hooks") {
        if let Some(event_hooks) = hooks.get(event) {
            let json = serde_json::to_string(event_hooks).unwrap_or_default();
            return json.contains(needle);
        }
    }
    false
}

fn count_conversations(dir: &Path) -> (usize, u64) {
    let mut count = 0;
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("conversation-") && name.ends_with(".md") {
                count += 1;
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (count, total)
}

fn find_date_range(dir: &Path) -> (Option<String>, Option<String>) {
    let mut dates: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("conversation-") && name.ends_with(".md") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    for line in content.lines().take(10) {
                        if let Some(date) = line.strip_prefix("date: ") {
                            let d = date.trim().trim_matches('"');
                            if let Some(day) = d.split('T').next() {
                                dates.push(day.to_string());
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    dates.sort();
    let oldest = dates.first().cloned();
    let newest = dates.last().cloned();
    (oldest, newest)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn progress_bar(pct: u32, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_on_initialized_env() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        crate::init::run_with_base(base).unwrap();
        assert!(run_with_base(base).is_ok());
    }

    #[test]
    fn status_on_missing_dir() {
        assert!(run_with_base(Path::new("/nonexistent")).is_err());
    }

    #[test]
    fn format_bytes_ranges() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn progress_bar_display() {
        assert_eq!(progress_bar(0, 4), "░░░░");
        assert_eq!(progress_bar(50, 4), "██░░");
        assert_eq!(progress_bar(100, 4), "████");
    }

    #[test]
    fn count_conversations_basic() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("conversation-001.md"), "hello").unwrap();
        fs::write(tmp.path().join("conversation-002.md"), "world").unwrap();
        fs::write(tmp.path().join("notes.md"), "ignore").unwrap();
        let (count, _) = count_conversations(tmp.path());
        assert_eq!(count, 2);
    }
}
