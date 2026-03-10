//! Conversation archival — converts Messages into persistent markdown archives.
//!
//! Called by pulse-null's session manager at session end, or by the library API.
//! Uses `summarize::extract_with_fallback` for metadata extraction.

use std::fs;
use std::path::Path;

use echo_system_types::llm::{LmProvider, Message};

use crate::config;
use crate::conversation;
use crate::ephemeral::{self, EphemeralEntry};
use crate::frontmatter::Frontmatter;
use crate::summarize;
use crate::tags;

/// Session metadata provided by the caller (pulse-null session manager).
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub entity_name: String,
}

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

/// Append an entry to ARCHIVE.md (markdown table row).
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

/// Archive a session from in-memory messages.
///
/// This is the primary archive function called by pulse-null.
/// Returns the conversation number of the created archive.
pub async fn archive_session(
    memory_dir: &Path,
    messages: &[Message],
    metadata: &SessionMetadata,
    provider: Option<&dyn LmProvider>,
) -> Result<u32, String> {
    let conversations_dir = memory_dir.join("conversations");
    let archive_index = memory_dir.join("ARCHIVE.md");
    let ephemeral_path = memory_dir.join("EPHEMERAL.md");

    if !conversations_dir.exists() {
        return Err("conversations/ directory not found. Run init first.".to_string());
    }

    // Skip empty sessions
    let (user_count, _) = conversation::count_messages(messages);
    if user_count == 0 {
        return Ok(0);
    }

    let next_num = highest_conversation_number(&conversations_dir) + 1;

    // Generate metadata via LLM or algorithmic fallback
    let summary = summarize::extract_with_fallback(provider, messages).await;

    let now = conversation::utc_now();
    let date = conversation::date_from_timestamp(&now);
    let duration = match (&metadata.started_at, &metadata.ended_at) {
        (Some(start), Some(end)) => conversation::calculate_duration(start, end),
        _ => "unknown".to_string(),
    };
    let total_messages = {
        let (u, a) = conversation::count_messages(messages);
        u + a
    };

    // Build frontmatter
    let fm = Frontmatter {
        log: next_num,
        date: now.clone(),
        session_id: metadata.session_id.clone(),
        message_count: total_messages,
        duration: duration.clone(),
        source: "session".to_string(),
        topics: summary.topics.clone(),
    };

    // Convert conversation to markdown
    let md_body = conversation::conversation_to_markdown(messages, next_num);

    // Extract tags from flattened entries
    let entries = conversation::flatten_messages(messages);
    let conv_tags = tags::extract_tags(&entries);
    let tags_section = tags::format_tags_section(&conv_tags);

    // Add LLM summary section if available
    let summary_section = if !summary.summary.is_empty() {
        let mut s = format!("## Summary\n\n{}\n\n", summary.summary);
        if !summary.decisions.is_empty() {
            s.push_str("**Decisions**:\n");
            for d in &summary.decisions {
                s.push_str(&format!("- {d}\n"));
            }
            s.push('\n');
        }
        if !summary.action_items.is_empty() {
            s.push_str("**Action Items**:\n");
            for a in &summary.action_items {
                s.push_str(&format!("- {a}\n"));
            }
            s.push('\n');
        }
        s
    } else {
        String::new()
    };

    let full_content = format!(
        "{}\n\n{}{}\n{}",
        fm.render(),
        summary_section,
        md_body,
        tags_section
    );

    // Write conversation file
    let conv_file = conversations_dir.join(format!("conversation-{next_num:03}.md"));
    fs::write(&conv_file, &full_content)
        .map_err(|e| format!("Failed to write conversation file: {e}"))?;

    // Append to ARCHIVE.md index
    append_index(
        &archive_index,
        next_num,
        &date,
        &metadata.session_id,
        &summary.topics,
        total_messages,
        &duration,
    )?;

    // Append to EPHEMERAL.md
    let entry = EphemeralEntry {
        session_id: metadata.session_id.clone(),
        date: now,
        duration,
        message_count: total_messages,
        archive_file: format!("conversation-{next_num:03}.md"),
        summary: summary.summary,
    };
    ephemeral::append_entry(&ephemeral_path, &entry)?;
    let cfg = config::load_from_dir(memory_dir);
    ephemeral::trim_to_limit(&ephemeral_path, cfg.ephemeral.max_entries)?;

    eprintln!(
        "recall-echo: archived conversation-{:03}.md ({} messages)",
        next_num, total_messages
    );

    Ok(next_num)
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
