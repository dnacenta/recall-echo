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
//! updates ARCHIVE.md, appends to EPHEMERAL.md, and writes a turns sidecar.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config;
use crate::conversation::{self, Conversation, ConversationEntry};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
use crate::graph::v2::input::TurnRole;
use crate::summarize;
use crate::tags;
use crate::transcript::{ClaudeCodeTranscripts, Transcript};

/// Session metadata provided by the caller.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// The pulse whose session this is, written as the archive's `pulse:`
    /// frontmatter key. Empty when unknown.
    pub pulse_name: String,
}

/// Result of archiving a conversation — used by callers for graph ingestion.
pub struct ArchiveResult {
    pub log_number: u32,
    pub full_content: String,
    pub session_id: String,
}

impl ArchiveResult {
    /// Empty content means an empty session or a session already archived
    /// under `log_number`, so there is nothing to ingest.
    #[must_use]
    pub fn is_new_archive(&self) -> bool {
        self.log_number > 0 && !self.full_content.is_empty()
    }
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
    write_archive(memory_dir, conv, summary, source, None, None)
}

/// [`archive_conversation`] with the session's working directory.
///
/// `cwd` is stored on the turns sidecar only. It is not a field of
/// [`Conversation`].
pub(crate) fn archive_conversation_with_cwd(
    memory_dir: &Path,
    conv: &Conversation,
    summary: &summarize::ConversationSummary,
    source: &str,
    cwd: Option<&str>,
) -> Result<ArchiveResult, RecallError> {
    write_archive(memory_dir, conv, summary, source, None, cwd)
}

/// [`archive_conversation`] for a named pulse: the archive's frontmatter
/// records it as `pulse: "<name>"`.
pub fn archive_pulse_conversation(
    memory_dir: &Path,
    conv: &Conversation,
    summary: &summarize::ConversationSummary,
    source: &str,
    pulse_name: &str,
) -> Result<ArchiveResult, RecallError> {
    let pulse = Some(pulse_name).filter(|name| !name.trim().is_empty());
    write_archive(memory_dir, conv, summary, source, pulse, None)
}

/// The `## Summary` block, or nothing when the caller has no summary text.
fn summary_section(summary: &summarize::ConversationSummary) -> String {
    if summary.summary.is_empty() {
        return String::new();
    }

    let mut section = format!("## Summary\n\n{}\n\n", summary.summary);
    if !summary.decisions.is_empty() {
        section.push_str("**Decisions**:\n");
        for decision in &summary.decisions {
            let _ = writeln!(section, "- {decision}");
        }
        section.push('\n');
    }
    if !summary.action_items.is_empty() {
        section.push_str("**Action Items**:\n");
        for item in &summary.action_items {
            let _ = writeln!(section, "- {item}");
        }
        section.push('\n');
    }
    section
}

/// Paths and fields the archive record is written from.
///
/// The conversation file is already on disk; `full_content` is what was written.
struct ArchiveTail<'a> {
    memory_dir: &'a Path,
    archive_index: &'a Path,
    ephemeral_path: &'a Path,
    next_num: u32,
    date: &'a str,
    timestamp: String,
    duration: String,
    total_messages: u32,
    conv: &'a Conversation,
    summary: &'a summarize::ConversationSummary,
    cwd: Option<&'a str>,
    full_content: &'a str,
}

/// Write the turns sidecar, the ARCHIVE.md row and the EPHEMERAL entry, in
/// that order. The conversation file was claimed already.
///
/// The sidecar is a second record. A failure here must not throw away the
/// archive that was just written.
fn record_archive(tail: ArchiveTail<'_>) -> Result<(), RecallError> {
    // Already written by `claim_archive_number`. Kept on the tail so the
    // caller still hands the bytes that landed on disk.
    let _ = tail.full_content;
    write_sidecar_best_effort(
        tail.memory_dir,
        tail.next_num,
        &tail.conv.session_id,
        tail.date,
        tail.cwd,
        &tail.conv.entries,
    );

    // Append to ARCHIVE.md index
    append_index(
        tail.archive_index,
        tail.next_num,
        tail.date,
        &tail.conv.session_id,
        &tail.summary.topics,
        tail.total_messages,
        &tail.duration,
    )?;

    // Append to EPHEMERAL.md
    let entry = EphemeralEntry {
        session_id: tail.conv.session_id.clone(),
        date: tail.timestamp,
        duration: tail.duration,
        message_count: tail.total_messages,
        archive_file: format!("conversation-{:03}.md", tail.next_num),
        summary: tail.summary.summary.clone(),
    };
    ephemeral::append_entry(tail.ephemeral_path, &entry)?;
    let cfg = config::load_from_dir(tail.memory_dir);
    ephemeral::trim_to_limit(tail.ephemeral_path, cfg.ephemeral.max_entries)?;
    Ok(())
}

/// Sources written by a capture. A checkpoint trigger is never one of these.
const CAPTURE_SOURCES: [&str; 6] = ["jsonl", "gemini", "claude-code", "codex", "grok", "session"];

/// The highest earlier capture of `session_id`. An empty id, a checkpoint,
/// and an unreadable header are not captures.
fn prior_capture(conversations_dir: &Path, session_id: &str) -> Option<u32> {
    if session_id.is_empty() {
        return None;
    }
    let mut best: Option<u32> = None;
    let entries = fs::read_dir(conversations_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(num_str) = name
            .strip_prefix("conversation-")
            .and_then(|rest| rest.strip_suffix(".md"))
        else {
            continue;
        };
        let Ok(number) = num_str.parse::<u32>() else {
            continue;
        };
        let Some(header) = read_header(&entry.path()) else {
            continue;
        };
        if header.session_id == session_id && CAPTURE_SOURCES.contains(&header.source.as_str()) {
            best = Some(best.map_or(number, |current| current.max(number)));
        }
    }
    best
}

/// Frontmatter from the opening block. At most 64 lines are read.
pub(crate) fn read_header(path: &Path) -> Option<Frontmatter> {
    let file = fs::File::open(path).ok()?;
    let mut header = String::new();
    let mut markers = 0u32;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        if index >= 64 {
            break;
        }
        let line = line.ok()?;
        header.push_str(&line);
        header.push('\n');
        if line.trim() == "---" {
            markers += 1;
            if markers == 2 {
                break;
            }
        }
    }
    crate::frontmatter::parse(&header)
}

/// Turns from the sidecar, when that file exists and names `session_id`.
fn stored_turns(memory_dir: &Path, log: u32, session_id: &str) -> Option<Vec<(TurnRole, String)>> {
    let path = crate::graph::v2::input::turns_sidecar(memory_dir, log);
    if !path.is_file() {
        return None;
    }
    // `load_session` reads the markdown when the sidecar names another session.
    // A repeat still requires the sidecar file itself to name this session.
    if crate::graph::v2::input::sidecar_session_id(&path).as_deref() != Some(session_id) {
        return None;
    }
    let session = crate::graph::v2::input::load_session(memory_dir, log).ok()?;
    if session.session_id != session_id {
        return None;
    }
    Some(
        session
            .turns
            .into_iter()
            .map(|turn| (turn.role, turn.text))
            .collect(),
    )
}

/// User and assistant text, in order. Empty text and tool entries are skipped.
fn text_turns(entries: &[ConversationEntry]) -> Vec<(TurnRole, &str)> {
    let mut turns = Vec::new();
    for entry in entries {
        let (role, text) = match entry {
            ConversationEntry::UserMessage(text) => (TurnRole::User, text.as_str()),
            ConversationEntry::AssistantText(text) => (TurnRole::Assistant, text.as_str()),
            ConversationEntry::ToolUse { .. } | ConversationEntry::ToolResult { .. } => continue,
        };
        if text.is_empty() {
            continue;
        }
        turns.push((role, text));
    }
    turns
}

/// True when `incoming` is a non-empty exact prefix of `stored`, role and text.
fn is_repeat(incoming: &[(TurnRole, &str)], stored: &[(TurnRole, String)]) -> bool {
    if incoming.is_empty() || incoming.len() > stored.len() {
        return false;
    }
    for (index, turn) in incoming.iter().enumerate() {
        let Some(kept) = stored.get(index) else {
            return false;
        };
        if turn.0 != kept.0 || turn.1 != kept.1 {
            return false;
        }
    }
    true
}

/// The earlier capture to return when `conv` repeats it.
fn repeat_of(memory_dir: &Path, conv: &Conversation) -> Option<u32> {
    let log = prior_capture(&memory_dir.join("conversations"), &conv.session_id)?;
    let stored = stored_turns(memory_dir, log, &conv.session_id)?;
    is_repeat(&text_turns(&conv.entries), &stored).then_some(log)
}

static CLAIM_SEQ: AtomicU64 = AtomicU64::new(0);
const CLAIM_ATTEMPTS: u32 = 64;

struct ClaimFile {
    path: PathBuf,
}

impl ClaimFile {
    fn new(dir: &Path) -> Self {
        let path = dir.join(format!(
            ".claim-{}-{}.tmp",
            std::process::id(),
            CLAIM_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        Self { path }
    }
}

impl Drop for ClaimFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Claim the next free conversation number and write `render(n)` there.
///
/// `render` is called again with n+1 when another writer took n.
pub(crate) fn claim_archive_number(
    conversations_dir: &Path,
    render: impl FnMut(u32) -> String,
) -> Result<(u32, String), RecallError> {
    claim_archive_number_in(conversations_dir, render, link_claim)
}

fn link_claim(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    fs::hard_link(tmp, dest)
}

fn claim_archive_number_in(
    conversations_dir: &Path,
    mut render: impl FnMut(u32) -> String,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(u32, String), RecallError> {
    let mut number = highest_conversation_number(conversations_dir) + 1;
    let tmp = ClaimFile::new(conversations_dir);
    for _ in 0..CLAIM_ATTEMPTS {
        let content = render(number);
        fs::write(&tmp.path, &content)?;
        let dest = conversation_file(conversations_dir, number);
        match place_claim(&tmp.path, &dest, &content, &link) {
            Ok(()) => return Ok((number, content)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
            Err(err) => return Err(err.into()),
        }
    }
    Err(RecallError::Other(
        "could not claim a free conversation number after 64 attempts".into(),
    ))
}

fn conversation_file(dir: &Path, number: u32) -> PathBuf {
    dir.join(format!("conversation-{number:03}.md"))
}

/// The link step. A test passes a linker that fails so the fallback can run.
fn place_claim(
    tmp: &Path,
    dest: &Path,
    content: &str,
    link: &impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match link(tmp, dest) {
        Ok(()) => Ok(()),
        Err(err) if hard_link_unavailable(&err) => reserve_new_file(dest, content),
        Err(err) => Err(err),
    }
}

fn hard_link_unavailable(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::Unsupported || is_cross_device(err)
}

fn is_cross_device(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::CrossesDevices || err.raw_os_error() == Some(libc::EXDEV)
}

fn reserve_new_file(dest: &Path, content: &str) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    file.write_all(content.as_bytes())
}

struct ArchiveRender<'a> {
    conv: &'a Conversation,
    summary: &'a summarize::ConversationSummary,
    source: &'a str,
    pulse: Option<&'a str>,
    timestamp: &'a str,
    duration: &'a str,
    total_messages: u32,
}

impl ArchiveRender<'_> {
    fn render(&self, number: u32) -> String {
        let fm = Frontmatter {
            log: number,
            date: self.timestamp.to_string(),
            session_id: self.conv.session_id.clone(),
            message_count: self.total_messages,
            duration: self.duration.to_string(),
            source: self.source.to_string(),
            pulse: self.pulse.map(str::to_string),
            topics: self.summary.topics.clone(),
        };
        let md_body = conversation::conversation_to_markdown(self.conv, number);
        let conv_tags = tags::extract_tags(&self.conv.entries);
        let tags_section = tags::format_tags_section(&conv_tags);
        let summary_section = summary_section(self.summary);
        format!(
            "{}\n\n{}{}\n{}",
            fm.render(),
            summary_section,
            md_body,
            tags_section
        )
    }
}

fn write_archive(
    memory_dir: &Path,
    conv: &Conversation,
    summary: &summarize::ConversationSummary,
    source: &str,
    pulse: Option<&str>,
    cwd: Option<&str>,
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

    if let Some(log) = repeat_of(memory_dir, conv) {
        eprintln!(
            "recall-echo: conversation-{log:03}.md already holds session {}, nothing new to archive",
            conv.session_id
        );
        return Ok(ArchiveResult {
            log_number: log,
            full_content: String::new(),
            session_id: conv.session_id.clone(),
        });
    }

    let (timestamp, date) = session_clock(conv.first_timestamp.as_deref());
    let duration = match (&conv.first_timestamp, &conv.last_timestamp) {
        (Some(start), Some(end)) => conversation::calculate_duration(start, end),
        _ => "unknown".to_string(),
    };
    let total_messages = conv.total_messages();
    let rendered = ArchiveRender {
        conv,
        summary,
        source,
        pulse,
        timestamp: &timestamp,
        duration: &duration,
        total_messages,
    };
    let (next_num, full_content) =
        claim_archive_number(&conversations_dir, |number| rendered.render(number))?;

    record_archive(ArchiveTail {
        memory_dir,
        archive_index: &archive_index,
        ephemeral_path: &ephemeral_path,
        next_num,
        date: &date,
        timestamp,
        duration,
        total_messages,
        conv,
        summary,
        cwd,
        full_content: &full_content,
    })?;

    eprintln!("recall-echo: archived conversation-{next_num:03}.md ({total_messages} messages)");

    Ok(ArchiveResult {
        log_number: next_num,
        full_content,
        session_id: conv.session_id.clone(),
    })
}

/// The archive's clock: the session's first timestamp when it is RFC 3339,
/// otherwise the moment of import. The second value is the calendar day for
/// the ARCHIVE.md column and the sidecar.
///
/// Chrono accepts a space or a lowercase `t` as the date/time separator. The
/// day comes from the parsed value, so those forms still yield `YYYY-MM-DD`.
/// An offset keeps that offset's calendar day rather than converting to UTC.
fn session_clock(first: Option<&str>) -> (String, String) {
    if let Some(ts) = first {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts) {
            return (ts.to_string(), parsed.date_naive().to_string());
        }
    }
    let now = conversation::utc_now();
    let day = conversation::date_from_timestamp(&now);
    (now, day)
}

/// Hook cwd, else the first transcript line that carries one, else the cwd
/// discovery already recorded on the transcript.
#[must_use]
pub(crate) fn resolve_cwd(
    hook_cwd: Option<&str>,
    transcript: Option<&Path>,
    recorded_cwd: Option<&str>,
) -> Option<String> {
    nonempty(hook_cwd)
        .or_else(|| transcript.and_then(first_cwd_in_file))
        .or_else(|| nonempty(recorded_cwd))
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn first_cwd_in_file(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = cwd_on_line(&value) {
            return Some(cwd);
        }
    }
    None
}

fn cwd_on_line(value: &serde_json::Value) -> Option<String> {
    if let Some(cwd) = string_field(value, "cwd") {
        return Some(cwd);
    }
    let object = value.as_object()?;
    for child in object.values() {
        if let Some(cwd) = string_field(child, "cwd") {
            return Some(cwd);
        }
    }
    None
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

pub(crate) fn write_sidecar_best_effort(
    memory_dir: &Path,
    log_number: u32,
    session_id: &str,
    date: &str,
    cwd: Option<&str>,
    entries: &[conversation::ConversationEntry],
) {
    if let Err(err) = crate::graph::v2::input::write_turns_sidecar(
        memory_dir, log_number, session_id, date, cwd, entries,
    ) {
        eprintln!(
            "recall-echo: conversation-{log_number:03} archived; turns sidecar not written: {err}"
        );
    }
}

/// Ingest an archive result into the knowledge graph.
pub fn graph_ingest(memory_dir: &Path, result: &ArchiveResult) {
    if !result.is_new_archive() {
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
    archive_jsonl(base_dir, session_id, transcript_path, None, None)
}

fn archive_jsonl(
    base_dir: &Path,
    session_id: &str,
    transcript_path: &str,
    hook_cwd: Option<&str>,
    recorded_cwd: Option<&str>,
) -> Result<u32, RecallError> {
    let conv = crate::jsonl::parse_transcript(transcript_path, session_id)?;
    let cwd = resolve_cwd(hook_cwd, Some(Path::new(transcript_path)), recorded_cwd);
    archive_and_ingest(base_dir, &conv, "jsonl", cwd.as_deref())
}

/// Summarise, archive, and hand the result to the graph.
///
/// The tail every input path shares, whichever CLI's transcript it started as.
fn archive_and_ingest(
    base_dir: &Path,
    conv: &Conversation,
    source: &str,
    cwd: Option<&str>,
) -> Result<u32, RecallError> {
    let summary = summarize::algorithmic_summary(conv);
    let result = archive_conversation_with_cwd(base_dir, conv, &summary, source, cwd)?;
    let log_number = result.log_number;

    if result.is_new_archive() {
        graph_ingest(base_dir, &result);
        pipeline_sync_on_archive(base_dir);
    }

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
    /// A Claude Code subagent's transcript — part of a session, not one.
    Subagent,
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
    if crate::jsonl::is_subagent_transcript(Path::new(transcript_path)) {
        return HookTarget::Subagent;
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
pub fn run_from_hook(pulse_root: Option<&Path>) -> Result<(), RecallError> {
    let hook_input = crate::jsonl::read_hook_input()?;
    let base_dir = crate::paths::resolved_hook_base_dir(pulse_root)?;
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
        HookTarget::Subagent => {
            crate::jsonl::report_declined_subagent(transcript_path, "nothing archived");
        }
        HookTarget::ClaudeJsonl => {
            archive_jsonl(
                base_dir,
                &hook_input.session_id,
                transcript_path,
                hook_input._cwd.as_deref(),
                None,
            )?;
        }
        HookTarget::GeminiSession(conv) => {
            let cwd = resolve_cwd(
                hook_input._cwd.as_deref(),
                Some(Path::new(transcript_path)),
                None,
            );
            archive_and_ingest(base_dir, &conv, "gemini", cwd.as_deref())?;
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

/// [`archive_all_unarchived_at`] with no `--pulse-root`.
pub fn archive_all_unarchived() -> Result<(), RecallError> {
    archive_all_unarchived_at(None)
}

/// Archive every unarchived Claude Code session under `~/.claude/projects/`.
///
/// The store is `pulse_root` when given, else `RECALL_ECHO_HOME`, else the
/// root `init` persisted, else the legacy `~/.claude` — never the current
/// directory, which the hooks consult but a batch import must not
/// (`paths::batch_archive_base_dir`). The chosen store and why are
/// printed before anything is written.
pub fn archive_all_unarchived_at(pulse_root: Option<&Path>) -> Result<(), RecallError> {
    let (memory_dir, why) = crate::paths::batch_archive_base_dir(pulse_root)?;
    // `{:?}` escapes control characters: the path can come from a file.
    eprintln!("recall-echo: archiving into {memory_dir:?} ({why})");
    let transcripts = ClaudeCodeTranscripts::detect()
        .ok_or_else(|| RecallError::Other("Could not determine home directory".into()))?;
    archive_all_with(&memory_dir, &transcripts).map(|_| ())
}

/// [`archive_all_with`] for a claude-style root that is both the store and
/// the parent of `projects/`.
#[deprecated(
    since = "4.7.0",
    note = "use `archive_all_with`, which takes the store and the transcripts separately"
)]
pub fn archive_all_with_base(base: &Path) -> Result<(), RecallError> {
    archive_all_with(base, &ClaudeCodeTranscripts::new(base.join("projects"))).map(|_| ())
}

/// What [`archive_all_with`] did — the numbers its summary line prints.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BatchArchiveReport {
    /// Sessions archived by this run.
    pub archived: u32,
    /// Sessions skipped because `conversations/` already holds them.
    pub already_archived: u32,
    /// Subagent runs set aside, by layout or by their `isSidechain` flag.
    pub subagents_skipped: u32,
}

impl BatchArchiveReport {
    /// The line `archive --all-unarchived` ends on.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "archived {} conversation{}, skipped {} already archived and {} subagent run{}",
            self.archived,
            if self.archived == 1 { "" } else { "s" },
            self.already_archived,
            self.subagents_skipped,
            if self.subagents_skipped == 1 { "" } else { "s" }
        )
    }
}

/// Archive every session `transcripts` discovers that `memory_dir` does not
/// hold yet — oldest first, no settle time, no watermark.
///
/// Discovery is the capture sweep's, so subagent transcripts are never
/// mistaken for sessions here either. The summary is printed to stderr and
/// returned.
pub fn archive_all_with(
    memory_dir: &Path,
    transcripts: &ClaudeCodeTranscripts,
) -> Result<BatchArchiveReport, RecallError> {
    let conversations_dir = memory_dir.join("conversations");
    if !conversations_dir.exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run `recall-echo init` first.".into(),
        ));
    }

    let archived_sessions = collect_captured_sessions(&conversations_dir);

    if !transcripts.is_installed() {
        eprintln!("No projects directory found \u{2014} nothing to archive.");
        return Ok(BatchArchiveReport::default());
    }

    let (found, by_layout) = transcripts.discover_with_skipped(None)?;
    let mut report = BatchArchiveReport {
        subagents_skipped: by_layout,
        ..BatchArchiveReport::default()
    };
    for transcript in found {
        let session_id = &transcript.session_id;
        if archived_sessions.contains(session_id) {
            report.already_archived += 1;
            continue;
        }
        // Read only after the archived check, as the capture sweep does.
        if transcripts.is_sub_run(&transcript) {
            crate::jsonl::debug_skipped_subagent(&transcript.path, "isSidechain flag");
            report.subagents_skipped += 1;
            continue;
        }

        let path_str = transcript.path.to_string_lossy().to_string();
        match archive_jsonl(
            memory_dir,
            session_id,
            &path_str,
            None,
            transcript.cwd.as_deref(),
        ) {
            Ok(_) => report.archived += 1,
            Err(e) => {
                eprintln!("recall-echo: skipping {session_id} \u{2014} {e}");
            }
        }
    }

    eprintln!("recall-echo: {}", report.summary());
    Ok(report)
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

/// Session ids that have a real capture in `conversations/`.
///
/// Checkpoints never count. A file counts only when [`read_header`] yields a
/// non-empty `session_id` whose `source` is in [`CAPTURE_SOURCES`].
pub(crate) fn collect_captured_sessions(
    conversations_dir: &Path,
) -> std::collections::HashSet<String> {
    let mut sessions = std::collections::HashSet::new();
    let Ok(entries) = fs::read_dir(conversations_dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_conversation_markdown(&name) {
            continue;
        }
        let Some(header) = read_header(&entry.path()) else {
            continue;
        };
        if header.session_id.is_empty() {
            continue;
        }
        if CAPTURE_SOURCES.contains(&header.source.as_str()) {
            sessions.insert(header.session_id);
        }
    }
    sessions
}

fn is_conversation_markdown(name: &str) -> bool {
    name.starts_with("conversation-") && name.ends_with(".md")
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
    let result =
        archive_pulse_conversation(memory_dir, &conv, &summary, "session", &metadata.pulse_name)?;
    let log_number = result.log_number;

    // Graph ingestion (async path — no need for Runtime)
    if result.is_new_archive() {
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

    /// Hooks ingest episodes. They do not extract, whatever `[graph] pipeline`
    /// says: no provider call, no admin lock, and the daemon is not stopped.
    #[test]
    fn the_archive_hook_does_not_extract() {
        let source = include_str!("archive.rs");
        let production = source.split("mod tests").next().expect("tests");
        assert!(
            !production.contains("extract_log"),
            "an archive hook must not run extraction"
        );
        assert!(
            !production.contains("serve_client::exclusive"),
            "an archive hook must not take the admin lock"
        );
        assert!(
            !production.contains("create_provider"),
            "an archive hook must not call the model"
        );
        assert!(production.contains("ingest_into_graph"));

        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".recall-echo.toml"),
            "[graph]\npipeline = \"v2\"\n",
        )
        .unwrap();
        graph_ingest(
            tmp.path(),
            &ArchiveResult {
                log_number: 3,
                full_content: "### User\n\nShip the shelf.\n".into(),
                session_id: "s".into(),
            },
        );
        assert!(!tmp.path().join("graph").exists());
    }

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
    fn a_pulse_archive_records_the_pulse_in_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        fs::create_dir_all(memory.join("conversations")).unwrap();
        let mut conv = Conversation::new("pulse-session");
        conv.user_message_count = 1;
        conv.entries
            .push(conversation::ConversationEntry::UserMessage(
                "hi".to_string(),
            ));
        let summary = summarize::ConversationSummary::default();

        archive_pulse_conversation(memory, &conv, &summary, "session", "Echo").unwrap();
        let mut plain = conv.clone();
        plain.session_id = "plain-session".to_string();
        archive_conversation(memory, &plain, &summary, "jsonl").unwrap();

        let named = fs::read_to_string(memory.join("conversations/conversation-001.md")).unwrap();
        assert!(named.contains("\npulse: \"Echo\"\n"), "{named}");
        assert!(!named.contains("entity:"), "{named}");
        let parsed = crate::frontmatter::parse(&named).unwrap();
        assert_eq!(parsed.pulse.as_deref(), Some("Echo"));

        let anonymous =
            fs::read_to_string(memory.join("conversations/conversation-002.md")).unwrap();
        assert!(!anonymous.contains("pulse:"), "{anonymous}");
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

    /// Where Claude Code writes a subagent's run, and a fork's run moved
    /// anywhere else: both are declined by the hook (RE-74).
    #[test]
    fn classify_subagent_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let subagents = dir.path().join("sess/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let by_path = subagents.join("agent-a1.jsonl");
        fs::write(&by_path, CLAUDE_JSONL).unwrap();
        assert!(matches!(
            classify(&by_path.to_string_lossy(), "agent-a1"),
            HookTarget::Subagent
        ));

        let fork = concat!(
            r#"{"type":"fork-context-ref","agentId":"a2"}"#,
            "\n",
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"hi"}}"#,
            "\n",
        );
        let (_dir, by_flag) = written("agent-a2.jsonl", fork);
        assert!(matches!(
            classify(&by_flag, "agent-a2"),
            HookTarget::Subagent
        ));
    }

    #[test]
    fn hook_on_subagent_transcript_archives_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        let subagents = tmp.path().join("projects/-p/sess/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let path = subagents.join("agent-a1.jsonl");
        fs::write(&path, CLAUDE_JSONL).unwrap();

        let hook_input = crate::jsonl::HookInput {
            session_id: "agent-a1".into(),
            transcript_path: path.to_string_lossy().to_string(),
            _cwd: None,
            _hook_event_name: Some("SessionEnd".into()),
        };
        assert!(run_with_hook_input(&hook_input, &memory).is_ok());
        assert_eq!(
            fs::read_dir(memory.join("conversations")).unwrap().count(),
            0
        );
        assert!(!memory.join("ARCHIVE.md").exists());
        assert!(!memory.join("EPHEMERAL.md").exists());
    }

    /// RE-74 audit C3: a fork moved beside its parent is caught by the
    /// `is_sub_run` read, not the layout, and both runs are counted.
    #[test]
    fn archive_all_skips_a_fork_beside_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".claude/projects/-home-dev-app");
        fs::create_dir_all(project.join("sess-1/subagents")).unwrap();
        fs::write(project.join("sess-1.jsonl"), CLAUDE_JSONL).unwrap();
        fs::write(
            project.join("sess-1/subagents/agent-a1.jsonl"),
            CLAUDE_JSONL,
        )
        .unwrap();
        let fork = concat!(
            r#"{"type":"fork-context-ref","agentId":"a2","parentSessionId":"sess-1"}"#,
            "\n",
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"worker prompt"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
        );
        fs::write(project.join("agent-a2.jsonl"), fork).unwrap();
        let memory = tmp.path().join("root/memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();

        let transcripts = ClaudeCodeTranscripts::new(tmp.path().join(".claude/projects"));
        let report = archive_all_with(&memory, &transcripts).unwrap();

        let archived = collect_archived_sessions(&memory.join("conversations"));
        assert_eq!(archived.len(), 1, "{archived:?}");
        assert!(archived.contains("sess-1"));
        assert!(!archived.contains("agent-a2"));
        assert_eq!(report.archived, 1);
        assert_eq!(report.subagents_skipped, 2, "one by layout, one by flag");
        assert!(
            report.summary().ends_with("and 2 subagent runs"),
            "{}",
            report.summary()
        );
    }

    /// `archive --all-unarchived` writes into the store it is given — never a
    /// `conversations/` beside `projects/` — and skips subagents like the
    /// capture sweep does (RE-74).
    #[test]
    fn archive_all_writes_into_memory_store_and_skips_subagents() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let project = claude.join("projects/-home-dev-app");
        fs::create_dir_all(project.join("sess-1/subagents")).unwrap();
        fs::write(project.join("sess-1.jsonl"), CLAUDE_JSONL).unwrap();
        fs::write(
            project.join("sess-1/subagents/agent-a1.jsonl"),
            CLAUDE_JSONL,
        )
        .unwrap();
        let memory = tmp.path().join("root/memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        // A legacy store beside `projects/` — where 4.6.2 wrote.
        fs::create_dir_all(claude.join("conversations")).unwrap();

        let transcripts = ClaudeCodeTranscripts::new(claude.join("projects"));
        archive_all_with(&memory, &transcripts).unwrap();

        let archived = collect_archived_sessions(&memory.join("conversations"));
        assert_eq!(archived.len(), 1, "{archived:?}");
        assert!(archived.contains("sess-1"));
        assert_eq!(
            fs::read_dir(claude.join("conversations")).unwrap().count(),
            0,
            "nothing may land in the legacy store beside projects/"
        );

        // Re-running finds nothing new.
        archive_all_with(&memory, &transcripts).unwrap();
        assert_eq!(
            fs::read_dir(memory.join("conversations")).unwrap().count(),
            1
        );
    }

    fn memory_with_conversations() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("conversations")).unwrap();
        tmp
    }

    fn one_turn(session_id: &str, first: Option<&str>) -> Conversation {
        Conversation {
            session_id: session_id.to_string(),
            first_timestamp: first.map(str::to_string),
            last_timestamp: first.map(str::to_string),
            user_message_count: 1,
            assistant_message_count: 0,
            entries: vec![conversation::ConversationEntry::UserMessage(
                "hello".to_string(),
            )],
        }
    }

    #[test]
    fn archive_date_is_session_start() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let summary = summarize::ConversationSummary::default();
        let conv = one_turn("sess", Some("2026-09-28T09:10:37Z"));

        archive_conversation(memory, &conv, &summary, "jsonl").unwrap();

        let archived =
            fs::read_to_string(memory.join("conversations/conversation-001.md")).unwrap();
        let frontmatter = crate::frontmatter::parse(&archived).unwrap();
        assert_eq!(frontmatter.date, "2026-09-28T09:10:37Z");
        let index = fs::read_to_string(memory.join("ARCHIVE.md")).unwrap();
        assert!(index.contains("| 001 | 2026-09-28 | sess |"), "{index}");
        let ephemeral = fs::read_to_string(memory.join("EPHEMERAL.md")).unwrap();
        assert!(ephemeral.contains("2026-09-28"), "{ephemeral}");
        assert!(!ephemeral.contains("2026-10-23"), "{ephemeral}");

        let fractional = one_turn("frac", Some("2026-09-28T09:10:37.000Z"));
        archive_conversation(memory, &fractional, &summary, "jsonl").unwrap();
        let second = fs::read_to_string(memory.join("conversations/conversation-002.md")).unwrap();
        assert!(
            second.contains("date: \"2026-09-28T09:10:37.000Z\""),
            "{second}"
        );
        let index = fs::read_to_string(memory.join("ARCHIVE.md")).unwrap();
        assert!(index.contains("| 002 | 2026-09-28 | frac |"), "{index}");

        let before = conversation::utc_now();
        let invalid = one_turn("bad", Some("not-a-timestamp"));
        archive_conversation(memory, &invalid, &summary, "jsonl").unwrap();
        let third = crate::frontmatter::parse(
            &fs::read_to_string(memory.join("conversations/conversation-003.md")).unwrap(),
        )
        .unwrap();
        assert_ne!(third.date, "not-a-timestamp");
        assert!(chrono::DateTime::parse_from_rfc3339(&third.date).is_ok());
        let day = &third.date[..10];
        let after = conversation::utc_now();
        assert!(
            day == &before[..10] || day == &after[..10],
            "fell back to {day}, import was {before}..{after}"
        );
    }

    #[test]
    fn archive_date_normalises_space_and_lowercase_separator() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let summary = summarize::ConversationSummary::default();

        archive_conversation(
            memory,
            &one_turn("space", Some("2026-09-28 09:10:37Z")),
            &summary,
            "jsonl",
        )
        .unwrap();
        archive_conversation(
            memory,
            &one_turn("lower", Some("2026-09-29t09:10:37Z")),
            &summary,
            "jsonl",
        )
        .unwrap();

        let index = fs::read_to_string(memory.join("ARCHIVE.md")).unwrap();
        assert!(index.contains("| 001 | 2026-09-28 | space |"), "{index}");
        assert!(index.contains("| 002 | 2026-09-29 | lower |"), "{index}");
        assert!(!index.contains("09:10:37"), "{index}");

        for (number, day) in [(1, "2026-09-28"), (2, "2026-09-29")] {
            let raw =
                fs::read_to_string(memory.join(format!("turns/conversation-{number:03}.json")))
                    .unwrap();
            let sidecar: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(sidecar["date"], day, "{raw}");
        }
    }

    #[test]
    fn sidecar_written_next_to_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let cwd = tmp.path().join("linkshelf");
        fs::create_dir(&cwd).unwrap();
        let conv = Conversation {
            session_id: "sess".into(),
            first_timestamp: Some("2026-09-28T09:10:37Z".into()),
            last_timestamp: Some("2026-09-28T09:16:47Z".into()),
            user_message_count: 2,
            assistant_message_count: 2,
            entries: vec![
                conversation::ConversationEntry::UserMessage("one".into()),
                conversation::ConversationEntry::UserMessage("two".into()),
                conversation::ConversationEntry::UserMessage(String::new()),
                conversation::ConversationEntry::AssistantText("on it".into()),
                conversation::ConversationEntry::ToolUse {
                    name: "Write".into(),
                    input_summary: "secret-input".into(),
                },
                conversation::ConversationEntry::ToolUse {
                    name: "Bash".into(),
                    input_summary: "rm -rf".into(),
                },
                conversation::ConversationEntry::ToolUse {
                    name: "Write".into(),
                    input_summary: "again".into(),
                },
                conversation::ConversationEntry::ToolResult {
                    content: "SECRET_OUTPUT".into(),
                    is_error: false,
                },
                conversation::ConversationEntry::AssistantText("done".into()),
                conversation::ConversationEntry::ToolUse {
                    name: "Read".into(),
                    input_summary: "notes.md".into(),
                },
            ],
        };
        archive_conversation_with_cwd(
            memory,
            &conv,
            &summarize::ConversationSummary::default(),
            "jsonl",
            Some(cwd.to_str().unwrap()),
        )
        .unwrap();

        let conversations = fs::read_dir(memory.join("conversations"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(conversations, vec!["conversation-001.md".to_string()]);

        let raw = fs::read_to_string(memory.join("turns/conversation-001.json")).unwrap();
        assert!(!raw.contains("secret-input"), "{raw}");
        assert!(!raw.contains("rm -rf"), "{raw}");
        assert!(!raw.contains("SECRET_OUTPUT"), "{raw}");
        assert!(!raw.contains("again"), "{raw}");
        assert!(!raw.contains("notes.md"), "{raw}");

        let sidecar: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(sidecar["version"], 1);
        assert_eq!(sidecar["session_id"], "sess");
        assert_eq!(sidecar["date"], "2026-09-28");
        assert_eq!(sidecar["cwd"], cwd.to_str().unwrap());
        assert_eq!(sidecar["project"]["name"], "linkshelf");
        assert_eq!(
            sidecar["project"]["aliases"],
            serde_json::json!(["linkshelf"])
        );
        assert_eq!(sidecar["turns"][0]["i"], 0);
        assert_eq!(sidecar["turns"][0]["role"], "user");
        assert_eq!(sidecar["turns"][0]["text"], "one");
        assert!(sidecar["turns"][0]["ts"].is_null());
        assert_eq!(sidecar["turns"][1]["text"], "two");
        assert_eq!(sidecar["turns"][2]["role"], "assistant");
        assert_eq!(sidecar["turns"][2]["text"], "on it");
        assert_eq!(sidecar["turns"][3]["text"], "done");
        assert_eq!(sidecar["turns"].as_array().unwrap().len(), 4);
        assert_eq!(
            sidecar["tools"],
            serde_json::json!([
                {"before_turn": 3, "names": ["Write", "Bash"]},
                {"before_turn": 4, "names": ["Read"]}
            ])
        );

        let loaded = crate::graph::v2::input::load_session(memory, 1).unwrap();
        assert_eq!(loaded.turns.len(), 4);
    }

    #[test]
    fn unwritable_turns_directory_does_not_fail_archiving() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        fs::write(memory.join("turns"), "not a directory").unwrap();
        let conv = one_turn("sess", Some("2026-09-28T09:10:37Z"));

        let result = archive_conversation(
            memory,
            &conv,
            &summarize::ConversationSummary::default(),
            "jsonl",
        )
        .unwrap();

        assert_eq!(result.log_number, 1);
        assert!(memory.join("conversations/conversation-001.md").exists());
        assert!(memory.join("ARCHIVE.md").exists());
        assert!(memory.join("EPHEMERAL.md").exists());
        assert!(memory.join("turns").is_file());
    }

    #[test]
    fn cwd_prefers_hook_then_transcript_line_then_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let line = tmp.path().join("line.jsonl");
        fs::write(&line, "{\"cwd\":\"/from/line\",\"type\":\"user\"}\n").unwrap();
        let nested = tmp.path().join("codex.jsonl");
        fs::write(
            &nested,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/from/payload\"}}\n",
        )
        .unwrap();
        let none = tmp.path().join("none.jsonl");
        fs::write(&none, "{\"type\":\"user\"}\n").unwrap();

        assert_eq!(
            resolve_cwd(Some("/from/hook"), Some(&line), Some("/from/ref")).as_deref(),
            Some("/from/hook")
        );
        assert_eq!(
            resolve_cwd(Some("  "), Some(&line), Some("/from/ref")).as_deref(),
            Some("/from/line")
        );
        assert_eq!(
            resolve_cwd(None, Some(&nested), Some("/from/ref")).as_deref(),
            Some("/from/payload")
        );
        assert_eq!(
            resolve_cwd(None, Some(&none), Some("/from/ref")).as_deref(),
            Some("/from/ref")
        );
        assert_eq!(resolve_cwd(None, None, None), None);
    }

    fn dialogue(session_id: &str, lines: &[(&str, &str)]) -> Conversation {
        let mut conv = Conversation::new(session_id);
        for (role, text) in lines {
            match *role {
                "user" => {
                    conv.user_message_count += 1;
                    conv.entries
                        .push(ConversationEntry::UserMessage((*text).to_string()));
                }
                "assistant" => {
                    conv.assistant_message_count += 1;
                    conv.entries
                        .push(ConversationEntry::AssistantText((*text).to_string()));
                }
                _ => unreachable!("role"),
            }
        }
        conv
    }

    fn archive(memory: &Path, conv: &Conversation, source: &str) -> ArchiveResult {
        archive_conversation(
            memory,
            conv,
            &summarize::ConversationSummary::default(),
            source,
        )
        .unwrap()
    }

    fn conversation_names(memory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(memory.join("conversations"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("conversation-") && name.ends_with(".md"))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn archive_data_rows(memory: &Path) -> usize {
        let path = memory.join("ARCHIVE.md");
        if !path.exists() {
            return 0;
        }
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| {
                line.starts_with("| ") && line.as_bytes().get(2).is_some_and(u8::is_ascii_digit)
            })
            .count()
    }

    fn sidecar_names(memory: &Path) -> Vec<String> {
        let dir = memory.join("turns");
        if !dir.is_dir() {
            return Vec::new();
        }
        let mut names = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn record_bytes(memory: &Path) -> Vec<Vec<u8>> {
        vec![
            fs::read(memory.join("conversations/conversation-001.md")).unwrap(),
            fs::read(memory.join("turns/conversation-001.json")).unwrap(),
            fs::read(memory.join("ARCHIVE.md")).unwrap(),
            fs::read(memory.join("EPHEMERAL.md")).unwrap(),
        ]
    }

    fn claude_transcript(dir: &Path) -> String {
        let path = dir.join("session.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"alpha"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    }

    fn hook_for(session: &str, path: &str) -> crate::jsonl::HookInput {
        crate::jsonl::HookInput {
            session_id: session.into(),
            transcript_path: path.into(),
            _cwd: None,
            _hook_event_name: Some("SessionEnd".into()),
        }
    }

    /// AC-1: the same capture twice writes one archive and returns it.
    #[test]
    fn a_repeat_capture_returns_the_existing_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = dialogue("sess", &[("user", "hello"), ("assistant", "alpha")]);
        assert!(archive(memory, &conv, "jsonl").is_new_archive());
        let before = record_bytes(memory);

        let second = archive(memory, &conv, "jsonl");

        assert_eq!(second.log_number, 1);
        assert!(!second.is_new_archive());
        assert!(second.full_content.is_empty());
        assert_eq!(record_bytes(memory), before);
        assert_eq!(
            conversation_names(memory),
            vec!["conversation-001.md".to_string()]
        );
        assert_eq!(archive_data_rows(memory), 1);
        assert_eq!(
            crate::ephemeral::count_entries(&memory.join("EPHEMERAL.md")).unwrap(),
            1
        );
        assert_eq!(
            sidecar_names(memory),
            vec!["conversation-001.json".to_string()]
        );
        assert!(!memory.join("graph").exists());
    }

    /// AC-2: the SessionEnd hook does not archive the same transcript twice.
    #[test]
    fn the_hook_archives_one_transcript_once() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let path = claude_transcript(memory);
        let hook = hook_for("sess", &path);
        run_with_hook_input(&hook, memory).unwrap();
        run_with_hook_input(&hook, memory).unwrap();
        assert_eq!(conversation_names(memory).len(), 1);
        assert_eq!(archive_data_rows(memory), 1);
        assert!(!memory.join("graph").exists());
    }

    /// AC-2: a sweep capture and the hook of the same file are one archive.
    #[test]
    fn a_sweep_capture_then_the_hook_archives_once() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let path = claude_transcript(memory);
        let conv = crate::jsonl::parse_transcript(&path, "sess").unwrap();
        archive(memory, &conv, "claude-code");
        run_with_hook_input(&hook_for("sess", &path), memory).unwrap();
        assert_eq!(
            conversation_names(memory),
            vec!["conversation-001.md".to_string()]
        );
        assert_eq!(archive_data_rows(memory), 1);
        let archived =
            fs::read_to_string(memory.join("conversations/conversation-001.md")).unwrap();
        assert!(archived.contains("source: \"claude-code\""), "{archived}");
        assert!(!memory.join("graph").exists());
    }

    /// AC-3: an exact prefix of the stored turns writes nothing.
    #[test]
    fn a_prefix_of_the_stored_turns_is_a_repeat() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let full = dialogue(
            "sess",
            &[
                ("user", "alpha"),
                ("assistant", "hello"),
                ("user", "alpha"),
                ("assistant", "hello"),
            ],
        );
        let prefix = dialogue("sess", &[("user", "alpha"), ("assistant", "hello")]);
        archive(memory, &full, "jsonl");
        let before = record_bytes(memory);
        let again = archive(memory, &prefix, "jsonl");
        assert_eq!(again.log_number, 1);
        assert!(!again.is_new_archive());
        assert_eq!(record_bytes(memory), before);
    }

    /// AC-3: the same number of turns with different text is a new archive.
    #[test]
    fn the_same_count_with_different_text_is_a_new_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        archive(
            memory,
            &dialogue("sess", &[("user", "hello"), ("assistant", "alpha")]),
            "jsonl",
        );
        let second = archive(
            memory,
            &dialogue("sess", &[("user", "alpha"), ("assistant", "hello")]),
            "jsonl",
        );
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-3: the same texts with the roles swapped are a new archive.
    #[test]
    fn the_same_texts_with_roles_swapped_is_a_new_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        archive(
            memory,
            &dialogue("sess", &[("user", "hello"), ("assistant", "alpha")]),
            "jsonl",
        );
        let second = archive(
            memory,
            &dialogue("sess", &[("assistant", "hello"), ("user", "alpha")]),
            "jsonl",
        );
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-3: a grown transcript is a new archive.
    #[test]
    fn a_grown_transcript_is_a_new_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        archive(
            memory,
            &dialogue("sess", &[("user", "hello"), ("assistant", "alpha")]),
            "jsonl",
        );
        let second = archive(
            memory,
            &dialogue(
                "sess",
                &[("user", "hello"), ("assistant", "alpha"), ("user", "hello")],
            ),
            "jsonl",
        );
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-4: a PreCompact checkpoint is not the capture the hook returns.
    #[test]
    fn a_precompact_checkpoint_is_not_the_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let path = claude_transcript(memory);
        let input = hook_for("sess", &path);
        crate::checkpoint::run_with_hook_input("precompact", Some(&input), memory).unwrap();
        run_with_hook_input(&input, memory).unwrap();
        assert_eq!(conversation_names(memory).len(), 2);
        let first = fs::read_to_string(memory.join("conversations/conversation-001.md")).unwrap();
        let second = fs::read_to_string(memory.join("conversations/conversation-002.md")).unwrap();
        assert!(first.contains("source: \"precompact\""), "{first}");
        assert!(second.contains("source: \"jsonl\""), "{second}");
        assert!(second.contains("### User"), "{second}");
        assert!(!memory.join("graph").exists());
    }

    /// AC-4: source `checkpoint` is not a prior capture.
    #[test]
    fn a_checkpoint_source_is_not_a_prior_capture() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = dialogue("sess", &[("user", "hello")]);
        archive(memory, &conv, "checkpoint");
        let second = archive(memory, &conv, "jsonl");
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-5: a prefix of the latest capture reuses that archive, not an earlier one.
    #[test]
    fn a_prefix_reuses_the_latest_capture() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        archive(
            memory,
            &dialogue("sess", &[("user", "alpha"), ("assistant", "hello")]),
            "jsonl",
        );
        archive(
            memory,
            &dialogue(
                "sess",
                &[
                    ("user", "alpha"),
                    ("assistant", "hello"),
                    ("user", "alpha"),
                    ("assistant", "hello"),
                ],
            ),
            "jsonl",
        );
        let reused = archive(
            memory,
            &dialogue(
                "sess",
                &[("user", "alpha"), ("assistant", "hello"), ("user", "alpha")],
            ),
            "jsonl",
        );
        assert_eq!(reused.log_number, 2);
        assert!(!reused.is_new_archive());
        assert_eq!(conversation_names(memory).len(), 2);
    }

    /// AC-5: an empty session id is never deduplicated.
    #[test]
    fn an_empty_session_id_is_archived_twice() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = dialogue("", &[("user", "hello")]);
        assert_eq!(archive(memory, &conv, "jsonl").log_number, 1);
        let second = archive(memory, &conv, "jsonl");
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-5: `sess` and `sess-2` are different sessions in either order.
    #[test]
    fn distinct_session_ids_are_not_repeats() {
        for (first_id, second_id) in [("sess", "sess-2"), ("sess-2", "sess")] {
            let tmp = memory_with_conversations();
            let memory = tmp.path();
            let lines = &[("user", "hello"), ("assistant", "alpha")];
            archive(memory, &dialogue(first_id, lines), "jsonl");
            assert_eq!(
                prior_capture(&memory.join("conversations"), second_id),
                None,
                "{second_id} is not {first_id}"
            );
            let second = archive(memory, &dialogue(second_id, lines), "jsonl");
            assert_eq!(second.log_number, 2, "{first_id} then {second_id}");
            assert!(second.is_new_archive());
        }
    }

    /// AC-5: a session whose only user text is empty is not a repeat.
    #[test]
    fn an_empty_user_message_is_not_a_repeat() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = Conversation {
            session_id: "sess".into(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: 1,
            assistant_message_count: 0,
            entries: vec![ConversationEntry::UserMessage(String::new())],
        };
        assert_eq!(archive(memory, &conv, "jsonl").log_number, 1);
        let second = archive(memory, &conv, "jsonl");
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-6: without a sidecar, two consecutive user messages are not a repeat.
    ///
    /// The markdown merges those messages, so reading it would not round-trip.
    /// The sidecar is what makes the first repeat a repeat; deleting it must
    /// not fall through to the markdown.
    #[test]
    fn a_missing_sidecar_is_not_a_repeat() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = dialogue("sess", &[("user", "alpha"), ("user", "hello")]);
        archive(memory, &conv, "jsonl");
        let again = archive(memory, &conv, "jsonl");
        assert_eq!(again.log_number, 1);
        assert!(!again.is_new_archive());

        fs::remove_file(memory.join("turns/conversation-001.json")).unwrap();
        assert!(stored_turns(memory, 1, "sess").is_none());
        let third = archive(memory, &conv, "jsonl");
        assert_eq!(third.log_number, 2);
        assert!(third.is_new_archive());
    }

    /// AC-6: a sidecar that names another session is not this capture.
    #[test]
    fn a_sidecar_for_another_session_is_not_a_repeat() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = dialogue("sess", &[("user", "hello")]);
        archive(memory, &conv, "jsonl");
        let path = memory.join("turns/conversation-001.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["session_id"] = serde_json::json!("sess-2");
        let body = serde_json::to_string_pretty(&value).unwrap();
        fs::write(&path, format!("{body}\n")).unwrap();
        let second = archive(memory, &conv, "jsonl");
        assert_eq!(second.log_number, 2);
        assert!(second.is_new_archive());
    }

    /// AC-6: `text_turns` and the sidecar agree, including what both skip.
    #[test]
    fn text_turns_match_the_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            ConversationEntry::UserMessage(String::new()),
            ConversationEntry::UserMessage("alpha".into()),
            ConversationEntry::UserMessage("hello".into()),
            ConversationEntry::ToolUse {
                name: "read".into(),
                input_summary: "notes".into(),
            },
            ConversationEntry::ToolResult {
                content: "ok".into(),
                is_error: false,
            },
            ConversationEntry::AssistantText(String::new()),
            ConversationEntry::AssistantText("alpha\n---\nhello".into()),
        ];
        crate::graph::v2::input::write_turns_sidecar(
            tmp.path(),
            1,
            "sess",
            "2026-01-01",
            None,
            &entries,
        )
        .unwrap();
        let loaded = crate::graph::v2::input::load_session(tmp.path(), 1).unwrap();
        let got: Vec<(TurnRole, String)> = loaded
            .turns
            .iter()
            .map(|turn| (turn.role, turn.text.clone()))
            .collect();
        let expected: Vec<(TurnRole, String)> = text_turns(&entries)
            .into_iter()
            .map(|(role, text)| (role, text.to_string()))
            .collect();
        assert_eq!(got, expected);
        assert_eq!(
            expected,
            vec![
                (TurnRole::User, "alpha".to_string()),
                (TurnRole::User, "hello".to_string()),
                (TurnRole::Assistant, "alpha\n---\nhello".to_string()),
            ]
        );
    }

    /// AC-7: a returned or empty result is not a new archive.
    #[test]
    fn is_new_archive_requires_a_number_and_a_body() {
        let cases = [(0u32, "", false), (3, "", false), (3, "x", true)];
        for (log_number, body, expected) in cases {
            let result = ArchiveResult {
                log_number,
                full_content: body.into(),
                session_id: "sess".into(),
            };
            assert_eq!(result.is_new_archive(), expected, "{log_number} {body:?}");
        }
    }

    /// AC-7: ingesting a returned archive does not create a graph.
    #[test]
    fn graph_ingest_skips_a_returned_archive() {
        let tmp = memory_with_conversations();
        let memory = tmp.path();
        let conv = dialogue("sess", &[("user", "hello")]);
        archive(memory, &conv, "jsonl");
        let returned = archive(memory, &conv, "jsonl");
        assert!(!returned.is_new_archive());
        graph_ingest(memory, &returned);
        assert!(!memory.join("graph").exists());
    }

    /// AC-7: the callers that ingest gate on `is_new_archive`.
    #[test]
    fn ingest_callers_gate_on_is_new_archive() {
        let archive_src = include_str!("archive.rs");
        let production = archive_src.split("mod tests").next().expect("tests");
        assert_eq!(production.matches("result.is_new_archive()").count(), 3);
        let checkpoint = include_str!("checkpoint.rs");
        let checkpoint_production = checkpoint.split("mod tests").next().expect("tests");
        assert!(checkpoint_production.contains("if result.is_new_archive()"));
    }

    /// The summary block is byte-identical to the inline block on `main`.
    #[test]
    fn summary_section_renders_decisions_and_action_items() {
        let full = summarize::ConversationSummary {
            summary: "alpha".into(),
            decisions: vec!["hello".into()],
            action_items: vec!["sess".into()],
            ..Default::default()
        };
        assert_eq!(
            summary_section(&full),
            "## Summary\n\nalpha\n\n**Decisions**:\n- hello\n\n**Action Items**:\n- sess\n\n"
        );
        let only = summarize::ConversationSummary {
            summary: "alpha".into(),
            ..Default::default()
        };
        assert_eq!(summary_section(&only), "## Summary\n\nalpha\n\n");
        let no_text = summarize::ConversationSummary {
            decisions: vec!["hello".into()],
            ..Default::default()
        };
        assert_eq!(summary_section(&no_text), "");
    }

    /// A log number of 0 is never a new archive, whatever the body holds.
    #[test]
    fn log_zero_is_never_a_new_archive() {
        let result = ArchiveResult {
            log_number: 0,
            full_content: "x".into(),
            session_id: "sess".into(),
        };
        assert!(!result.is_new_archive());
    }

    /// The header read stops at the closing marker, so the body is never read.
    #[test]
    fn read_header_stops_at_the_closing_marker() {
        let tmp = memory_with_conversations();
        let path = tmp.path().join("conversations/conversation-001.md");
        let mut bytes = b"---\nlog: 1\ndate: \"2026-01-01T00:00:00Z\"\nsession_id: \"sess\"\nsource: \"jsonl\"\n---\n".to_vec();
        bytes.extend_from_slice(b"\xff\xfe not utf-8\n");
        fs::write(&path, bytes).unwrap();
        let header = read_header(&path).expect("header");
        assert_eq!(header.session_id, "sess");
        assert_eq!(
            prior_capture(&tmp.path().join("conversations"), "sess"),
            Some(1)
        );
    }

    fn write_conversation(dir: &Path, number: u32, session_id: &str, source: &str) {
        let body = format!(
            "---\nlog: {number}\ndate: \"2026-08-05T22:29:00Z\"\nsession_id: \"{session_id}\"\nmessage_count: 1\nduration: \"unknown\"\nsource: \"{source}\"\ntopics: []\n---\n\n# Conversation {number:03}\n\n---\n\n### User\n\nhello\n"
        );
        fs::write(dir.join(format!("conversation-{number:03}.md")), body).unwrap();
    }

    /// A PreCompact checkpoint is not a capture, and neither is an empty id
    /// or a header that does not parse.
    #[test]
    fn a_checkpoint_only_session_is_not_captured() {
        let tmp = memory_with_conversations();
        let dir = tmp.path().join("conversations");
        write_conversation(&dir, 1, "sess", "precompact");
        write_conversation(&dir, 2, "sess-2", "checkpoint");
        write_conversation(&dir, 3, "", "jsonl");
        fs::write(dir.join("conversation-004.md"), "session_id: alpha\n").unwrap();

        let captured = collect_captured_sessions(&dir);
        assert!(captured.is_empty(), "{captured:?}");
        let archived = collect_archived_sessions(&dir);
        assert!(archived.contains("sess"), "{archived:?}");
        assert!(archived.contains("sess-2"), "{archived:?}");
        assert!(archived.contains("alpha"), "{archived:?}");
    }

    /// Every allowlisted source counts. A checkpoint beside them does not.
    #[test]
    fn a_capture_counts_as_captured() {
        let tmp = memory_with_conversations();
        let dir = tmp.path().join("conversations");
        let ids = ["sess", "sess-2", "alpha", "sess-3", "sess-4", "sess-5"];
        assert_eq!(ids.len(), CAPTURE_SOURCES.len());
        for (index, source) in CAPTURE_SOURCES.iter().enumerate() {
            write_conversation(&dir, (index as u32) + 1, ids[index], source);
        }
        write_conversation(&dir, 7, "hello", "precompact");

        let captured = collect_captured_sessions(&dir);
        assert_eq!(captured.len(), CAPTURE_SOURCES.len(), "{captured:?}");
        for id in ids {
            assert!(captured.contains(id), "{id} missing from {captured:?}");
        }
        assert!(!captured.contains("hello"));
    }

    const CODEX_ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-08-05T22:29:00.878Z","type":"session_meta","payload":{"session_id":"SESSION","cwd":"/tmp/probe"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.329Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:04.028Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"alpha"}]}}"#,
        "\n",
    );

    const CODEX_SESS: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn settled_now() -> crate::capture::CaptureOptions {
        crate::capture::CaptureOptions {
            settle: std::time::Duration::from_secs(0),
            now: std::time::SystemTime::now(),
        }
    }

    fn codex_fixture(root: &Path, session_id: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let memory = root.join("memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        let sessions = root.join("sessions");
        fs::create_dir_all(sessions.join("2026/08/05")).unwrap();
        let rollout = sessions.join(format!(
            "2026/08/05/rollout-2026-08-05T22-29-00-{session_id}.jsonl"
        ));
        fs::write(&rollout, CODEX_ROLLOUT.replace("SESSION", session_id)).unwrap();
        fs::create_dir_all(memory.join("capture")).unwrap();
        fs::write(memory.join("capture/codex.watermark"), "1\n").unwrap();
        (memory, sessions)
    }

    /// A checkpoint-only session newer than the watermark is captured in full.
    #[test]
    fn the_sweep_captures_a_checkpoint_only_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (memory, sessions) = codex_fixture(tmp.path(), CODEX_SESS);
        write_conversation(&memory.join("conversations"), 1, CODEX_SESS, "precompact");
        assert!(!memory.join("graph").exists());

        let report = crate::capture::sweep(
            &memory,
            &crate::transcript::CodexTranscripts::new(sessions),
            settled_now(),
        )
        .unwrap();

        assert_eq!(report.archived, vec![2]);
        let captured =
            fs::read_to_string(memory.join("conversations/conversation-002.md")).unwrap();
        let header = crate::frontmatter::parse(&captured).unwrap();
        assert_eq!(header.session_id, CODEX_SESS);
        assert_eq!(header.source, "codex");
        assert!(CAPTURE_SOURCES.contains(&header.source.as_str()));
        assert!(memory.join("turns/conversation-002.json").is_file());
        assert!(!memory.join("graph").exists());
    }

    /// A session that already has a capture is still skipped.
    #[test]
    fn the_sweep_still_skips_a_captured_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (memory, sessions) = codex_fixture(tmp.path(), CODEX_SESS);
        write_conversation(&memory.join("conversations"), 1, CODEX_SESS, "codex");

        let report = crate::capture::sweep(
            &memory,
            &crate::transcript::CodexTranscripts::new(sessions),
            settled_now(),
        )
        .unwrap();

        assert!(report.archived.is_empty(), "{report:?}");
        assert_eq!(report.duplicates, 1);
        assert_eq!(
            fs::read_dir(memory.join("conversations")).unwrap().count(),
            1
        );
    }

    /// `archive --all-unarchived` captures a checkpoint-only session.
    #[test]
    fn all_unarchived_captures_a_checkpoint_only_session() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects/-home-dev-app");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("sess.jsonl"), CLAUDE_JSONL).unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        write_conversation(&memory.join("conversations"), 1, "sess", "precompact");
        assert!(!memory.join("graph").exists());

        let transcripts = ClaudeCodeTranscripts::new(tmp.path().join("projects"));
        let report = archive_all_with(&memory, &transcripts).unwrap();

        assert_eq!(report.archived, 1);
        assert_eq!(report.already_archived, 0);
        let captured =
            fs::read_to_string(memory.join("conversations/conversation-002.md")).unwrap();
        assert!(captured.contains("source: \"jsonl\""), "{captured}");
        assert!(captured.contains("session_id: \"sess\""), "{captured}");
        assert!(memory.join("turns/conversation-002.json").is_file());
        assert!(!memory.join("graph").exists());
    }

    fn rendered_log(number: u32) -> String {
        format!("---\nlog: {number}\n---\n\n# Conversation {number:03}\n")
    }

    fn claim_temps(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(".claim-").then_some(name)
            })
            .collect()
    }

    fn unsupported_link(_tmp: &Path, _dest: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no hard links",
        ))
    }

    fn crosses_devices_link(_tmp: &Path, _dest: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::CrossesDevices,
            "cross-device",
        ))
    }

    fn exdev_link(_tmp: &Path, _dest: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::EXDEV))
    }

    #[test]
    fn a_taken_number_moves_the_archive_to_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (number, content) = claim_archive_number(dir, |k| {
            if k == 1 {
                fs::write(dir.join("conversation-001.md"), "occupied").unwrap();
            }
            rendered_log(k)
        })
        .unwrap();
        assert_eq!(number, 2);
        assert!(content.contains("log: 2"), "{content}");
        assert!(content.contains("# Conversation 002"), "{content}");
        assert_eq!(
            fs::read_to_string(dir.join("conversation-002.md")).unwrap(),
            content
        );
    }

    #[test]
    fn the_taken_file_is_left_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let occupied = b"occupied-bytes\n";
        claim_archive_number(dir, |k| {
            if k == 1 {
                fs::write(dir.join("conversation-001.md"), occupied).unwrap();
            }
            rendered_log(k)
        })
        .unwrap();
        assert_eq!(fs::read(dir.join("conversation-001.md")).unwrap(), occupied);
    }

    #[test]
    fn no_claim_file_is_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let pending = ClaimFile::new(dir);
        let name = pending
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(".claim-") && name.ends_with(".tmp"),
            "{name}"
        );
        assert!(!name.starts_with("conversation-"), "{name}");
        assert!(!name.starts_with("archive-log-"), "{name}");
        fs::write(&pending.path, "temp").unwrap();
        fs::write(dir.join("conversation-003.md"), "kept").unwrap();
        assert_eq!(highest_conversation_number(dir), 3);
        drop(pending);
        assert!(!dir.join(&name).exists());

        claim_archive_number(dir, rendered_log).unwrap();
        assert!(claim_temps(dir).is_empty(), "after success");

        let next = highest_conversation_number(dir) + 1;
        claim_archive_number(dir, |k| {
            if k == next {
                fs::write(conversation_file(dir, k), "taken").unwrap();
            }
            rendered_log(k)
        })
        .unwrap();
        assert!(claim_temps(dir).is_empty(), "after a collision");

        let err = claim_archive_number_in(dir, rendered_log, |_: &Path, _: &Path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no",
            ))
        });
        assert!(err.is_err());
        assert!(claim_temps(dir).is_empty(), "after an error");
    }

    #[test]
    fn too_many_collisions_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let err = claim_archive_number(dir, |n| {
            fs::write(conversation_file(dir, n), "taken").unwrap();
            rendered_log(n)
        })
        .unwrap_err();
        assert!(matches!(err, RecallError::Other(_)), "{err}");
        assert!(claim_temps(dir).is_empty());
        assert!(dir.join("conversation-064.md").is_file());
        assert!(!dir.join("conversation-065.md").exists());
    }

    #[test]
    fn fallback_create_new_writes_the_archive() {
        let links: [fn(&Path, &Path) -> std::io::Result<()>; 3] =
            [unsupported_link, crosses_devices_link, exdev_link];
        for link in links {
            let tmp = tempfile::tempdir().unwrap();
            let (number, content) =
                claim_archive_number_in(tmp.path(), rendered_log, link).unwrap();
            assert_eq!(number, 1);
            assert!(content.contains("log: 1"), "{content}");
            assert!(content.contains("# Conversation 001"), "{content}");
            assert_eq!(
                fs::read_to_string(tmp.path().join("conversation-001.md")).unwrap(),
                content
            );
            assert!(claim_temps(tmp.path()).is_empty());
        }
    }

    #[test]
    fn fallback_create_new_rerenders_after_a_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let occupied = "occupied-bytes\n";
        let mut calls = Vec::new();
        let (number, content) = claim_archive_number_in(
            dir,
            |k| {
                calls.push(k);
                if k == 1 {
                    fs::write(dir.join("conversation-001.md"), occupied).unwrap();
                }
                rendered_log(k)
            },
            unsupported_link,
        )
        .unwrap();
        assert_eq!(calls, vec![1, 2]);
        assert_eq!(number, 2);
        assert!(content.contains("log: 2"), "{content}");
        assert!(content.contains("# Conversation 002"), "{content}");
        assert_eq!(
            fs::read_to_string(dir.join("conversation-001.md")).unwrap(),
            occupied
        );
        assert_eq!(
            fs::read_to_string(dir.join("conversation-002.md")).unwrap(),
            content
        );
        assert!(claim_temps(dir).is_empty());
    }

    #[test]
    fn concurrent_archives_take_distinct_numbers() {
        let tmp = memory_with_conversations();
        let memory = tmp.path().to_path_buf();
        fs::write(
            memory.join("ARCHIVE.md"),
            "# Conversation Archive\n\n| # | Date | Session | Topics | Messages | Duration |\n|---|------|---------|--------|----------|----------|\n",
        )
        .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let iterations = 4u32;
        let spawn = |prefix: &str, barrier: std::sync::Arc<std::sync::Barrier>, memory: PathBuf| {
            let prefix = prefix.to_string();
            std::thread::spawn(move || {
                for i in 0..iterations {
                    barrier.wait();
                    let conv = one_turn(&format!("{prefix}-{i}"), Some("2026-08-05T22:29:00Z"));
                    archive_conversation(
                        &memory,
                        &conv,
                        &summarize::ConversationSummary::default(),
                        "jsonl",
                    )
                    .unwrap();
                }
            })
        };
        let first = spawn("sess", std::sync::Arc::clone(&barrier), memory.clone());
        let second = spawn("alpha", barrier, memory.clone());
        first.join().unwrap();
        second.join().unwrap();

        let conversations = memory.join("conversations");
        let mut numbers = Vec::new();
        for entry in fs::read_dir(&conversations).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(raw) = name
                .strip_prefix("conversation-")
                .and_then(|rest| rest.strip_suffix(".md"))
            else {
                continue;
            };
            let number: u32 = raw.parse().unwrap();
            let body = fs::read_to_string(entry.path()).unwrap();
            let header = crate::frontmatter::parse(&body).unwrap();
            assert_eq!(header.log, number, "{name}");
            assert!(
                body.contains(&format!("# Conversation {number:03}")),
                "{name}"
            );
            numbers.push(number);
        }
        numbers.sort_unstable();
        let distinct = numbers.len();
        numbers.dedup();
        assert_eq!(numbers.len(), 8, "{numbers:?}");
        assert_eq!(distinct, numbers.len());

        let index = fs::read_to_string(memory.join("ARCHIVE.md")).unwrap();
        for i in 0..iterations {
            assert_eq!(index.matches(&format!("sess-{i}")).count(), 1, "{index}");
            assert_eq!(index.matches(&format!("alpha-{i}")).count(), 1, "{index}");
        }
        assert!(claim_temps(&conversations).is_empty());
    }
}
