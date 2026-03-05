use std::fs;
use std::path::Path;

use crate::archive;
use crate::frontmatter::Frontmatter;
use crate::jsonl;
use crate::paths;

pub fn log_body(log_num: u32) -> String {
    format!(
        "\n# Conversation {log_num:03}\n\n\
         ## Summary\n\n\
         <!-- Fill in: What was discussed, decided, and accomplished -->\n\n\
         ## Key Details\n\n\
         <!-- Fill in: Important specifics — code changes, configurations, decisions -->\n\n\
         ## Action Items\n\n\
         <!-- Fill in: What needs to happen next -->\n\n\
         ## Unresolved\n\n\
         <!-- Fill in: Open questions or threads to pick up later -->\n"
    )
}

fn run_with_paths(
    conversations_dir: &Path,
    archive_index: &Path,
    trigger: &str,
    context: &str,
) -> Result<(), String> {
    if !conversations_dir.exists() {
        return Err(
            "conversations/ directory not found. Run `recall-echo init` first.".to_string(),
        );
    }

    let next_num = archive::highest_conversation_number(conversations_dir) + 1;
    let date = jsonl::utc_now();
    let date_short = date.split('T').next().unwrap_or(&date);

    let fm = Frontmatter {
        log: next_num,
        date: date.clone(),
        session_id: String::new(),
        message_count: 0,
        duration: String::new(),
        source: trigger.to_string(),
        topics: vec![],
    };

    let content = format!("{}\n{}", fm.render(), log_body(next_num));
    let log_path = conversations_dir.join(format!("conversation-{next_num:03}.md"));

    fs::write(&log_path, &content)
        .map_err(|e| format!("Failed to create conversation file: {e}"))?;

    archive::append_index(archive_index, next_num, date_short, "", &[], 0, "")?;

    let path_display = log_path.to_string_lossy();
    println!("RECALL-ECHO checkpoint: {path_display}");
    println!("Trigger: {trigger} | Log: {next_num:03} | Date: {date}");
    if !context.is_empty() {
        println!("Context: {context}");
    }
    println!("Fill in the Summary, Key Details, Action Items, and Unresolved sections.");

    Ok(())
}

pub fn run(trigger: &str, context: &str) -> Result<(), String> {
    let conversations_dir = paths::conversations_dir()?;
    let archive_index = paths::archive_index()?;
    run_with_paths(&conversations_dir, &archive_index, trigger, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let conversations = tmp.path().join("conversations");
        let archive = tmp.path().join("ARCHIVE.md");
        fs::create_dir_all(&conversations).unwrap();
        (tmp, conversations, archive)
    }

    #[test]
    fn creates_logs_and_sequences() {
        let (_tmp, conversations, archive) = setup_test_dirs();

        run_with_paths(&conversations, &archive, "precompact", "test session").unwrap();

        let log = conversations.join("conversation-001.md");
        assert!(log.exists());

        let content = fs::read_to_string(&log).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("log: 1"));
        assert!(content.contains("source: \"precompact\""));
        assert!(content.contains("# Conversation 001"));
        assert!(content.contains("## Summary"));

        // Second checkpoint — should get next number
        run_with_paths(&conversations, &archive, "session-end", "").unwrap();

        assert!(conversations.join("conversation-002.md").exists());

        let content = fs::read_to_string(conversations.join("conversation-002.md")).unwrap();
        assert!(content.contains("log: 2"));
    }
}
