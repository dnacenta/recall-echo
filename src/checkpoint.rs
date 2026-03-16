//! Checkpoint creation — saves a conversation snapshot before context compression.
//!
//! Supports two paths:
//! 1. **JSONL hook** — called by Claude Code PreCompact hook (standalone)
//! 2. **Pulse-null** — called with in-memory Messages (behind feature flag)

use std::fs;
use std::path::Path;

use crate::archive;
use crate::conversation;
use crate::frontmatter::Frontmatter;
use crate::tags;

// ---------------------------------------------------------------------------
// JSONL path — for Claude Code PreCompact hook
// ---------------------------------------------------------------------------

/// Checkpoint from a JSONL transcript (Claude Code hook).
/// Reads hook input from stdin.
pub fn run_from_hook(trigger: &str) -> Result<(), String> {
    run_from_hook_with_paths(trigger, &crate::paths::claude_dir()?)
}

pub fn run_from_hook_with_paths(trigger: &str, base_dir: &Path) -> Result<(), String> {
    let conversations_dir = base_dir.join("conversations");
    let archive_index = base_dir.join("ARCHIVE.md");

    if !conversations_dir.exists() {
        return Err("conversations/ directory not found. Run init first.".to_string());
    }

    // Try to read hook input from stdin (Claude Code passes transcript_path)
    let hook_input = crate::jsonl::read_hook_input().ok();

    let next_num = archive::highest_conversation_number(&conversations_dir) + 1;
    let now = conversation::utc_now();
    let date = conversation::date_from_timestamp(&now);

    // If we have hook input with a transcript, parse it for metadata
    let data = match &hook_input {
        Some(input) => extract_from_transcript(input).unwrap_or_else(empty_checkpoint),
        None => empty_checkpoint(),
    };

    let fm = Frontmatter {
        log: next_num,
        date: now,
        session_id: data.session_id,
        message_count: data.message_count,
        duration: data.duration.clone(),
        source: trigger.to_string(),
        topics: data.topics.clone(),
    };

    let full_content = format!("{}\n\n{}{}", fm.render(), data.md_body, data.tags_section);

    let conv_file = conversations_dir.join(format!("conversation-{next_num:03}.md"));
    fs::write(&conv_file, &full_content)
        .map_err(|e| format!("Failed to create checkpoint file: {e}"))?;

    // Graph ingestion
    #[cfg(feature = "graph")]
    {
        let result = archive::ArchiveResult {
            log_number: next_num,
            full_content: full_content.clone(),
            session_id: fm.session_id.clone(),
        };
        archive::graph_ingest(base_dir, &result);
    }

    archive::append_index(
        &archive_index,
        next_num,
        &date,
        &fm.session_id,
        &data.topics,
        data.message_count,
        &data.duration,
    )?;

    eprintln!(
        "recall-echo: checkpoint conversation-{:03}.md ({} \u{2014} {} messages, {} topics)",
        next_num,
        trigger,
        data.message_count,
        data.topics.len()
    );

    Ok(())
}

struct CheckpointData {
    session_id: String,
    topics: Vec<String>,
    message_count: u32,
    duration: String,
    md_body: String,
    tags_section: String,
}

fn extract_from_transcript(input: &crate::jsonl::HookInput) -> Option<CheckpointData> {
    let conv = crate::jsonl::parse_transcript(&input.transcript_path, &input.session_id).ok()?;

    if conv.user_message_count == 0 {
        return None;
    }

    let duration = match (&conv.first_timestamp, &conv.last_timestamp) {
        (Some(first), Some(last)) => conversation::calculate_duration(first, last),
        _ => "unknown".to_string(),
    };
    let total_messages = conv.total_messages();
    let topics = conversation::extract_topics(&conv, 5);
    let md_body = conversation::conversation_to_markdown(&conv, 0);
    let conv_tags = tags::extract_tags(&conv.entries);
    let tags_section = tags::format_tags_section(&conv_tags);

    Some(CheckpointData {
        session_id: input.session_id.clone(),
        topics,
        message_count: total_messages,
        duration,
        md_body,
        tags_section,
    })
}

fn empty_checkpoint() -> CheckpointData {
    CheckpointData {
        session_id: String::new(),
        topics: vec![],
        message_count: 0,
        duration: String::new(),
        md_body: "# Checkpoint\n\nNo transcript available.\n".to_string(),
        tags_section: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Pulse-null path — behind feature flag
// ---------------------------------------------------------------------------

/// Create a checkpoint from pulse-null in-memory messages.
///
/// Returns the conversation number of the created checkpoint.
#[cfg(feature = "pulse-null")]
pub async fn create_checkpoint(
    memory_dir: &Path,
    messages: &[echo_system_types::llm::Message],
    metadata: &archive::SessionMetadata,
    provider: Option<&dyn echo_system_types::llm::LmProvider>,
) -> Result<u32, String> {
    let mut conv = crate::pulse_null::messages_to_conversation(messages, &metadata.session_id);
    conv.first_timestamp = metadata.started_at.clone();
    conv.last_timestamp = metadata.ended_at.clone();

    let summary = crate::summarize::extract_with_fallback(provider, &conv).await;
    let result = archive::archive_conversation(memory_dir, &conv, &summary, "checkpoint")?;
    let log_number = result.log_number;

    // Graph ingestion (async path)
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
    use std::io::Write;

    fn write_test_jsonl(dir: &Path) -> String {
        let path = dir.join("test-session.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        let lines = [
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-03-05T14:30:00.000Z","sessionId":"test-ckpt"}"#,
            r#"{"parentUuid":null,"type":"user","sessionId":"test-ckpt","timestamp":"2026-03-05T14:30:00.100Z","message":{"role":"user","content":"Let's refactor the auth module to use JWT"}}"#,
            r#"{"parentUuid":"aaa","type":"assistant","sessionId":"test-ckpt","timestamp":"2026-03-05T14:30:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"I'll refactor the auth module to use JWT tokens."}]}}"#,
            r#"{"parentUuid":"bbb","type":"assistant","sessionId":"test-ckpt","timestamp":"2026-03-05T14:30:06.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{"file_path":"/src/auth.rs"}}]}}"#,
            r#"{"parentUuid":"ccc","type":"user","sessionId":"test-ckpt","timestamp":"2026-03-05T14:30:07.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"pub fn login() {}"}]}}"#,
            r#"{"parentUuid":"ddd","type":"user","sessionId":"test-ckpt","timestamp":"2026-03-05T14:35:00.000Z","message":{"role":"user","content":"Now add token validation"}}"#,
            r#"{"parentUuid":"eee","type":"assistant","sessionId":"test-ckpt","timestamp":"2026-03-05T14:35:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Adding token validation now."}]}}"#,
        ];
        for line in &lines {
            writeln!(f, "{}", line).unwrap();
        }
        path.to_string_lossy().to_string()
    }

    #[test]
    fn checkpoint_with_transcript_extracts_topics() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_jsonl(tmp.path());

        let input = crate::jsonl::HookInput {
            session_id: "test-ckpt".to_string(),
            transcript_path: p,
            cwd: None,
            hook_event_name: Some("PreCompact".to_string()),
        };

        let data = extract_from_transcript(&input);
        assert!(data.is_some());

        let data = data.unwrap();
        assert_eq!(data.session_id, "test-ckpt");
        assert!(data.message_count > 0);
        assert!(!data.topics.is_empty());
    }

    #[test]
    fn empty_checkpoint_fallback() {
        let data = empty_checkpoint();
        assert!(data.session_id.is_empty());
        assert!(data.topics.is_empty());
        assert_eq!(data.message_count, 0);
        assert!(data.md_body.contains("No transcript available"));
    }
}
