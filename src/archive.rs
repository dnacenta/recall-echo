//! Conversation archival — converts conversations into persistent markdown archives.
//!
//! Supports two input paths:
//! 1. **JSONL hook** — called directly by Claude Code SessionEnd hook (standalone)
//! 2. **Pulse-null** — called with in-memory Messages (behind feature flag)
//!
//! Both converge into `archive_conversation()` which writes the markdown file,
//! updates ARCHIVE.md, and appends to EPHEMERAL.md.

use std::fs;
use std::path::Path;

use crate::config;
use crate::conversation::{self, Conversation};
use crate::ephemeral::{self, EphemeralEntry};
use crate::frontmatter::Frontmatter;
use crate::summarize;
use crate::tags;

/// Session metadata provided by the caller.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub entity_name: String,
}

/// Result of archiving a conversation — used by callers for graph ingestion.
pub struct ArchiveResult {
    pub log_number: u32,
    pub full_content: String,
    pub session_id: String,
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
        "\u{2014}".to_string()
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

// ---------------------------------------------------------------------------
// Core archive function — works with Conversation (universal path)
// ---------------------------------------------------------------------------

/// Archive a conversation from internal types.
///
/// This is the core archive function. All input paths (JSONL, pulse-null)
/// converge here after converting to a Conversation.
///
/// Returns an ArchiveResult with the log number and content (for graph ingestion).
pub fn archive_conversation(
    memory_dir: &Path,
    conv: &Conversation,
    summary: &summarize::ConversationSummary,
    source: &str,
) -> Result<ArchiveResult, String> {
    let conversations_dir = memory_dir.join("conversations");
    let archive_index = memory_dir.join("ARCHIVE.md");
    let ephemeral_path = memory_dir.join("EPHEMERAL.md");

    if !conversations_dir.exists() {
        return Err("conversations/ directory not found. Run init first.".to_string());
    }

    // Skip empty sessions
    if conv.user_message_count == 0 {
        return Ok(ArchiveResult {
            log_number: 0,
            full_content: String::new(),
            session_id: conv.session_id.clone(),
        });
    }

    let next_num = highest_conversation_number(&conversations_dir) + 1;

    let now = conversation::utc_now();
    let date = conversation::date_from_timestamp(&now);
    let duration = match (&conv.first_timestamp, &conv.last_timestamp) {
        (Some(start), Some(end)) => conversation::calculate_duration(start, end),
        _ => "unknown".to_string(),
    };
    let total_messages = conv.total_messages();

    // Build frontmatter
    let fm = Frontmatter {
        log: next_num,
        date: now.clone(),
        session_id: conv.session_id.clone(),
        message_count: total_messages,
        duration: duration.clone(),
        source: source.to_string(),
        topics: summary.topics.clone(),
    };

    // Convert conversation to markdown
    let md_body = conversation::conversation_to_markdown(conv, next_num);

    // Extract tags
    let conv_tags = tags::extract_tags(&conv.entries);
    let tags_section = tags::format_tags_section(&conv_tags);

    // Add summary section if available
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
        &conv.session_id,
        &summary.topics,
        total_messages,
        &duration,
    )?;

    // Append to EPHEMERAL.md
    let entry = EphemeralEntry {
        session_id: conv.session_id.clone(),
        date: now,
        duration,
        message_count: total_messages,
        archive_file: format!("conversation-{next_num:03}.md"),
        summary: summary.summary.clone(),
    };
    ephemeral::append_entry(&ephemeral_path, &entry)?;
    let cfg = config::load_from_dir(memory_dir);
    ephemeral::trim_to_limit(&ephemeral_path, cfg.ephemeral.max_entries)?;

    eprintln!(
        "recall-echo: archived conversation-{:03}.md ({} messages)",
        next_num, total_messages
    );

    Ok(ArchiveResult {
        log_number: next_num,
        full_content,
        session_id: conv.session_id.clone(),
    })
}

/// Ingest an archive result into the knowledge graph (if enabled).
#[cfg(feature = "graph")]
pub fn graph_ingest(memory_dir: &Path, result: &ArchiveResult) {
    if result.log_number == 0 {
        return;
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("recall-echo: graph runtime error: {e}");
            return;
        }
    };
    if let Err(e) = rt.block_on(crate::graph_bridge::ingest_into_graph(
        memory_dir,
        &result.full_content,
        &result.session_id,
        Some(result.log_number),
    )) {
        eprintln!("recall-echo: graph ingestion warning: {e}");
    }
}

// ---------------------------------------------------------------------------
// JSONL path — for Claude Code hooks (standalone, no LLM)
// ---------------------------------------------------------------------------

/// Archive a session from a JSONL transcript file.
///
/// Parses JSONL, generates algorithmic summary, archives, and optionally
/// ingests into the knowledge graph.
pub fn archive_from_jsonl(
    base_dir: &Path,
    session_id: &str,
    transcript_path: &str,
) -> Result<u32, String> {
    let conv = crate::jsonl::parse_transcript(transcript_path, session_id)?;
    let summary = summarize::algorithmic_summary(&conv);
    let result = archive_conversation(base_dir, &conv, &summary, "jsonl")?;
    let log_number = result.log_number;

    #[cfg(feature = "graph")]
    graph_ingest(base_dir, &result);

    Ok(log_number)
}

/// Main archive-session flow, called from the SessionEnd hook.
/// Reads hook input from stdin.
pub fn run_from_hook() -> Result<(), String> {
    let hook_input = crate::jsonl::read_hook_input()?;
    let base_dir = crate::paths::claude_dir()?;
    archive_from_jsonl(
        &base_dir,
        &hook_input.session_id,
        &hook_input.transcript_path,
    )?;
    Ok(())
}

/// Archive all unarchived JSONL transcripts found under ~/.claude/projects/.
pub fn archive_all_unarchived() -> Result<(), String> {
    let base = crate::paths::claude_dir()?;
    archive_all_with_base(&base)
}

pub fn archive_all_with_base(base: &Path) -> Result<(), String> {
    let conversations_dir = base.join("conversations");
    if !conversations_dir.exists() {
        return Err(
            "conversations/ directory not found. Run `recall-echo init` first.".to_string(),
        );
    }

    let archived_sessions = collect_archived_sessions(&conversations_dir);

    let projects_dir = base.join("projects");
    if !projects_dir.exists() {
        eprintln!("No projects directory found \u{2014} nothing to archive.");
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
        match archive_from_jsonl(base, &session_id, &path_str) {
            Ok(_) => archived_count += 1,
            Err(e) => {
                eprintln!("recall-echo: skipping {} \u{2014} {}", session_id, e);
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

// ---------------------------------------------------------------------------
// Pulse-null path — behind feature flag
// ---------------------------------------------------------------------------

/// Archive a session from pulse-null in-memory messages.
///
/// Converts Messages to Conversation, uses LLM for summarization if available,
/// and optionally ingests into the knowledge graph.
#[cfg(feature = "pulse-null")]
pub async fn archive_session(
    memory_dir: &Path,
    messages: &[echo_system_types::llm::Message],
    metadata: &SessionMetadata,
    provider: Option<&dyn echo_system_types::llm::LmProvider>,
) -> Result<u32, String> {
    let mut conv = crate::pulse_null::messages_to_conversation(messages, &metadata.session_id);
    conv.first_timestamp = metadata.started_at.clone();
    conv.last_timestamp = metadata.ended_at.clone();

    let summary = summarize::extract_with_fallback(provider, &conv).await;
    let result = archive_conversation(memory_dir, &conv, &summary, "session")?;
    let log_number = result.log_number;

    // Graph ingestion (async path — no need for Runtime)
    #[cfg(feature = "graph")]
    if log_number > 0 {
        if let Err(e) = crate::graph_bridge::ingest_into_graph(
            memory_dir,
            &result.full_content,
            &result.session_id,
            Some(log_number),
        )
        .await
        {
            eprintln!("recall-echo: graph ingestion warning: {e}");
        }
    }

    Ok(log_number)
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
        assert!(content.contains("| 002 | 2026-03-05 | def | \u{2014} | 20 | 10m |"));
        assert_eq!(content.matches("# Conversation Archive").count(), 1);
    }

    #[test]
    fn archive_conversation_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        fs::create_dir_all(memory.join("conversations")).unwrap();

        let conv = Conversation {
            session_id: "test-abc".to_string(),
            first_timestamp: Some("2026-03-05T14:30:00Z".to_string()),
            last_timestamp: Some("2026-03-05T15:00:00Z".to_string()),
            user_message_count: 1,
            assistant_message_count: 1,
            entries: vec![
                conversation::ConversationEntry::UserMessage("Let's build something".to_string()),
                conversation::ConversationEntry::AssistantText("Sure, let's do it.".to_string()),
            ],
        };

        let summary = summarize::ConversationSummary {
            summary: "Built something cool".to_string(),
            topics: vec!["building".to_string()],
            decisions: vec![],
            action_items: vec![],
        };

        let result = archive_conversation(memory, &conv, &summary, "test").unwrap();
        assert_eq!(result.log_number, 1);
        assert!(memory.join("conversations/conversation-001.md").exists());

        let content = fs::read_to_string(memory.join("conversations/conversation-001.md")).unwrap();
        assert!(content.contains("session_id: \"test-abc\""));
        assert!(content.contains("source: \"test\""));
        assert!(content.contains("Built something cool"));
    }

    #[test]
    fn archive_conversation_skips_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        fs::create_dir_all(memory.join("conversations")).unwrap();

        let conv = Conversation::new("empty");
        let summary = summarize::ConversationSummary::default();

        let result = archive_conversation(memory, &conv, &summary, "test").unwrap();
        assert_eq!(result.log_number, 0);
    }
}
