use std::fs;

use crate::RecallEcho;

// ── ANSI colors ─────────────────────────────────────────────────────────

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

const LOGO: &str = r#"
╦═╗╔═╗╔═╗╔═╗╦  ╦
╠╦╝║╣ ║  ╠═╣║  ║
╩╚═╚═╝╚═╝╩ ╩╩═╝╩═╝"#;

const SEPARATOR: &str = "  ──────────────────────────────────────────────────────────────";

// ── Public data types ───────────────────────────────────────────────────

/// Memory health assessment.
pub enum HealthLevel {
    Healthy,
    Watch,
    Alert,
}

pub struct HealthAssessment {
    pub level: HealthLevel,
    pub warnings: Vec<String>,
}

impl HealthAssessment {
    pub fn display(&self) -> String {
        match self.level {
            HealthLevel::Healthy => format!("{GREEN}HEALTHY{RESET}"),
            HealthLevel::Watch => format!("{YELLOW}WATCH{RESET}"),
            HealthLevel::Alert => format!("{RED}ALERT{RESET}"),
        }
    }
}

/// Stats about MEMORY.md.
pub struct MemoryStats {
    pub line_count: usize,
    pub sections: Vec<(String, usize)>,
    pub modified: Option<std::time::SystemTime>,
}

impl MemoryStats {
    pub fn collect(recall: &RecallEcho) -> Self {
        let memory_path = recall.memory_file();
        if !memory_path.exists() {
            return Self {
                line_count: 0,
                sections: Vec::new(),
                modified: None,
            };
        }

        let content = fs::read_to_string(&memory_path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let line_count = lines.len();

        let sections: Vec<(String, usize)> = find_sections(&lines)
            .into_iter()
            .map(|(name, _, size)| (name, size))
            .collect();

        let modified = fs::metadata(&memory_path)
            .ok()
            .and_then(|m| m.modified().ok());

        Self {
            line_count,
            sections,
            modified,
        }
    }

    pub fn freshness_display(&self) -> String {
        match self.modified {
            Some(time) => format_age(time),
            None => "unknown".to_string(),
        }
    }
}

/// A parsed ephemeral session entry for display.
pub struct EphemeralEntry {
    pub log_num: String,
    pub age_display: String,
    pub duration: String,
    pub message_count: String,
    pub topics: String,
}

/// Stats about the conversation archive.
pub struct ArchiveStats {
    pub count: usize,
    pub total_bytes: u64,
    pub newest_modified: Option<std::time::SystemTime>,
}

impl ArchiveStats {
    pub fn collect(recall: &RecallEcho) -> Self {
        let conv_dir = recall.conversations_dir();
        if !conv_dir.exists() {
            return Self {
                count: 0,
                total_bytes: 0,
                newest_modified: None,
            };
        }

        let entries: Vec<_> = fs::read_dir(&conv_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("conversation-"))
            .collect();

        let count = entries.len();
        let mut total_bytes = 0u64;
        let mut newest: Option<std::time::SystemTime> = None;

        for entry in &entries {
            if let Ok(meta) = entry.metadata() {
                total_bytes += meta.len();
                if let Ok(modified) = meta.modified() {
                    newest = Some(match newest {
                        Some(prev) if modified > prev => modified,
                        Some(prev) => prev,
                        None => modified,
                    });
                }
            }
        }

        Self {
            count,
            total_bytes,
            newest_modified: newest,
        }
    }

    pub fn freshness_display(&self) -> String {
        match self.newest_modified {
            Some(time) => format_age(time),
            None => "no archives".to_string(),
        }
    }
}

// ── Dashboard rendering ─────────────────────────────────────────────────

/// Render the neofetch-style memory dashboard to stderr.
pub fn render(recall: &RecallEcho, entity_name: &str, version: &str, max_memory_lines: usize) {
    let memory_stats = MemoryStats::collect(recall);
    let ephemeral_entries = parse_ephemeral_entries(recall);
    let archive_stats = ArchiveStats::collect(recall);
    let health = assess_health(&memory_stats, &archive_stats, max_memory_lines);

    // Logo + metadata side by side
    let logo_lines: Vec<&str> = LOGO.lines().skip(1).collect();
    let meta_lines = [
        format!("entity    {CYAN}{entity_name}{RESET}"),
        format!(
            "memory    {}/{}  {}  {}",
            memory_stats.line_count,
            max_memory_lines,
            memory_bar(memory_stats.line_count, max_memory_lines),
            memory_status_word(memory_stats.line_count, max_memory_lines),
        ),
        format!("sessions  {}/5 entries", ephemeral_entries.len()),
        format!(
            "archive   {} conversations ({})",
            archive_stats.count,
            format_bytes(archive_stats.total_bytes),
        ),
        format!("freshness {}", archive_stats.freshness_display()),
    ];

    eprintln!();
    let logo_width = 26;
    for (i, logo_line) in logo_lines.iter().enumerate() {
        if i < meta_lines.len() {
            eprintln!(
                "  {GREEN}{:<width$}{RESET}  {}",
                logo_line,
                meta_lines[i],
                width = logo_width,
            );
        } else {
            eprintln!("  {GREEN}{}{RESET}", logo_line);
        }
    }

    // Print remaining metadata if logo ran out of lines
    for meta_line in meta_lines.iter().skip(logo_lines.len()) {
        eprintln!("  {:<width$}  {}", "", meta_line, width = logo_width);
    }

    eprintln!("  v{version}");
    eprintln!("{SEPARATOR}");

    // Memory Health
    eprintln!();
    eprintln!(
        "  {BOLD}Memory Health{RESET}                   {}",
        health.display()
    );
    eprintln!();

    eprintln!(
        "  {:<14} {}  {:<8} {}",
        "curated",
        memory_bar(memory_stats.line_count, max_memory_lines),
        format!("{}/{}", memory_stats.line_count, max_memory_lines),
        memory_status_word(memory_stats.line_count, max_memory_lines),
    );
    eprintln!(
        "  {:<14} {}  {:<8} ok",
        "ephemeral",
        memory_bar(ephemeral_entries.len(), 5),
        format!("{}/5", ephemeral_entries.len()),
    );
    eprintln!(
        "  {:<14} {} conversations     {}",
        "archive",
        archive_stats.count,
        format_bytes(archive_stats.total_bytes),
    );

    // Warnings
    for warning in &health.warnings {
        eprintln!("  {YELLOW}!{RESET} {warning}");
    }

    // Recent Sessions
    if !ephemeral_entries.is_empty() {
        eprintln!();
        eprintln!("  {BOLD}Recent Sessions{RESET}");
        eprintln!();

        for entry in ephemeral_entries.iter().rev() {
            eprintln!(
                "  {DIM}#{:<4}{RESET} {DIM}{:<8}{RESET} {:<5} {:<8} {}",
                entry.log_num,
                entry.age_display,
                entry.duration,
                format!("{} msgs", entry.message_count),
                entry.topics,
            );
        }
    }

    // Memory Sections
    if !memory_stats.sections.is_empty() {
        eprintln!();
        eprintln!("  {BOLD}Memory Sections{RESET}");
        eprintln!();
        eprintln!(
            "  {} sections · {} lines · last updated {}",
            memory_stats.sections.len(),
            memory_stats.line_count,
            memory_stats.freshness_display(),
        );

        let mut sorted: Vec<_> = memory_stats.sections.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = sorted
            .iter()
            .take(3)
            .map(|(name, size)| format!("{name} ({size} lines)"))
            .collect();
        if !top.is_empty() {
            eprintln!("  {DIM}largest: {}{RESET}", top.join(", "));
        }
    }

    eprintln!();
}

// ── Health assessment ───────────────────────────────────────────────────

pub fn assess_health(
    memory: &MemoryStats,
    archive: &ArchiveStats,
    max_memory_lines: usize,
) -> HealthAssessment {
    let mut warnings = Vec::new();
    let mut level = HealthLevel::Healthy;

    if memory.line_count > max_memory_lines * 90 / 100 {
        warnings.push(format!(
            "MEMORY.md at {}% — run distill",
            memory.line_count * 100 / max_memory_lines,
        ));
        level = HealthLevel::Alert;
    } else if memory.line_count > max_memory_lines * 75 / 100 {
        warnings.push(format!(
            "MEMORY.md approaching limit ({}%)",
            memory.line_count * 100 / max_memory_lines,
        ));
        level = HealthLevel::Watch;
    }

    if archive.count == 0 {
        warnings.push("No conversation archives yet".to_string());
        if !matches!(level, HealthLevel::Alert) {
            level = HealthLevel::Watch;
        }
    }

    if let Some(newest) = archive.newest_modified {
        if let Ok(elapsed) = newest.elapsed() {
            if elapsed.as_secs() > 7 * 86400 {
                warnings.push("Last archive is over 7 days old".to_string());
                if !matches!(level, HealthLevel::Alert) {
                    level = HealthLevel::Watch;
                }
            }
        }
    }

    HealthAssessment { level, warnings }
}

// ── Ephemeral parsing ───────────────────────────────────────────────────

pub fn parse_ephemeral_entries(recall: &RecallEcho) -> Vec<EphemeralEntry> {
    let ephemeral_path = recall.ephemeral_file();
    if !ephemeral_path.exists() {
        return Vec::new();
    }

    let content = match fs::read_to_string(&ephemeral_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let raw_entries: Vec<&str> = content
        .split("\n---\n")
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect();

    raw_entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let first_line = entry.lines().next().unwrap_or("");

            // Extract date from "## Session <id> — <date>"
            let date_str = first_line
                .split('—')
                .nth(1)
                .or_else(|| first_line.split(" - ").nth(1))
                .unwrap_or("")
                .trim();

            // Extract duration from "**Duration**: ~<dur>"
            let duration = entry
                .lines()
                .find(|l| l.contains("**Duration**"))
                .and_then(|l| {
                    l.split("~")
                        .nth(1)
                        .and_then(|s| s.split('|').next().map(|d| d.trim().to_string()))
                })
                .unwrap_or_else(|| "\u{2014}".to_string());

            // Extract message count from "**Messages**: <n>"
            let msg_count = entry
                .lines()
                .find(|l| l.contains("**Messages**"))
                .and_then(|l| {
                    l.split("**Messages**:").nth(1).and_then(|s| {
                        s.trim()
                            .split(|c: char| !c.is_ascii_digit())
                            .next()
                            .and_then(|n| n.parse::<u32>().ok())
                    })
                })
                .unwrap_or(0);

            // Extract summary from "**Summary**: <text>"
            let summary = entry
                .lines()
                .find(|l| l.contains("**Summary**"))
                .and_then(|l| l.split("**Summary**:").nth(1))
                .map(|s| {
                    let trimmed = s.trim();
                    if trimmed.len() > 50 {
                        format!("{}...", &trimmed[..47])
                    } else {
                        trimmed.to_string()
                    }
                })
                .unwrap_or_else(|| "\u{2014}".to_string());

            EphemeralEntry {
                log_num: format!("{}", i + 1),
                age_display: if date_str.is_empty() {
                    "\u{2014}".to_string()
                } else {
                    date_str.chars().take(16).collect()
                },
                duration,
                message_count: msg_count.to_string(),
                topics: summary,
            }
        })
        .collect()
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn memory_bar(count: usize, max: usize) -> String {
    let width = 10;
    let filled = if max > 0 {
        (count * width / max).min(width)
    } else {
        0
    };
    let empty = width - filled;

    let color = if count > max * 90 / 100 {
        RED
    } else if count > max * 75 / 100 {
        YELLOW
    } else {
        GREEN
    };

    format!(
        "{}{}{}{}",
        color,
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        RESET
    )
}

fn memory_status_word(count: usize, max: usize) -> &'static str {
    if count > max * 90 / 100 {
        "full"
    } else if count > max * 75 / 100 {
        "warning"
    } else {
        "ok"
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_age(time: std::time::SystemTime) -> String {
    let elapsed = time.elapsed().unwrap_or_default();
    let secs = elapsed.as_secs();

    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Find sections: returns (name, start_line, line_count)
pub fn find_sections(lines: &[&str]) -> Vec<(String, usize, usize)> {
    let mut sections = Vec::new();
    let mut current_name = String::new();
    let mut current_start = 0;

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("# ") || line.starts_with("## ") {
            if !current_name.is_empty() {
                sections.push((current_name.clone(), current_start, i - current_start));
            }
            current_name = line.trim_start_matches('#').trim().to_string();
            current_start = i;
        }
    }
    if !current_name.is_empty() {
        sections.push((current_name, current_start, lines.len() - current_start));
    }

    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_sections_basic() {
        let lines = vec![
            "# Memory",
            "",
            "## Server",
            "- host: vps",
            "- os: linux",
            "",
            "## Projects",
            "- project A",
        ];
        let sections = find_sections(&lines);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].0, "Memory");
        assert_eq!(sections[1].0, "Server");
        assert_eq!(sections[2].0, "Projects");
    }

    #[test]
    fn memory_bar_colors() {
        let bar = memory_bar(50, 200);
        assert!(bar.contains(GREEN));

        let bar = memory_bar(160, 200);
        assert!(bar.contains(YELLOW));

        let bar = memory_bar(190, 200);
        assert!(bar.contains(RED));
    }

    #[test]
    fn format_bytes_ranges() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn health_healthy_state() {
        let memory = MemoryStats {
            line_count: 100,
            sections: Vec::new(),
            modified: None,
        };
        let archive = ArchiveStats {
            count: 5,
            total_bytes: 1000,
            newest_modified: Some(std::time::SystemTime::now()),
        };
        let health = assess_health(&memory, &archive, 200);
        assert!(matches!(health.level, HealthLevel::Healthy));
        assert!(health.warnings.is_empty());
    }

    #[test]
    fn health_alert_on_full_memory() {
        let memory = MemoryStats {
            line_count: 195,
            sections: Vec::new(),
            modified: None,
        };
        let archive = ArchiveStats {
            count: 5,
            total_bytes: 1000,
            newest_modified: Some(std::time::SystemTime::now()),
        };
        let health = assess_health(&memory, &archive, 200);
        assert!(matches!(health.level, HealthLevel::Alert));
    }

    #[test]
    fn health_watch_on_no_archives() {
        let memory = MemoryStats {
            line_count: 50,
            sections: Vec::new(),
            modified: None,
        };
        let archive = ArchiveStats {
            count: 0,
            total_bytes: 0,
            newest_modified: None,
        };
        let health = assess_health(&memory, &archive, 200);
        assert!(matches!(health.level, HealthLevel::Watch));
    }
}
