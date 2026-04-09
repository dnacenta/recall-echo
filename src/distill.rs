use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::RecallError;
use crate::paths;

const BOLD: &str = "\x1b[1m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Analyze MEMORY.md and suggest distillation actions.
pub fn run() -> Result<(), RecallError> {
    let base = paths::entity_root()?;
    run_with_base(&base)
}

pub fn run_with_base(base: &Path) -> Result<(), RecallError> {
    let memory_path = base.join("memory/MEMORY.md");
    if !memory_path.exists() {
        return Err(RecallError::NotInitialized(
            "MEMORY.md not found. Run `recall-echo init` first.".into(),
        ));
    }

    let content = fs::read_to_string(&memory_path)?;
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();

    eprintln!("\n{BOLD}recall-echo distill{RESET} — MEMORY.md analysis\n");
    eprintln!("  Lines: {line_count}/200");

    if line_count < 140 {
        eprintln!("  {DIM}MEMORY.md is healthy — no distillation needed.{RESET}\n");
        return Ok(());
    }

    let suggestions = analyze_memory(&lines);

    if suggestions.is_empty() {
        eprintln!("  {DIM}No specific suggestions — consider manual review.{RESET}\n");
        return Ok(());
    }

    eprintln!("\n  {YELLOW}Suggestions:{RESET}\n");
    for (i, suggestion) in suggestions.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, suggestion);
    }
    eprintln!();

    Ok(())
}

fn analyze_memory(lines: &[&str]) -> Vec<String> {
    let mut suggestions = Vec::new();

    // 1. Find sections and their sizes
    let sections = find_sections(lines);
    for (name, size) in &sections {
        if *size > 30 {
            suggestions.push(format!(
                "Section \"{name}\" is {size} lines — consider moving details to a topic file"
            ));
        }
    }

    // 2. Find potential duplicates (lines with high similarity)
    let duplicates = find_near_duplicates(lines);
    if !duplicates.is_empty() {
        suggestions.push(format!(
            "Found {} potential duplicate entries — consider merging:",
            duplicates.len()
        ));
        for (a, b) in duplicates.iter().take(3) {
            suggestions.push(format!("    Lines {a} and {b}"));
        }
    }

    // 3. Find stale patterns (dates more than 30 days old)
    let stale_count = count_stale_entries(lines);
    if stale_count > 0 {
        suggestions.push(format!(
            "{stale_count} entries reference dates >30 days old — review for relevance"
        ));
    }

    // 4. Check for empty sections
    let empty = find_empty_sections(lines);
    for name in &empty {
        suggestions.push(format!("Section \"{name}\" appears empty — remove it"));
    }

    // 5. Suggest topic file extraction if over 170 lines
    if lines.len() > 170 {
        suggestions.push(
            "Over 170 lines — extract detailed sections into topic files in memory/".to_string(),
        );
    }

    suggestions
}

/// Find markdown sections (## headers) and count lines in each.
fn find_sections(lines: &[&str]) -> Vec<(String, usize)> {
    let mut sections = Vec::new();
    let mut current_name = String::new();
    let mut current_start = 0;

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("# ") || line.starts_with("## ") {
            if !current_name.is_empty() {
                sections.push((current_name.clone(), i - current_start));
            }
            current_name = line.trim_start_matches('#').trim().to_string();
            current_start = i;
        }
    }
    if !current_name.is_empty() {
        sections.push((current_name, lines.len() - current_start));
    }

    sections
}

/// Find lines that are very similar (potential duplicates).
fn find_near_duplicates(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut duplicates = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

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

        if let Some(&prev_line) = seen.get(&normalized) {
            duplicates.push((prev_line + 1, i + 1));
        } else {
            seen.insert(normalized, i);
        }
    }

    duplicates
}

/// Count entries that reference dates more than ~30 days old.
fn count_stale_entries(lines: &[&str]) -> usize {
    let now = crate::conversation::utc_now();
    let current_year_month = &now[..7]; // "2026-03"

    let mut stale = 0;
    for line in lines {
        if let Some(pos) = line.find("202") {
            if pos + 7 <= line.len() {
                let date_fragment = &line[pos..pos + 7];
                if date_fragment.len() == 7
                    && date_fragment.chars().nth(4) == Some('-')
                    && date_fragment < current_year_month
                {
                    let month_diff = rough_month_diff(date_fragment, current_year_month);
                    if month_diff > 1 {
                        stale += 1;
                    }
                }
            }
        }
    }

    stale
}

fn rough_month_diff(earlier: &str, later: &str) -> i32 {
    let parse = |s: &str| -> Option<(i32, i32)> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() >= 2 {
            Some((parts[0].parse().ok()?, parts[1].parse().ok()?))
        } else {
            None
        }
    };

    match (parse(earlier), parse(later)) {
        (Some((y1, m1)), Some((y2, m2))) => (y2 - y1) * 12 + (m2 - m1),
        _ => 0,
    }
}

/// Find sections that appear to be empty (header followed by another header or end).
fn find_empty_sections(lines: &[&str]) -> Vec<String> {
    let mut empty = Vec::new();
    let mut prev_header: Option<String> = None;
    let mut has_content = false;

    for line in lines {
        let is_section = line.starts_with("## ");
        let is_any_header = is_section || line.starts_with("# ");
        if is_any_header {
            if let Some(ref header) = prev_header {
                if !has_content {
                    empty.push(header.clone());
                }
            }
            prev_header = if is_section {
                Some(line.trim_start_matches('#').trim().to_string())
            } else {
                None
            };
            has_content = false;
        } else if !line.trim().is_empty() && !line.starts_with("<!--") {
            has_content = true;
        }
    }

    if let Some(ref header) = prev_header {
        if !has_content {
            empty.push(header.clone());
        }
    }

    empty
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
    }

    #[test]
    fn find_near_duplicates_basic() {
        let lines = vec![
            "# Memory",
            "",
            "The server runs on Ubuntu Linux",
            "Some other content here",
            "The server runs on Ubuntu Linux",
        ];
        let dups = find_near_duplicates(&lines);
        assert_eq!(dups.len(), 1);
    }

    #[test]
    fn find_empty_sections_basic() {
        let lines = vec![
            "# Memory",
            "",
            "## Filled",
            "content here",
            "",
            "## Empty",
            "",
            "## Also Filled",
            "more content",
        ];
        let empty = find_empty_sections(&lines);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0], "Empty");
    }

    #[test]
    fn rough_month_diff_basic() {
        assert_eq!(rough_month_diff("2026-01", "2026-03"), 2);
        assert_eq!(rough_month_diff("2025-12", "2026-02"), 2);
        assert_eq!(rough_month_diff("2026-03", "2026-03"), 0);
    }

    #[test]
    fn analyze_healthy_memory() {
        let lines: Vec<&str> = (0..100).map(|_| "some content line").collect();
        let suggestions = analyze_memory(&lines);
        assert!(suggestions.len() <= 1);
    }
}
