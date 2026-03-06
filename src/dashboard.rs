use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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

/// Render the neofetch-style memory dashboard to stdout.
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

    println!();
    let logo_width = 26;
    for (i, logo_line) in logo_lines.iter().enumerate() {
        if i < meta_lines.len() {
            println!(
                "  {GREEN}{:<width$}{RESET}  {}",
                logo_line,
                meta_lines[i],
                width = logo_width,
            );
        } else {
            println!("  {GREEN}{}{RESET}", logo_line);
        }
    }

    // Print remaining metadata if logo ran out of lines
    for meta_line in meta_lines.iter().skip(logo_lines.len()) {
        println!("  {:<width$}  {}", "", meta_line, width = logo_width);
    }

    println!("  v{version}");
    println!("{SEPARATOR}");

    // Memory Health
    println!();
    println!(
        "  {BOLD}Memory Health{RESET}                   {}",
        health.display()
    );
    println!();

    println!(
        "  {:<14} {}  {:<8} {}",
        "curated",
        memory_bar(memory_stats.line_count, max_memory_lines),
        format!("{}/{}", memory_stats.line_count, max_memory_lines),
        memory_status_word(memory_stats.line_count, max_memory_lines),
    );
    println!(
        "  {:<14} {}  {:<8} ok",
        "ephemeral",
        memory_bar(ephemeral_entries.len(), 5),
        format!("{}/5", ephemeral_entries.len()),
    );
    println!(
        "  {:<14} {} conversations     {}",
        "archive",
        archive_stats.count,
        format_bytes(archive_stats.total_bytes),
    );

    // Warnings
    for warning in &health.warnings {
        println!("  {YELLOW}!{RESET} {warning}");
    }

    // Recent Sessions
    if !ephemeral_entries.is_empty() {
        println!();
        println!("  {BOLD}Recent Sessions{RESET}");
        println!();

        for entry in ephemeral_entries.iter().rev() {
            println!(
                "  {DIM}#{:<4}{RESET} {DIM}{:<8}{RESET} {:<5} {:<8} {}",
                entry.log_num,
                entry.age_display,
                entry.duration,
                format!("{} msgs", entry.message_count),
                entry.topics,
            );
        }
    }

    // Memory Notes
    if !memory_stats.sections.is_empty() {
        println!();
        println!("  {BOLD}Memory Notes{RESET}");
        println!();
        println!(
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
            println!("  {DIM}largest: {}{RESET}", top.join(", "));
        }
    }

    println!();
}

// ── Search ──────────────────────────────────────────────────────────────

/// Line-level search across conversation archives.
pub fn search_lines(recall: &RecallEcho, query: &str) -> Result<(), String> {
    let conv_dir = recall.conversations_dir();
    if !conv_dir.exists() {
        println!("  No conversation archives found.");
        return Ok(());
    }

    let files = list_conversation_files(&conv_dir)?;
    if files.is_empty() {
        println!("  No conversation archives found.");
        return Ok(());
    }

    let query_lower = query.to_lowercase();
    let mut total_matches = 0;

    for file in &files {
        let content = fs::read_to_string(file)
            .map_err(|e| format!("Failed to read {}: {e}", file.display()))?;
        let filename = file.file_name().unwrap_or_default().to_string_lossy();
        let mut file_matches = Vec::new();

        for (i, line) in content.lines().enumerate() {
            if line.to_lowercase().contains(&query_lower) {
                file_matches.push((i + 1, line.to_string()));
            }
        }

        if !file_matches.is_empty() {
            println!("\n  {CYAN}{filename}{RESET}");
            for (line_num, line) in file_matches.iter().take(5) {
                let display = if line.len() > 100 {
                    format!("{}...", &line[..97])
                } else {
                    line.to_string()
                };
                println!("  {DIM}{line_num:>4}{RESET}  {display}");
            }
            if file_matches.len() > 5 {
                println!(
                    "  {DIM}  ...and {} more matches{RESET}",
                    file_matches.len() - 5
                );
            }
            total_matches += file_matches.len();
        }
    }

    if total_matches == 0 {
        println!("  No matches for \"{query}\"");
    } else {
        println!(
            "\n  {DIM}{total_matches} matches across {} files{RESET}",
            files.len()
        );
    }

    Ok(())
}

/// Ranked file-level search across conversation archives.
pub fn search_ranked(recall: &RecallEcho, query: &str) -> Result<(), String> {
    let conv_dir = recall.conversations_dir();
    if !conv_dir.exists() {
        println!("  No conversation archives found.");
        return Ok(());
    }

    let files = list_conversation_files(&conv_dir)?;
    if files.is_empty() {
        println!("  No conversation archives found.");
        return Ok(());
    }

    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();
    let mut scored: Vec<(f64, &PathBuf, Vec<String>)> = Vec::new();

    for (idx, file) in files.iter().enumerate() {
        let content = fs::read_to_string(file)
            .map_err(|e| format!("Failed to read {}: {e}", file.display()))?;
        let content_lower = content.to_lowercase();

        let match_count = content_lower.matches(&query_lower).count();
        if match_count == 0 {
            continue;
        }

        let words_found = query_words
            .iter()
            .filter(|w| content_lower.contains(**w))
            .count();
        let word_ratio = words_found as f64 / query_words.len().max(1) as f64;

        let recency = (idx as f64 + 1.0) / files.len() as f64;

        let content_boost = if content_lower.contains(&format!("### user\n\n{}", query_lower)) {
            1.5
        } else {
            1.0
        };

        let score = (match_count as f64 * word_ratio + recency) * content_boost;

        let previews: Vec<String> = content
            .lines()
            .filter(|l| {
                let lower = l.to_lowercase();
                lower.contains(&query_lower) && !l.starts_with('#') && !l.starts_with("---")
            })
            .take(3)
            .map(|l| {
                if l.len() > 90 {
                    format!("{}...", &l[..87])
                } else {
                    l.to_string()
                }
            })
            .collect();

        scored.push((score, file, previews));
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    if scored.is_empty() {
        println!("  No matches for \"{query}\"");
        return Ok(());
    }

    println!();
    println!(
        "  {BOLD}Search Results{RESET}  ({} files matched)\n",
        scored.len()
    );

    for (score, file, previews) in scored.iter().take(10) {
        let filename = file.file_name().unwrap_or_default().to_string_lossy();
        println!("  {CYAN}{filename}{RESET}  {DIM}(score: {score:.1}){RESET}");
        for preview in previews {
            println!("    {DIM}{preview}{RESET}");
        }
    }

    println!();
    Ok(())
}

// ── Auto-distill ────────────────────────────────────────────────────────

/// Analyze MEMORY.md and auto-extract heavy sections into topic files.
pub fn auto_distill(recall: &RecallEcho, max_lines: usize) -> Result<(), String> {
    let memory_path = recall.memory_file();
    let memory_dir = recall.memory_dir();

    if !memory_path.exists() {
        println!("  MEMORY.md not found. Nothing to distill.");
        return Ok(());
    }

    let content =
        fs::read_to_string(&memory_path).map_err(|e| format!("Failed to read MEMORY.md: {e}"))?;
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();

    println!();
    if line_count > (max_lines * 85 / 100) {
        println!(
            "  {YELLOW}!{RESET} MEMORY.md at {line_count}/{max_lines} lines ({}%) — cleanup recommended",
            line_count * 100 / max_lines,
        );
    } else {
        println!(
            "  MEMORY.md at {line_count}/{max_lines} lines ({}%) — {GREEN}healthy{RESET}",
            line_count * 100 / max_lines,
        );
        println!();
        return Ok(());
    }

    // Find heavy sections
    let sections = find_sections(&lines);
    let mut extractions: Vec<(String, usize, PathBuf)> = Vec::new();

    for (name, start, size) in &sections {
        if *size <= 30 {
            continue;
        }

        let slug: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let slug = slug.trim_matches('-').to_string();
        let topic_path = memory_dir.join(format!("{slug}.md"));

        let section_lines: Vec<&str> = lines[*start..*start + *size].to_vec();
        let section_content = section_lines.join("\n");

        fs::write(&topic_path, format!("{section_content}\n"))
            .map_err(|e| format!("Failed to write {}: {e}", topic_path.display()))?;

        extractions.push((name.clone(), *size, topic_path));
    }

    if extractions.is_empty() {
        let suggestions = analyze_non_section_issues(&lines);
        if suggestions.is_empty() {
            println!("  {DIM}No large sections to extract. Consider manual review.{RESET}");
        } else {
            println!();
            println!("  {BOLD}Suggestions{RESET}");
            println!();
            for (i, s) in suggestions.iter().enumerate() {
                println!("  {}. {s}", i + 1);
            }
        }
        println!();
        return Ok(());
    }

    // Rewrite MEMORY.md with references
    let mut new_lines: Vec<String> = Vec::new();
    let mut skip_until_next_section = false;

    for (i, line) in lines.iter().enumerate() {
        let is_extracted = extractions.iter().find(|(name, _, _)| {
            sections
                .iter()
                .any(|(sname, start, _)| sname == name && *start == i)
        });

        if let Some(extraction) = is_extracted {
            new_lines.push(line.to_string());
            let rel_path = extraction
                .2
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            new_lines.push(format!("See memory/{rel_path} for details."));
            new_lines.push(String::new());
            skip_until_next_section = true;
            continue;
        }

        if skip_until_next_section {
            if (line.starts_with("# ") || line.starts_with("## ")) && i > 0 {
                skip_until_next_section = false;
                new_lines.push(line.to_string());
            }
            continue;
        }

        new_lines.push(line.to_string());
    }

    let new_content = new_lines.join("\n");
    fs::write(&memory_path, format!("{new_content}\n"))
        .map_err(|e| format!("Failed to write MEMORY.md: {e}"))?;

    // Report
    println!();
    println!("  {BOLD}Extracted{RESET}");
    println!();
    for (name, size, path) in &extractions {
        let rel = path.file_name().unwrap_or_default().to_string_lossy();
        println!("  {GREEN}→{RESET} {name} ({size} lines) → memory/{rel}");
    }

    let new_line_count = new_content.lines().count();
    println!();
    println!(
        "  MEMORY.md: {line_count} → {new_line_count} lines ({}%)",
        new_line_count * 100 / max_lines,
    );
    println!();

    Ok(())
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

// ── Helpers ─────────────────────────────────────────────────────────────

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

            let date_str = first_line
                .split('—')
                .nth(1)
                .or_else(|| first_line.split(" - ").nth(1))
                .unwrap_or("")
                .trim();

            let msg_count = entry
                .lines()
                .find(|l| l.contains("messages"))
                .and_then(|l| {
                    l.split('(')
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|n| n.parse::<u32>().ok())
                })
                .unwrap_or(0);

            let topics: Vec<&str> = entry
                .lines()
                .filter(|l| l.starts_with("- ") && !l.contains("...and"))
                .take(3)
                .map(|l| l.trim_start_matches("- "))
                .collect();

            let topics_display = if topics.is_empty() {
                "\u{2014}".to_string() // em dash
            } else {
                let joined: String = topics
                    .iter()
                    .map(|t| {
                        if t.len() > 30 {
                            format!("{}...", &t[..27])
                        } else {
                            t.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if joined.len() > 60 {
                    format!("{}...", &joined[..57])
                } else {
                    joined
                }
            };

            EphemeralEntry {
                log_num: format!("{}", i + 1),
                age_display: if date_str.is_empty() {
                    "\u{2014}".to_string()
                } else {
                    date_str.chars().take(16).collect()
                },
                duration: "\u{2014}".to_string(),
                message_count: msg_count.to_string(),
                topics: topics_display,
            }
        })
        .collect()
}

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

fn list_conversation_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read conversations dir: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("conversation-")
                && p.extension().is_some_and(|ext| ext == "md")
        })
        .collect();

    files.sort();
    Ok(files)
}

fn analyze_non_section_issues(lines: &[&str]) -> Vec<String> {
    let mut suggestions = Vec::new();

    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut dup_count = 0;

    for (i, line) in lines.iter().enumerate() {
        let normalized: String = line
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ");

        if normalized.len() < 20 {
            continue;
        }

        if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(normalized) {
            e.insert(i);
        } else {
            dup_count += 1;
        }
    }

    if dup_count > 0 {
        suggestions.push(format!(
            "{dup_count} potential duplicate entries found — consider merging"
        ));
    }

    suggestions
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
    fn non_section_duplicates() {
        let lines = vec![
            "# Memory",
            "",
            "The server runs on Ubuntu Linux with SSH access",
            "Some other content here that is long enough",
            "The server runs on Ubuntu Linux with SSH access",
        ];
        let suggestions = analyze_non_section_issues(&lines);
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].contains("duplicate"));
    }
}
