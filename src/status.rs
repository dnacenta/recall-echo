//! Memory system health check.
//!
//! Quick status overview of the four-layer memory system.
//! For the full ASCII art dashboard, use the `dashboard` module.

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
    run_with_base(&paths::entity_root()?)
}

pub fn run_with_base(entity_root: &Path) -> Result<(), String> {
    let memory = entity_root.join("memory");
    if !memory.exists() {
        return Err("memory/ directory not found. Run `recall-echo init` first.".to_string());
    }

    let mut issues: Vec<String> = Vec::new();

    // Header
    let overall = if memory.join("conversations").exists()
        && memory.join("EPHEMERAL.md").exists()
        && memory.join("MEMORY.md").exists()
    {
        format!("{GREEN}healthy{RESET}")
    } else {
        issues.push("Run `recall-echo init` to complete setup".to_string());
        format!("{YELLOW}incomplete{RESET}")
    };

    eprintln!("\n{BOLD}recall-echo{RESET} — {overall}\n");

    // MEMORY.md
    let memory_path = memory.join("MEMORY.md");
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
    let cfg = config::load(&memory);
    let max_entries = cfg.ephemeral.max_entries;
    let ephemeral_path = memory.join("EPHEMERAL.md");
    if ephemeral_path.exists() {
        let count = ephemeral::count_entries(&ephemeral_path).unwrap_or(0);
        eprintln!("  EPHEMERAL       {count}/{max_entries} sessions");
    } else {
        eprintln!("  EPHEMERAL       {DIM}not found{RESET}");
    }

    // Archives
    let conversations_dir = memory.join("conversations");
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
        let root = tmp.path();
        crate::init::run(root).unwrap();
        assert!(run_with_base(root).is_ok());
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
