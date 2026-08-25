// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversation archival — converts conversations into persistent markdown archives.
//!
//! Supports two input paths:
//! 1. **JSONL hook** — called directly by Claude Code SessionEnd hook (standalone)
//! 2. **Pulse-null** — called with in-memory Messages (behind feature flag)
//!
//! Both converge into `archive_conversation()` which writes the markdown file,
//! updates ARCHIVE.md, and appends to EPHEMERAL.md.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::config;
use crate::conversation::{self, Conversation};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
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
#[must_use]
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
) -> Result<(), RecallError> {
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
        .open(archive_path)?;

    if needs_header {
        writeln!(file, "# Conversation Archive\n")?;
        writeln!(
            file,
            "| # | Date | Session | Topics | Messages | Duration |"
        )?;
        writeln!(
            file,
            "|---|------|---------|--------|----------|----------|"
        )?;
    }

    let topics_str = if topics.is_empty() {
        "\u{2014}".to_string()
    } else {
        topics.join(", ")
    };

    writeln!(
        file,
        "| {log_num:03} | {date} | {session_id} | {topics_str} | {message_count} | {duration} |"
    )?;

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
) -> Result<ArchiveResult, RecallError> {
    let conversations_dir = memory_dir.join("conversations");
    let archive_index = memory_dir.join("ARCHIVE.md");
    let ephemeral_path = memory_dir.join("EPHEMERAL.md");

    if !conversations_dir.exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run init first.".into(),
        ));
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
                let _ = writeln!(s, "- {d}");
            }
            s.push('\n');
        }
        if !summary.action_items.is_empty() {
            s.push_str("**Action Items**:\n");
            for a in &summary.action_items {
                let _ = writeln!(s, "- {a}");
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
    fs::write(&conv_file, &full_content)?;

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

    eprintln!("recall-echo: archived conversation-{next_num:03}.md ({total_messages} messages)");

    Ok(ArchiveResult {
        log_number: next_num,
        full_content,
        session_id: conv.session_id.clone(),
    })
}

/// Ingest an archive result into the knowledge graph.
pub fn graph_ingest(memory_dir: &Path, result: &ArchiveResult) {
    if result.log_number == 0 {
        return;
    }
    let rt = match client_runtime() {
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

/// A runtime for a client-side command: a handful of socket round-trips, plus
/// whatever the daemon does on our behalf. One thread is enough — the worker
/// pool of a multi-thread runtime exists to be idle here.
fn client_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// The pipeline documents to sync, or `None` when auto-sync is off, no
/// `docs_dir` is configured, or there is no graph to sync into.
fn pipeline_docs_to_sync(memory_dir: &Path) -> Option<crate::graph::types::PipelineDocuments> {
    let cfg = config::load_from_dir(memory_dir);
    let pipeline = match cfg.pipeline {
        Some(ref p) if p.auto_sync == Some(true) => p,
        _ => return None,
    };

    let docs_dir = match pipeline.docs_dir {
        Some(ref d) => {
            let path = std::path::PathBuf::from(shellexpand_path(d));
            if !path.exists() {
                eprintln!(
                    "recall-echo: pipeline docs_dir not found: {}",
                    path.display()
                );
                return None;
            }
            path
        }
        None => {
            eprintln!("recall-echo: pipeline auto_sync enabled but no docs_dir configured");
            return None;
        }
    };

    if !memory_dir.join("graph").exists() {
        return None;
    }
    Some(read_pipeline_docs(&docs_dir))
}

/// Sync pipeline documents into the graph (if auto_sync enabled).
///
/// Non-blocking: logs warnings on failure but never fails the caller.
pub fn pipeline_sync_on_archive(memory_dir: &Path) {
    let Some(docs) = pipeline_docs_to_sync(memory_dir) else {
        return;
    };
    let rt = match client_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("recall-echo: pipeline sync runtime error: {e}");
            return;
        }
    };
    report_pipeline_sync(rt.block_on(crate::graph_bridge::sync_pipeline_into_graph(
        memory_dir, docs,
    )));
}

/// Async version of pipeline_sync_on_archive for use in async contexts (pulse-null).
#[cfg(feature = "pulse-null")]
async fn pipeline_sync_on_archive_async(memory_dir: &Path) {
    let Some(docs) = pipeline_docs_to_sync(memory_dir) else {
        return;
    };
    report_pipeline_sync(crate::graph_bridge::sync_pipeline_into_graph(memory_dir, docs).await);
}

fn report_pipeline_sync(result: Result<crate::graph::types::PipelineSyncReport, RecallError>) {
    match result {
        Ok(report) => {
            if report.entities_created > 0
                || report.entities_updated > 0
                || report.entities_archived > 0
            {
                eprintln!(
                    "recall-echo: pipeline synced — +{} created, ~{} updated, -{} archived",
                    report.entities_created, report.entities_updated, report.entities_archived
                );
            }
        }
        Err(e) => eprintln!("recall-echo: pipeline sync warning: {e}"),
    }
}

fn read_pipeline_docs(docs_dir: &Path) -> crate::graph::types::PipelineDocuments {
    crate::graph::types::PipelineDocuments {
        learning: read_opt_file(docs_dir, "LEARNING.md"),
        thoughts: read_opt_file(docs_dir, "THOUGHTS.md"),
        curiosity: read_opt_file(docs_dir, "CURIOSITY.md"),
        reflections: read_opt_file(docs_dir, "REFLECTIONS.md"),
        praxis: read_opt_file(docs_dir, "PRAXIS.md"),
    }
}

fn read_opt_file(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap_or_default()
}

fn shellexpand_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// JSONL path — for Claude Code hooks (standalone, no LLM)
// ---------------------------------------------------------------------------

/// Archive a session from a JSONL transcript file.
///
/// Parses JSONL, generates algorithmic summary, archives, and optionally
/// ingests into the knowledge graph. Both graph steps are daemon requests, so
/// the hook pays for one warm store and one embedding-model load, not two.
pub fn archive_from_jsonl(
    base_dir: &Path,
    session_id: &str,
    transcript_path: &str,
) -> Result<u32, RecallError> {
    let conv = crate::jsonl::parse_transcript(transcript_path, session_id)?;
    archive_and_ingest(base_dir, &conv, "jsonl")
}

/// Summarise, archive, and hand the result to the graph.
///
/// The tail every input path shares, whichever CLI's transcript it started as.
fn archive_and_ingest(
    base_dir: &Path,
    conv: &Conversation,
    source: &str,
) -> Result<u32, RecallError> {
    let summary = summarize::algorithmic_summary(conv);
    let result = archive_conversation(base_dir, conv, &summary, source)?;
    let log_number = result.log_number;

    graph_ingest(base_dir, &result);
    pipeline_sync_on_archive(base_dir);

    Ok(log_number)
}

/// What a hook pointed `archive-session` at.
///
/// Deciding this is separate from acting on it because the decision is the part
/// worth testing: which transcripts are read, which are declined, and which are
/// simply absent.
#[derive(Debug)]
enum HookTarget {
    /// The payload named no transcript at all.
    Unnamed,
    /// A path with nothing at it — a session that was never persisted.
    Absent,
    /// Claude Code's JSON Lines, to be streamed from the path.
    ClaudeJsonl,
    /// A Gemini chat session document, already read.
    GeminiSession(Box<Conversation>),
    /// A file in neither format.
    Unreadable,
}

/// Work out what is at the transcript path, reading it only as far as needed.
fn classify(transcript_path: &str, session_id: &str) -> HookTarget {
    if transcript_path.is_empty() {
        return HookTarget::Unnamed;
    }
    if !Path::new(transcript_path).exists() {
        return HookTarget::Absent;
    }
    // Sniffed rather than assumed: a migrated Gemini hook hands this command a
    // JSON document, and every line of one fails the JSON Lines parser.
    if crate::jsonl::is_jsonl_transcript(transcript_path) {
        return HookTarget::ClaudeJsonl;
    }

    fs::read_to_string(transcript_path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|document| crate::transcript::gemini::parse_session(&document, session_id))
        .map_or(HookTarget::Unreadable, |conv| {
            HookTarget::GeminiSession(Box::new(conv))
        })
}

/// Main archive-session flow, called from the SessionEnd hook.
/// Reads hook input from stdin.
pub fn run_from_hook(entity_root: Option<&Path>) -> Result<(), RecallError> {
    let hook_input = crate::jsonl::read_hook_input()?;
    let base_dir = crate::paths::resolved_hook_base_dir(entity_root)?;
    run_with_hook_input(&hook_input, &base_dir)
}

/// Archive the session named by a hook input.
///
/// # Why nothing here returns an error
///
/// A hook that exits nonzero fails the invocation that ran it, every time. So
/// the three ways this can have nothing to do all report themselves and exit
/// zero:
///
/// - **No transcript path.** Some other harness's payload, spelled some other
///   way. Named, with what to do about it.
/// - **No file at the path.** Sessions run with `--no-session-persistence`
///   never write one. Normal, and silent about anything but the fact.
/// - **A transcript we cannot read.** Gemini's `hooks migrate --from-claude`
///   copies this command into Gemini's own settings, where it is handed a JSON
///   session document rather than Claude Code's JSON Lines. That shape is
///   [read](crate::transcript::gemini) when it is recognisable; when it is not,
///   the message says which file and which format, because a user who ran a
///   migration command deserves to know it half-worked.
///
/// Everything written here goes to **stderr**: Gemini requires a hook's stdout
/// to be JSON and nothing else, and stray prose there is swallowed at best.
pub fn run_with_hook_input(
    hook_input: &crate::jsonl::HookInput,
    base_dir: &Path,
) -> Result<(), RecallError> {
    let transcript_path = hook_input.transcript_path.trim();

    match classify(transcript_path, &hook_input.session_id) {
        HookTarget::Unnamed => {
            eprintln!(
                "recall-echo: the hook payload names no transcript, so there is nothing to \
                 archive. `archive-session` expects a SessionEnd payload on stdin, shaped \
                 {{\"session_id\": …, \"transcript_path\": …}}."
            );
        }
        HookTarget::Absent => {
            eprintln!(
                "recall-echo: no transcript at {transcript_path} (session not persisted), \
                 nothing to archive"
            );
        }
        HookTarget::ClaudeJsonl => {
            archive_from_jsonl(base_dir, &hook_input.session_id, transcript_path)?;
        }
        HookTarget::GeminiSession(conv) => {
            archive_and_ingest(base_dir, &conv, "gemini")?;
        }
        HookTarget::Unreadable => {
            eprintln!(
                "recall-echo: {transcript_path} is neither a Claude Code JSONL transcript nor \
                 a Gemini chat session, so nothing was archived. If this hook came from \
                 `gemini hooks migrate --from-claude`, recall-echo reads Gemini sessions at \
                 ~/.gemini/tmp/<hash>/chats/session-*.json — please report this file's shape \
                 at https://github.com/dnacenta/recall-echo/issues so it can be read too."
            );
        }
    }
    Ok(())
}

/// Archive all unarchived JSONL transcripts found under ~/.claude/projects/.
pub fn archive_all_unarchived() -> Result<(), RecallError> {
    let base = crate::paths::claude_dir()?;
    archive_all_with_base(&base)
}

pub fn archive_all_with_base(base: &Path) -> Result<(), RecallError> {
    let conversations_dir = base.join("conversations");
    if !conversations_dir.exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run `recall-echo init` first.".into(),
        ));
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
                eprintln!("recall-echo: skipping {session_id} \u{2014} {e}");
            }
        }
    }

    eprintln!(
        "recall-echo: archived {archived_count} conversation{}, skipped {skipped_count} already archived",
        if archived_count == 1 { "" } else { "s" }
    );

    Ok(())
}

/// Session ids that already have an archive in `conversations/`.
///
/// The archives themselves are the record of what has been captured — there is
/// no separate ledger to fall out of step with them. Every importer checks this
/// set before parsing anything, which is what makes re-running an import safe.
#[must_use]
pub fn collect_archived_sessions(conversations_dir: &Path) -> std::collections::HashSet<String> {
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
    messages: &[pulse_system_types::llm::Message],
    metadata: &SessionMetadata,
    provider: Option<&dyn pulse_system_types::llm::LmProvider>,
) -> Result<u32, RecallError> {
    let mut conv = crate::pulse_null::messages_to_conversation(messages, &metadata.session_id);
    conv.first_timestamp = metadata.started_at.clone();
    conv.last_timestamp = metadata.ended_at.clone();

    let summary = summarize::extract_with_fallback(provider, &conv).await;
    let result = archive_conversation(memory_dir, &conv, &summary, "session")?;
    let log_number = result.log_number;

    // Graph ingestion (async path — no need for Runtime)
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
        pipeline_sync_on_archive_async(memory_dir).await;
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

    #[test]
    fn hook_missing_transcript_exits_ok() {
        let hook_input = crate::jsonl::HookInput {
            session_id: "no-persist".into(),
            transcript_path: "/nonexistent/path/transcript.jsonl".into(),
            _cwd: None,
            _hook_event_name: None,
        };
        // --no-session-persistence sessions have no transcript: must be Ok, not Err.
        assert!(run_with_hook_input(&hook_input, Path::new("/nonexistent-base")).is_ok());
    }

    /// A payload from a harness that spells its fields differently must not
    /// fail the session it is attached to.
    #[test]
    fn hook_without_a_transcript_path_exits_ok() {
        let hook_input = crate::jsonl::HookInput::default();
        assert!(run_with_hook_input(&hook_input, Path::new("/nonexistent-base")).is_ok());
        assert!(matches!(classify("", ""), HookTarget::Unnamed));
    }

    fn written(name: &str, body: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, body).unwrap();
        let path = path.to_string_lossy().to_string();
        (dir, path)
    }

    const CLAUDE_JSONL: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        "\n",
    );

    /// The format recall-echo has always read, still read the same way.
    #[test]
    fn a_claude_code_transcript_is_recognised() {
        let (_dir, path) = written("session.jsonl", CLAUDE_JSONL);
        assert!(matches!(classify(&path, "sess"), HookTarget::ClaudeJsonl));
    }

    /// The defect: `gemini hooks migrate --from-claude` points this command at
    /// a JSON *document*, which the JSON Lines parser reads as nothing at all.
    #[test]
    fn a_gemini_session_document_is_read_rather_than_skipped() {
        let document = serde_json::json!({
            "sessionId": "sess-1",
            "startTime": "2026-08-06T10:00:00Z",
            "messages": [
                {"type": "user", "content": "Where does recall-echo live?"},
                {"type": "gemini", "content": "Under /opt/recall-echo."},
            ],
        });

        // Pretty-printed and single-line are the same document.
        for body in [
            serde_json::to_string_pretty(&document).unwrap(),
            serde_json::to_string(&document).unwrap(),
        ] {
            let (_dir, path) = written("session-1.json", &body);
            let HookTarget::GeminiSession(conv) = classify(&path, "hook-session") else {
                panic!("not recognised as a Gemini session: {body}");
            };
            assert_eq!(conv.user_message_count, 1);
            assert_eq!(conv.assistant_message_count, 1);
        }
    }

    /// Anything else is declined by name, not archived as an empty session.
    #[test]
    fn a_transcript_in_no_known_format_is_declined() {
        let (_dir, path) = written("notes.txt", "not a transcript at all\n");
        assert!(matches!(classify(&path, "sess"), HookTarget::Unreadable));

        let (_dir, path) = written("empty.jsonl", "");
        assert!(matches!(classify(&path, "sess"), HookTarget::Unreadable));

        let hook_input = crate::jsonl::HookInput {
            session_id: "sess".into(),
            transcript_path: path,
            _cwd: None,
            _hook_event_name: None,
        };
        assert!(
            run_with_hook_input(&hook_input, Path::new("/nonexistent-base")).is_ok(),
            "an unreadable transcript must not fail the session"
        );
    }
}
