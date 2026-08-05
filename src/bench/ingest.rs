// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LoCoMo ingestion — write each session as a recall-echo archive, then
//! trigger graph extraction over the resulting archive set.
//!
//! Reuses the same primitives the runtime archive pipeline uses
//! ([`Frontmatter`], [`conversation_to_markdown`], [`tags`], [`append_index`],
//! [`ephemeral::append_entry`]) so the artifacts on disk are bit-for-bit what
//! a real entity would produce — only the *date* is back-stamped to the
//! session's LoCoMo timestamp instead of `now()`.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::archive::{append_index, highest_conversation_number};
use crate::conversation::{self, conversation_to_markdown, Conversation, ConversationEntry};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
use crate::graph::{GraphMemory, IngestContext};
use crate::tags;

use super::{BenchConversation, BenchSession};

/// Result of ingesting a single LoCoMo conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IngestStats {
    pub sessions_written: usize,
    pub entities_extracted: usize,
    pub relations_extracted: usize,
    pub episodes: usize,
    /// Per-session log numbers in the order they were written.
    pub log_numbers: Vec<u32>,
    /// Non-fatal warnings surfaced during extraction (passed through from
    /// `IngestionReport.errors`).
    pub warnings: Vec<String>,
    /// Entity candidates that cost a dedup model call — the cost term that
    /// used to grow with the size of the graph.
    #[serde(default)]
    pub dedup_llm_calls: usize,
    /// Entity candidates dedup resolved without a model call.
    #[serde(default)]
    pub dedup_fast_path: usize,
}

/// Ingest a LoCoMo conversation into the entity at `entity_root`.
///
/// Writes one archive per session, with the frontmatter `date` set from the
/// session's `date_time` so temporal queries make sense. Then drives graph
/// extraction over each archive — the same code path the runtime uses on
/// SessionEnd.
///
/// `llm` is optional: when `None`, only episodes are created (no entity /
/// relationship extraction). The benchmark harness will normally pass a
/// provider so the graph is fully populated.
pub async fn ingest_conversation(
    entity_root: &Path,
    conv: &BenchConversation,
    llm: Option<&dyn crate::graph::llm::LlmProvider>,
) -> Result<IngestStats, RecallError> {
    let memory_dir = entity_root.join("memory");
    ensure_layout(&memory_dir)?;

    let graph_dir = memory_dir.join("graph");
    let gm = GraphMemory::open(&graph_dir).await?;

    let mut stats = IngestStats::default();

    for (idx, session) in conv.sessions.iter().enumerate() {
        let session_index = idx + 1;
        let session_id = format!("{}:session-{}", conv.sample_id, session_index);
        let timestamp = normalize_date_time(&session.date_time);

        let written = write_session_archive(
            &memory_dir,
            conv,
            session,
            session_index,
            &session_id,
            &timestamp,
        )?;

        stats.sessions_written += 1;
        stats.log_numbers.push(written.log_number);

        // Benchmark sessions are written through the same archive renderer as
        // real conversations, so turn-role inference applies unchanged.
        let context = IngestContext::new(&session_id, Some(written.log_number));
        let report = gm
            .ingest_archive(&written.full_content, &context, llm)
            .await?;

        stats.episodes += report.episodes_created as usize;
        stats.entities_extracted += report.entities_created as usize;
        stats.relations_extracted += report.relationships_created as usize;
        stats.dedup_llm_calls += report.dedup_llm_calls as usize;
        stats.dedup_fast_path += report.dedup_fast_path as usize;
        stats.warnings.extend(report.errors);
    }

    Ok(stats)
}

// ── Internal helpers ─────────────────────────────────────────────────────

struct WrittenArchive {
    log_number: u32,
    full_content: String,
}

fn write_session_archive(
    memory_dir: &Path,
    conv: &BenchConversation,
    session: &BenchSession,
    session_index: usize,
    session_id: &str,
    timestamp: &str,
) -> Result<WrittenArchive, RecallError> {
    let conversations_dir = memory_dir.join("conversations");
    let archive_index = memory_dir.join("ARCHIVE.md");
    let ephemeral_path = memory_dir.join("EPHEMERAL.md");

    let log_number = highest_conversation_number(&conversations_dir) + 1;

    let conversation = session_to_conversation(conv, session, session_id, timestamp);
    let message_count = conversation.total_messages();

    let topics = build_topics(conv, session_index);
    let fm = Frontmatter {
        log: log_number,
        date: timestamp.to_string(),
        session_id: session_id.to_string(),
        message_count,
        duration: "session".to_string(),
        source: "locomo".to_string(),
        topics: topics.clone(),
    };

    let md_body = conversation_to_markdown(&conversation, log_number);
    let conv_tags = tags::extract_tags(&conversation.entries);
    let tags_section = tags::format_tags_section(&conv_tags);

    let full_content = format!("{}\n\n{}\n{}", fm.render(), md_body, tags_section);

    let conv_file = conversations_dir.join(format!("conversation-{log_number:03}.md"));
    fs::write(&conv_file, &full_content)?;

    let date_only = conversation::date_from_timestamp(timestamp);
    append_index(
        &archive_index,
        log_number,
        &date_only,
        session_id,
        &topics,
        message_count,
        "session",
    )?;

    let entry = EphemeralEntry {
        session_id: session_id.to_string(),
        date: timestamp.to_string(),
        duration: "session".to_string(),
        message_count,
        archive_file: format!("conversation-{log_number:03}.md"),
        summary: format!(
            "LoCoMo {} session {} ({} \u{2194} {})",
            conv.sample_id, session_index, conv.speaker_a, conv.speaker_b
        ),
    };
    ephemeral::append_entry(&ephemeral_path, &entry)?;
    let cfg = crate::config::load_from_dir(memory_dir);
    ephemeral::trim_to_limit(&ephemeral_path, cfg.ephemeral.max_entries)?;

    Ok(WrittenArchive {
        log_number,
        full_content,
    })
}

fn session_to_conversation(
    conv: &BenchConversation,
    session: &BenchSession,
    session_id: &str,
    timestamp: &str,
) -> Conversation {
    let mut entries = Vec::with_capacity(session.turns.len());
    let mut user_count = 0u32;
    let mut assistant_count = 0u32;

    for turn in &session.turns {
        // Map speaker_a → user, speaker_b → assistant. This is a synthetic
        // mapping for archive shape — the actual speaker name is preserved
        // inside the text body so retrieval and downstream extraction can
        // see it.
        let body = format!("{}: {}", turn.speaker, turn.text);
        if turn.speaker == conv.speaker_b {
            entries.push(ConversationEntry::AssistantText(body));
            assistant_count += 1;
        } else {
            entries.push(ConversationEntry::UserMessage(body));
            user_count += 1;
        }
    }

    Conversation {
        session_id: session_id.to_string(),
        first_timestamp: Some(timestamp.to_string()),
        last_timestamp: Some(timestamp.to_string()),
        user_message_count: user_count,
        assistant_message_count: assistant_count,
        entries,
    }
}

fn build_topics(conv: &BenchConversation, session_index: usize) -> Vec<String> {
    vec![
        "locomo".to_string(),
        conv.sample_id.clone(),
        format!("session-{session_index}"),
    ]
}

fn ensure_layout(memory_dir: &Path) -> Result<(), RecallError> {
    let conversations_dir = memory_dir.join("conversations");
    fs::create_dir_all(&conversations_dir)?;
    fs::create_dir_all(memory_dir.join("graph"))?;
    let memory_md = memory_dir.join("MEMORY.md");
    if !memory_md.exists() {
        fs::write(&memory_md, "")?;
    }
    Ok(())
}

/// Normalize a LoCoMo `date_time` like `"22 May 2023"` or
/// `"22 May 2023, 10:00 am"` to ISO 8601 (`2023-05-22T10:00:00Z`).
/// Falls back to midnight UTC when the time portion is missing or
/// unparseable, and to the raw string for completely opaque inputs so the
/// caller still gets *something* in the frontmatter.
fn normalize_date_time(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return conversation::utc_now();
    }

    // Pre-parsed ISO 8601 — accept as-is.
    if trimmed.contains('T') && trimmed.ends_with('Z') {
        return trimmed.to_string();
    }

    // LoCoMo strings come as "DD Mon YYYY[, HH:MM (am|pm)]" or
    // "Mon DD, YYYY[ HH:MM (am|pm)]". We split on comma first to peel the
    // optional time portion.
    let (date_part, time_part) = match trimmed.split_once(',') {
        Some((d, t)) => (d.trim(), Some(t.trim())),
        None => (trimmed, None),
    };

    let (year, month, day) = match parse_date_part(date_part) {
        Some(triple) => triple,
        None => return trimmed.to_string(),
    };

    let (hour, minute) = time_part.and_then(parse_time_part).unwrap_or((0, 0));

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z")
}

fn parse_date_part(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 3 {
        return None;
    }

    // Try "DD Mon YYYY"
    if let (Ok(day), Some(month), Ok(year)) = (
        parts[0].parse::<u32>(),
        month_from_str(parts[1]),
        parts[2].parse::<u32>(),
    ) {
        return Some((year, month, day));
    }

    // Try "Mon DD YYYY" (DD may have a trailing comma stripped already)
    if let (Some(month), Ok(day), Ok(year)) = (
        month_from_str(parts[0]),
        parts[1].trim_end_matches(',').parse::<u32>(),
        parts[2].parse::<u32>(),
    ) {
        return Some((year, month, day));
    }

    None
}

fn parse_time_part(s: &str) -> Option<(u32, u32)> {
    let lowered = s.to_lowercase();
    let mut pm = false;
    let body = if let Some(rest) = lowered.strip_suffix("am") {
        rest.trim()
    } else if let Some(rest) = lowered.strip_suffix("pm") {
        pm = true;
        rest.trim()
    } else {
        lowered.trim()
    };

    let (h_str, m_str) = body.split_once(':').unwrap_or((body, "0"));
    let mut hour: u32 = h_str.trim().parse().ok()?;
    let minute: u32 = m_str.trim().parse().ok()?;

    if pm && hour < 12 {
        hour += 12;
    } else if !pm && hour == 12 && lowered.contains("am") {
        hour = 0;
    }

    Some((hour, minute))
}

fn month_from_str(s: &str) -> Option<u32> {
    match s.to_lowercase().trim_end_matches('.') {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}
