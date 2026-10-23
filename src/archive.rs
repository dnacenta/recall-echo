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
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::config;
use crate::conversation::{self, Conversation};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
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

    let next_num = highest_conversation_number(&conversations_dir) + 1;

    let (timestamp, date) = session_clock(conv.first_timestamp.as_deref());
    let duration = match (&conv.first_timestamp, &conv.last_timestamp) {
        (Some(start), Some(end)) => conversation::calculate_duration(start, end),
        _ => "unknown".to_string(),
    };
    let total_messages = conv.total_messages();

    // Build frontmatter
    let fm = Frontmatter {
        log: next_num,
        date: timestamp.clone(),
        session_id: conv.session_id.clone(),
        message_count: total_messages,
        duration: duration.clone(),
        source: source.to_string(),
        pulse: pulse.map(String::from),
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
    // The sidecar is a second record. A failure here must not throw away the
    // archive that was just written.
    write_sidecar_best_effort(
        memory_dir,
        next_num,
        &conv.session_id,
        &date,
        cwd,
        &conv.entries,
    );

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
        date: timestamp,
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

    let archived_sessions = collect_archived_sessions(&conversations_dir);

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
        archive_conversation(memory, &conv, &summary, "jsonl").unwrap();

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
}
