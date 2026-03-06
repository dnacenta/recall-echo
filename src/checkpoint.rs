//! Checkpoint creation — saves a conversation snapshot before context compression.
//!
//! Called by pulse-null's session/context manager when the context window
//! is getting long. Similar to archive but marks source as "checkpoint".

use std::fs;
use std::path::Path;

use echo_system_types::llm::{LmProvider, Message};

use crate::archive::{self, SessionMetadata};
use crate::conversation;
use crate::frontmatter::Frontmatter;
use crate::summarize;
use crate::tags;

/// Create a checkpoint archive from the current conversation state.
///
/// Returns the conversation number of the created checkpoint.
pub async fn create_checkpoint(
    memory_dir: &Path,
    messages: &[Message],
    metadata: &SessionMetadata,
    provider: Option<&dyn LmProvider>,
) -> Result<u32, String> {
    let conversations_dir = memory_dir.join("conversations");
    let archive_index = memory_dir.join("ARCHIVE.md");

    if !conversations_dir.exists() {
        return Err("conversations/ directory not found. Run init first.".to_string());
    }

    let (user_count, _) = conversation::count_messages(messages);
    if user_count == 0 {
        return Ok(0);
    }

    let next_num = archive::highest_conversation_number(&conversations_dir) + 1;

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

    let fm = Frontmatter {
        log: next_num,
        date: now,
        session_id: metadata.session_id.clone(),
        message_count: total_messages,
        duration: duration.clone(),
        source: "checkpoint".to_string(),
        topics: summary.topics.clone(),
    };

    let md_body = conversation::conversation_to_markdown(messages, next_num);
    let entries = conversation::flatten_messages(messages);
    let conv_tags = tags::extract_tags(&entries);
    let tags_section = tags::format_tags_section(&conv_tags);

    let summary_section = if !summary.summary.is_empty() {
        format!("## Summary\n\n{}\n\n", summary.summary)
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

    let log_path = conversations_dir.join(format!("conversation-{next_num:03}.md"));
    fs::write(&log_path, &full_content)
        .map_err(|e| format!("Failed to create checkpoint file: {e}"))?;

    archive::append_index(
        &archive_index,
        next_num,
        &date,
        &metadata.session_id,
        &summary.topics,
        total_messages,
        &duration,
    )?;

    eprintln!(
        "recall-echo: checkpoint conversation-{:03}.md ({} messages)",
        next_num, total_messages
    );

    Ok(next_num)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_memory_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        (tmp, memory)
    }

    #[tokio::test]
    async fn checkpoint_creates_file_and_index() {
        let (_tmp, memory) = setup_memory_dir();

        let msgs = vec![
            echo_system_types::llm::Message {
                role: echo_system_types::llm::Role::User,
                content: echo_system_types::llm::MessageContent::Text(
                    "Set up the auth module".to_string(),
                ),
            },
            echo_system_types::llm::Message {
                role: echo_system_types::llm::Role::Assistant,
                content: echo_system_types::llm::MessageContent::Text(
                    "I'll implement JWT authentication.".to_string(),
                ),
            },
        ];

        let meta = SessionMetadata {
            session_id: "test-session".to_string(),
            started_at: Some("2026-03-06T10:00:00Z".to_string()),
            ended_at: Some("2026-03-06T10:30:00Z".to_string()),
            entity_name: "nova".to_string(),
        };

        let num = create_checkpoint(&memory, &msgs, &meta, None)
            .await
            .unwrap();
        assert_eq!(num, 1);
        assert!(memory.join("conversations/conversation-001.md").exists());

        let content = fs::read_to_string(memory.join("conversations/conversation-001.md")).unwrap();
        assert!(content.contains("source: \"checkpoint\""));

        let index = fs::read_to_string(memory.join("ARCHIVE.md")).unwrap();
        assert!(index.contains("001"));
    }

    #[tokio::test]
    async fn checkpoint_skips_empty_conversation() {
        let (_tmp, memory) = setup_memory_dir();

        let meta = SessionMetadata {
            session_id: "empty".to_string(),
            started_at: None,
            ended_at: None,
            entity_name: "nova".to_string(),
        };

        let num = create_checkpoint(&memory, &[], &meta, None).await.unwrap();
        assert_eq!(num, 0);
    }
}
