// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checkpoint creation — saves a conversation snapshot before context compression.
//!
//! Supports two paths:
//! 1. **JSONL hook** — called by Claude Code PreCompact hook (standalone)
//! 2. **Pulse-null** — called with in-memory Messages (behind feature flag)

use std::path::Path;

use crate::archive;
use crate::conversation;
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
use crate::tags;

// ---------------------------------------------------------------------------
// JSONL path — for Claude Code PreCompact hook
// ---------------------------------------------------------------------------

/// Checkpoint from a JSONL transcript (Claude Code hook).
/// Reads hook input from stdin.
pub fn run_from_hook(trigger: &str, pulse_root: Option<&Path>) -> Result<(), RecallError> {
    run_from_hook_with_paths(trigger, &crate::paths::resolved_hook_base_dir(pulse_root)?)
}

pub fn run_from_hook_with_paths(trigger: &str, base_dir: &Path) -> Result<(), RecallError> {
    // Try to read hook input from stdin (Claude Code passes transcript_path)
    let hook_input = crate::jsonl::read_hook_input().ok();
    run_with_hook_input(trigger, hook_input.as_ref(), base_dir)
}

/// Checkpoint the session a hook input names — or an empty checkpoint when
/// there is no input.
///
/// A subagent compacting its own context is not the session compacting: its
/// transcript is noted on stderr and nothing is numbered or written.
pub fn run_with_hook_input(
    trigger: &str,
    hook_input: Option<&crate::jsonl::HookInput>,
    base_dir: &Path,
) -> Result<(), RecallError> {
    if let Some(input) = hook_input {
        let transcript_path = input.transcript_path.trim();
        if crate::jsonl::is_subagent_transcript(Path::new(transcript_path)) {
            crate::jsonl::report_declined_subagent(transcript_path, "no checkpoint written");
            return Ok(());
        }
    }

    let conversations_dir = base_dir.join("conversations");
    let archive_index = base_dir.join("ARCHIVE.md");

    if !conversations_dir.exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run init first.".into(),
        ));
    }

    let now = conversation::utc_now();
    let date = conversation::date_from_timestamp(&now);

    // If we have hook input with a transcript, parse it for metadata
    let data = match hook_input {
        Some(input) => extract_from_transcript(input).unwrap_or_else(empty_checkpoint),
        None => empty_checkpoint(),
    };

    let mut fm = Frontmatter {
        log: 0,
        date: now,
        session_id: data.session_id,
        message_count: data.message_count,
        duration: data.duration.clone(),
        source: trigger.to_string(),
        pulse: None,
        topics: data.topics.clone(),
    };

    let (next_num, full_content) = archive::claim_archive_number(&conversations_dir, |n| {
        fm.log = n;
        format!("{}\n\n{}{}", fm.render(), data.md_body, data.tags_section)
    })?;

    // The sidecar is a second record. A failure here must not throw away the
    // archive that was just written; the helper logs and returns.
    if crate::graph::v2::input::turns_sidecar_enabled() {
        archive::write_sidecar_best_effort(
            base_dir,
            next_num,
            &fm.session_id,
            &date,
            None,
            &data.entries,
        );
    }

    // Graph ingestion
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
    entries: Vec<conversation::ConversationEntry>,
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
        entries: conv.entries,
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
        entries: Vec::new(),
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
    messages: &[pulse_system_types::llm::Message],
    metadata: &archive::SessionMetadata,
    provider: Option<&dyn pulse_system_types::llm::LmProvider>,
) -> Result<u32, RecallError> {
    let mut conv = crate::pulse_null::messages_to_conversation(messages, &metadata.session_id);
    conv.first_timestamp = metadata.started_at.clone();
    conv.last_timestamp = metadata.ended_at.clone();

    let summary = crate::summarize::extract_with_fallback(provider, &conv).await;
    let result = archive::archive_conversation(memory_dir, &conv, &summary, "checkpoint")?;
    let log_number = result.log_number;

    // Graph ingestion (async path)
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
    }

    Ok(log_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
            _cwd: None,
            _hook_event_name: Some("PreCompact".to_string()),
        };

        let data = extract_from_transcript(&input);
        assert!(data.is_some());

        let data = data.unwrap();
        assert_eq!(data.session_id, "test-ckpt");
        assert!(data.message_count > 0);
        assert!(!data.topics.is_empty());
    }

    /// RE-74: a subagent's compaction writes nothing — no conversation file, no
    /// index row — and does not fail the hook.
    #[test]
    fn checkpoint_on_subagent_transcript_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("conversations")).unwrap();
        let subagents = tmp.path().join("projects/-p/test-ckpt/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let path = subagents.join("agent-a1.jsonl");
        fs::rename(write_test_jsonl(tmp.path()), &path).unwrap();

        let input = crate::jsonl::HookInput {
            session_id: "agent-a1".to_string(),
            transcript_path: path.to_string_lossy().to_string(),
            _cwd: None,
            _hook_event_name: Some("PreCompact".to_string()),
        };
        run_with_hook_input("precompact", Some(&input), tmp.path()).unwrap();

        assert_eq!(
            fs::read_dir(tmp.path().join("conversations"))
                .unwrap()
                .count(),
            0
        );
        assert!(!tmp.path().join("ARCHIVE.md").exists());
    }

    #[test]
    fn empty_checkpoint_fallback() {
        let data = empty_checkpoint();
        assert!(data.session_id.is_empty());
        assert!(data.topics.is_empty());
        assert_eq!(data.message_count, 0);
        assert!(data.md_body.contains("No transcript available"));
    }

    fn prepared_root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("conversations")).unwrap();
        tmp
    }

    fn hook_for(root: &Path, session_id: &str, lines: &[&str]) -> crate::jsonl::HookInput {
        let path = root.join("session.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        crate::jsonl::HookInput {
            session_id: session_id.to_string(),
            transcript_path: path.to_string_lossy().to_string(),
            _cwd: None,
            _hook_event_name: Some("PreCompact".to_string()),
        }
    }

    /// AC1: a hook checkpoint writes a sidecar, and load returns each turn's role.
    #[test]
    fn hook_checkpoint_writes_a_sidecar_with_roles() {
        let tmp = prepared_root();
        let lines = [
            r#"{"type":"user","timestamp":"2026-03-05T14:30:00.100Z","sessionId":"sess-a","message":{"role":"user","content":"alpha"}}"#,
            r#"{"type":"user","timestamp":"2026-03-05T14:31:00.100Z","sessionId":"sess-a","message":{"role":"user","content":"beta"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-05T14:32:00.100Z","sessionId":"sess-a","message":{"role":"assistant","content":[{"type":"text","text":"gamma"}]}}"#,
        ];
        let input = hook_for(tmp.path(), "sess-a", &lines);
        run_with_hook_input("precompact", Some(&input), tmp.path()).unwrap();

        let sidecar_path = tmp.path().join("turns/conversation-001.json");
        let sidecar: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(sidecar["session_id"], "sess-a");
        let archive =
            fs::read_to_string(tmp.path().join("conversations/conversation-001.md")).unwrap();
        let frontmatter = crate::frontmatter::parse(&archive).unwrap();
        assert_eq!(
            sidecar["date"],
            conversation::date_from_timestamp(&frontmatter.date)
        );
        assert!(archive.contains("### User"));
        assert!(!archive.contains("turns/"));

        let session = crate::graph::v2::input::load_session(tmp.path(), 1).unwrap();
        assert_eq!(session.session_id, "sess-a");
        assert_eq!(session.date, sidecar["date"].as_str().unwrap());
        let turns: Vec<_> = session
            .turns
            .iter()
            .map(|turn| (turn.role, turn.text.as_str()))
            .collect();
        assert_eq!(
            turns,
            vec![
                (crate::graph::v2::input::TurnRole::User, "alpha"),
                (crate::graph::v2::input::TurnRole::User, "beta"),
                (crate::graph::v2::input::TurnRole::Assistant, "gamma"),
            ]
        );
    }

    /// AC3: an empty checkpoint still writes a sidecar, replacing a leftover.
    #[test]
    fn empty_hook_checkpoint_overwrites_a_leftover_sidecar() {
        let tmp = prepared_root();
        crate::graph::v2::input::write_turns_sidecar(
            tmp.path(),
            1,
            "sess-a",
            "2020-01-01",
            None,
            &[conversation::ConversationEntry::UserMessage(
                "leftover".into(),
            )],
        )
        .unwrap();

        run_with_hook_input("precompact", None, tmp.path()).unwrap();

        let session = crate::graph::v2::input::load_session(tmp.path(), 1).unwrap();
        assert_eq!(session.session_id, "");
        assert!(session.turns.is_empty());
        let archive =
            fs::read_to_string(tmp.path().join("conversations/conversation-001.md")).unwrap();
        assert!(archive.contains("No transcript available"));
        let index = fs::read_to_string(tmp.path().join("ARCHIVE.md")).unwrap();
        assert!(index.contains("| 001 |"));
    }

    /// AC7: a sidecar write that cannot create the file keeps the markdown and index.
    #[test]
    fn a_failed_sidecar_write_keeps_the_markdown_and_index() {
        let tmp = prepared_root();
        let turns = tmp.path().join("turns");
        fs::create_dir_all(&turns).unwrap();
        let mut perms = fs::metadata(&turns).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&turns, perms).unwrap();

        let lines = [
            r#"{"type":"user","timestamp":"2026-03-05T14:30:00.100Z","sessionId":"sess-a","message":{"role":"user","content":"alpha"}}"#,
        ];
        let input = hook_for(tmp.path(), "sess-a", &lines);
        run_with_hook_input("precompact", Some(&input), tmp.path()).unwrap();

        let archive =
            fs::read_to_string(tmp.path().join("conversations/conversation-001.md")).unwrap();
        assert!(archive.contains("session_id: \"sess-a\""));
        assert!(archive.contains("alpha"));
        let index = fs::read_to_string(tmp.path().join("ARCHIVE.md")).unwrap();
        assert!(index.contains("sess-a"));
        assert!(!tmp.path().join("turns/conversation-001.json").exists());
    }

    /// A PreCompact checkpoint racing an archive takes a different number,
    /// and the checkpoint's `log:` is that number.
    #[test]
    fn a_checkpoint_and_an_archive_take_distinct_numbers() {
        let tmp = prepared_root();
        let memory = tmp.path().to_path_buf();
        fs::write(
            memory.join("ARCHIVE.md"),
            "# Conversation Archive\n\n| # | Date | Session | Topics | Messages | Duration |\n|---|------|---------|--------|----------|----------|\n",
        )
        .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let iterations = 4u32;
        let archive_memory = memory.clone();
        let archive_barrier = std::sync::Arc::clone(&barrier);
        let archived = std::thread::spawn(move || {
            for i in 0..iterations {
                archive_barrier.wait();
                let mut conv = crate::conversation::Conversation::new(&format!("sess-{i}"));
                conv.user_message_count = 1;
                conv.entries
                    .push(crate::conversation::ConversationEntry::UserMessage(
                        "hello".into(),
                    ));
                crate::archive::archive_conversation(
                    &archive_memory,
                    &conv,
                    &crate::summarize::ConversationSummary::default(),
                    "jsonl",
                )
                .unwrap();
            }
        });
        for i in 0..iterations {
            barrier.wait();
            let line = format!(
                r#"{{"type":"user","timestamp":"2026-03-05T14:30:00.100Z","sessionId":"alpha-{i}","message":{{"role":"user","content":"hello"}}}}"#
            );
            let input = hook_for(&memory, &format!("alpha-{i}"), &[&line]);
            run_with_hook_input("precompact", Some(&input), &memory).unwrap();
        }
        archived.join().unwrap();

        let mut numbers = Vec::new();
        for entry in fs::read_dir(memory.join("conversations"))
            .unwrap()
            .flatten()
        {
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
            if header.source == "precompact" {
                assert!(header.session_id.starts_with("alpha-"), "{body}");
            }
            numbers.push(number);
        }
        numbers.sort_unstable();
        assert_eq!(numbers.len(), 8, "{numbers:?}");
        numbers.dedup();
        assert_eq!(numbers.len(), 8, "{numbers:?}");
    }
}
