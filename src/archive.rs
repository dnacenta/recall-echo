use std::fs;
use std::path::Path;

use crate::config;
use crate::ephemeral::{self, EphemeralEntry};
use crate::frontmatter::Frontmatter;
use crate::jsonl;
use crate::paths;
use crate::tags;

/// Scan conversations/ for highest conversation-NNN number. Returns 0 if none.
pub fn highest_conversation_number(conversations_dir: &Path) -> u32 {
    let entries = match fs::read_dir(conversations_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut max = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(num_str) = name
            .strip_prefix("conversation-")
            .and_then(|s| s.strip_suffix(".md"))
        {
            if let Ok(n) = num_str.parse::<u32>() {
                if n > max {
                    max = n;
                }
            }
        }
    }

    max
}

/// Append an entry to ARCHIVE.md (markdown table row)
pub fn append_index(
    archive_path: &Path,
    log_num: u32,
    date: &str,
    session_id: &str,
    topics: &[String],
    message_count: u32,
    duration: &str,
) -> Result<(), String> {
    use std::io::Write;

    // If file doesn't exist or is empty, write header first
    let needs_header = if archive_path.exists() {
        fs::read_to_string(archive_path)
            .unwrap_or_default()
            .trim()
            .is_empty()
    } else {
        true
    };

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive_path)
        .map_err(|e| format!("Failed to open ARCHIVE.md: {e}"))?;

    if needs_header {
        writeln!(file, "# Conversation Archive\n")
            .map_err(|e| format!("Failed to write ARCHIVE.md header: {e}"))?;
        writeln!(
            file,
            "| # | Date | Session | Topics | Messages | Duration |"
        )
        .map_err(|e| format!("Failed to write ARCHIVE.md header: {e}"))?;
        writeln!(
            file,
            "|---|------|---------|--------|----------|----------|"
        )
        .map_err(|e| format!("Failed to write ARCHIVE.md header: {e}"))?;
    }

    let topics_str = if topics.is_empty() {
        "—".to_string()
    } else {
        topics.join(", ")
    };

    writeln!(
        file,
        "| {log_num:03} | {date} | {session_id} | {topics_str} | {message_count} | {duration} |"
    )
    .map_err(|e| format!("Failed to write to ARCHIVE.md: {e}"))?;

    Ok(())
}

/// Main archive-session flow, called from the SessionEnd hook.
pub fn run_from_hook() -> Result<(), String> {
    let hook_input = jsonl::read_hook_input()?;
    archive_session_with_paths(
        &hook_input.session_id,
        &hook_input.transcript_path,
        &paths::claude_dir()?,
    )
}

/// Testable archive function with explicit paths.
pub fn archive_session_with_paths(
    session_id: &str,
    transcript_path: &str,
    base_dir: &Path,
) -> Result<(), String> {
    let conversations_dir = base_dir.join("conversations");
    let archive_index = base_dir.join("ARCHIVE.md");
    let ephemeral_path = base_dir.join("EPHEMERAL.md");

    if !conversations_dir.exists() {
        return Err(
            "conversations/ directory not found. Run `recall-echo init` first.".to_string(),
        );
    }

    // Parse the JSONL transcript
    let conv = jsonl::parse_transcript(transcript_path, session_id)?;

    // Skip empty sessions
    if conv.user_message_count == 0 {
        return Ok(());
    }

    // Determine next archive number
    let next_num = highest_conversation_number(&conversations_dir) + 1;

    // Generate metadata
    let now = jsonl::utc_now();
    let date = jsonl::date_from_timestamp(&now);
    let duration = match (&conv.first_timestamp, &conv.last_timestamp) {
        (Some(first), Some(last)) => jsonl::calculate_duration(first, last),
        _ => "unknown".to_string(),
    };
    let total_messages = conv.user_message_count + conv.assistant_message_count;
    let topics = jsonl::extract_topics(&conv, 5);
    let summary = jsonl::extract_summary(&conv);

    // Build frontmatter
    let fm = Frontmatter {
        log: next_num,
        date: now.clone(),
        session_id: session_id.to_string(),
        message_count: total_messages,
        duration: duration.clone(),
        source: "jsonl".to_string(),
        topics: topics.clone(),
    };

    // Convert to markdown with tags
    let md_body = jsonl::conversation_to_markdown(&conv, next_num);
    let conv_tags = tags::extract_tags(&conv);
    let tags_section = tags::format_tags_section(&conv_tags);
    let full_content = format!("{}\n\n{}{}", fm.render(), md_body, tags_section);

    // Write conversation file
    let conv_file = conversations_dir.join(format!("conversation-{next_num:03}.md"));
    fs::write(&conv_file, &full_content)
        .map_err(|e| format!("Failed to write conversation file: {e}"))?;

    // Append to ARCHIVE.md index
    append_index(
        &archive_index,
        next_num,
        &date,
        session_id,
        &topics,
        total_messages,
        &duration,
    )?;

    // Append to EPHEMERAL.md
    let entry = EphemeralEntry {
        session_id: session_id.to_string(),
        date: now,
        duration,
        message_count: total_messages,
        archive_file: format!("conversation-{next_num:03}.md"),
        summary,
    };
    ephemeral::append_entry(&ephemeral_path, &entry)?;
    let cfg = config::load(base_dir);
    ephemeral::trim_to_limit(&ephemeral_path, cfg.ephemeral.max_entries)?;

    eprintln!(
        "recall-echo: archived conversation-{:03}.md ({} messages)",
        next_num, total_messages
    );

    Ok(())
}

/// Archive all unarchived JSONL transcripts found under ~/.claude/projects/.
pub fn archive_all_unarchived() -> Result<(), String> {
    let base = paths::claude_dir()?;
    archive_all_with_base(&base)
}

pub fn archive_all_with_base(base: &Path) -> Result<(), String> {
    let conversations_dir = base.join("conversations");
    if !conversations_dir.exists() {
        return Err(
            "conversations/ directory not found. Run `recall-echo init` first.".to_string(),
        );
    }

    // Collect already-archived session IDs from conversation frontmatter
    let archived_sessions = collect_archived_sessions(&conversations_dir);

    // Find all JSONL files under ~/.claude/projects/
    let projects_dir = base.join("projects");
    if !projects_dir.exists() {
        eprintln!("No projects directory found — nothing to archive.");
        return Ok(());
    }

    let mut jsonl_files = find_jsonl_files(&projects_dir);
    jsonl_files.sort_by_key(|p| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });

    let mut archived_count = 0;
    let mut skipped_count = 0;

    for jsonl_path in &jsonl_files {
        let session_id = match jsonl_path.file_stem().and_then(|s| s.to_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };

        if archived_sessions.contains(&session_id) {
            skipped_count += 1;
            continue;
        }

        let path_str = jsonl_path.to_string_lossy().to_string();
        match archive_session_with_paths(&session_id, &path_str, base) {
            Ok(()) => archived_count += 1,
            Err(e) => {
                eprintln!("recall-echo: skipping {} — {}", session_id, e);
            }
        }
    }

    eprintln!(
        "recall-echo: archived {archived_count} conversation{}, skipped {skipped_count} already archived",
        if archived_count == 1 { "" } else { "s" }
    );

    Ok(())
}

fn collect_archived_sessions(conversations_dir: &Path) -> std::collections::HashSet<String> {
    let mut sessions = std::collections::HashSet::new();
    if let Ok(entries) = fs::read_dir(conversations_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("conversation-") && name.ends_with(".md") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    for line in content.lines().take(15) {
                        if let Some(sid) = line.strip_prefix("session_id: ") {
                            sessions.insert(sid.trim().trim_matches('"').to_string());
                            break;
                        }
                    }
                }
            }
        }
    }
    sessions
}

fn find_jsonl_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(find_jsonl_files(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highest_from_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(highest_conversation_number(tmp.path()), 0);
    }

    #[test]
    fn highest_from_sequential_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("conversation-001.md"), "").unwrap();
        fs::write(tmp.path().join("conversation-002.md"), "").unwrap();
        fs::write(tmp.path().join("conversation-003.md"), "").unwrap();
        assert_eq!(highest_conversation_number(tmp.path()), 3);
    }

    #[test]
    fn highest_with_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("conversation-001.md"), "").unwrap();
        fs::write(tmp.path().join("conversation-010.md"), "").unwrap();
        assert_eq!(highest_conversation_number(tmp.path()), 10);
    }

    #[test]
    fn highest_ignores_non_matching() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("conversation-003.md"), "").unwrap();
        fs::write(tmp.path().join("notes.md"), "").unwrap();
        fs::write(tmp.path().join("conversation-bad.md"), "").unwrap();
        assert_eq!(highest_conversation_number(tmp.path()), 3);
    }

    #[test]
    fn append_index_creates_header_and_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("ARCHIVE.md");

        append_index(
            &index,
            1,
            "2026-03-05",
            "abc123",
            &["auth".to_string()],
            34,
            "45m",
        )
        .unwrap();
        append_index(
            &index,
            2,
            "2026-03-05",
            "def456",
            &["ci".to_string(), "tests".to_string()],
            22,
            "20m",
        )
        .unwrap();

        let content = fs::read_to_string(&index).unwrap();
        assert!(content.contains("# Conversation Archive"));
        assert!(content.contains("| 001 | 2026-03-05 | abc123 | auth | 34 | 45m |"));
        assert!(content.contains("| 002 | 2026-03-05 | def456 | ci, tests | 22 | 20m |"));
    }

    #[test]
    fn append_index_to_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("ARCHIVE.md");
        fs::write(
            &index,
            "# Conversation Archive\n\n| # | Date | Session | Topics | Messages | Duration |\n|---|------|---------|--------|----------|----------|\n| 001 | 2026-03-05 | abc | test | 10 | 5m |\n",
        )
        .unwrap();

        append_index(&index, 2, "2026-03-05", "def", &[], 20, "10m").unwrap();

        let content = fs::read_to_string(&index).unwrap();
        assert!(content.contains("| 002 | 2026-03-05 | def | — | 20 | 10m |"));
        assert_eq!(content.matches("# Conversation Archive").count(), 1);
    }
}
