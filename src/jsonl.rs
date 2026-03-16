//! JSONL transcript parsing for Claude Code sessions.
//!
//! Parses Claude Code's `.jsonl` transcript files into the universal
//! `Conversation` format. This is the input adapter for standalone
//! (non-pulse-null) usage — e.g., when recall-echo is used as a
//! Claude Code hook.

use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use crate::conversation::{Conversation, ConversationEntry};

// ---------------------------------------------------------------------------
// Hook input (stdin from Claude Code)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct HookInput {
    pub session_id: String,
    pub transcript_path: String,
    #[allow(dead_code)]
    pub cwd: Option<String>,
    #[allow(dead_code)]
    pub hook_event_name: Option<String>,
}

pub fn read_hook_input() -> Result<HookInput, String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("Failed to read stdin: {e}"))?;

    if buf.trim().is_empty() {
        return Err(
            "No input on stdin. This command is called by the Claude Code SessionEnd hook."
                .to_string(),
        );
    }

    serde_json::from_str(&buf).map_err(|e| format!("Invalid hook JSON on stdin: {e}"))
}

// ---------------------------------------------------------------------------
// JSONL entry types (deserialization)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonlEntry {
    #[serde(rename = "type")]
    entry_type: String,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    #[allow(dead_code)]
    session_id: Option<String>,
    message: Option<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    role: Option<String>,
    content: Option<ContentValue>,
    #[allow(dead_code)]
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ContentValue {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

// ---------------------------------------------------------------------------
// JSONL parsing
// ---------------------------------------------------------------------------

/// Parse a Claude Code JSONL transcript into a Conversation.
pub fn parse_transcript(path: &str, session_id: &str) -> Result<Conversation, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open transcript {path}: {e}"))?;
    let reader = BufReader::new(file);

    let mut conv = Conversation::new(session_id);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        let entry: JsonlEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("recall-echo: skipping malformed JSONL line: {e}");
                continue;
            }
        };

        // Skip system entries
        if entry.entry_type == "queue-operation" || entry.entry_type == "summary" {
            continue;
        }

        // Track timestamps
        if let Some(ref ts) = entry.timestamp {
            if conv.first_timestamp.is_none() {
                conv.first_timestamp = Some(ts.clone());
            }
            conv.last_timestamp = Some(ts.clone());
        }

        // Only process entries with messages
        let msg = match entry.message {
            Some(m) => m,
            None => continue,
        };

        let role = msg.role.as_deref().unwrap_or("");
        let content = match msg.content {
            Some(c) => c,
            None => continue,
        };

        match role {
            "user" => parse_user_content(&mut conv, content),
            "assistant" => parse_assistant_content(&mut conv, content),
            _ => {}
        }
    }

    Ok(conv)
}

fn parse_user_content(conv: &mut Conversation, content: ContentValue) {
    match content {
        ContentValue::Text(text) => {
            conv.user_message_count += 1;
            conv.entries.push(ConversationEntry::UserMessage(text));
        }
        ContentValue::Blocks(blocks) => {
            for block in blocks {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if block_type == "tool_result" {
                    let raw_content = block.get("content");
                    let text = match raw_content {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                        None => String::new(),
                    };
                    let is_error = block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    conv.entries.push(ConversationEntry::ToolResult {
                        content: crate::conversation::truncate(&text, 2000),
                        is_error,
                    });
                }
            }
        }
    }
}

fn parse_assistant_content(conv: &mut Conversation, content: ContentValue) {
    match content {
        ContentValue::Text(text) => {
            conv.assistant_message_count += 1;
            conv.entries.push(ConversationEntry::AssistantText(text));
        }
        ContentValue::Blocks(blocks) => {
            for block in blocks {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                conv.assistant_message_count += 1;
                                conv.entries
                                    .push(ConversationEntry::AssistantText(text.to_string()));
                            }
                        }
                    }
                    "tool_use" => {
                        let name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let input = block.get("input");
                        let summary = format_tool_input(&name, input);
                        conv.entries.push(ConversationEntry::ToolUse {
                            name,
                            input_summary: summary,
                        });
                    }
                    // Skip thinking blocks entirely (private reasoning + signatures)
                    "thinking" => {}
                    _ => {}
                }
            }
        }
    }
}

fn format_tool_input(name: &str, input: Option<&serde_json::Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return String::new(),
    };

    match name {
        "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| format!("`{p}`"))
            .unwrap_or_default(),
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| format!("`{}`", crate::conversation::truncate(c, 200)))
            .unwrap_or_default(),
        "Edit" | "Write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| format!("`{p}`"))
            .unwrap_or_default(),
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            format!("`{pattern}` in `{path}`")
        }
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| format!("`{p}`"))
            .unwrap_or_default(),
        _ => {
            let s = serde_json::to_string(input).unwrap_or_default();
            crate::conversation::truncate(&s, 200)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_jsonl(dir: &std::path::Path) -> String {
        let path = dir.join("test-session.jsonl");
        let mut f = File::create(&path).unwrap();
        let lines = [
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-03-05T14:30:00.000Z","sessionId":"test-sess-1"}"#,
            r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-03-05T14:30:00.001Z","sessionId":"test-sess-1"}"#,
            r#"{"parentUuid":null,"type":"user","sessionId":"test-sess-1","timestamp":"2026-03-05T14:30:00.100Z","message":{"role":"user","content":"Can you read the auth module?"}}"#,
            r#"{"parentUuid":"aaa","type":"assistant","sessionId":"test-sess-1","timestamp":"2026-03-05T14:30:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me check the auth module.","signature":"sig123"}]}}"#,
            r#"{"parentUuid":"bbb","type":"assistant","sessionId":"test-sess-1","timestamp":"2026-03-05T14:30:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Let me read the auth module."}]}}"#,
            r#"{"parentUuid":"ccc","type":"assistant","sessionId":"test-sess-1","timestamp":"2026-03-05T14:30:07.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{"file_path":"/src/auth.rs"}}]}}"#,
            r#"{"parentUuid":"ddd","type":"user","sessionId":"test-sess-1","timestamp":"2026-03-05T14:30:08.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"pub fn authenticate() {\n    // auth logic\n}"}]}}"#,
            r#"{"parentUuid":"eee","type":"assistant","sessionId":"test-sess-1","timestamp":"2026-03-05T14:31:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"The auth module has a single authenticate function."}]}}"#,
        ];
        for line in &lines {
            writeln!(f, "{}", line).unwrap();
        }
        path.to_string_lossy().to_string()
    }

    #[test]
    fn parse_transcript_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_jsonl(dir.path());
        let conv = parse_transcript(&path, "test-sess-1").unwrap();

        assert_eq!(conv.session_id, "test-sess-1");
        assert_eq!(conv.user_message_count, 1);
        assert_eq!(conv.assistant_message_count, 2);
        assert!(conv.first_timestamp.is_some());
        assert!(conv.last_timestamp.is_some());

        // Should have: UserMessage, AssistantText, ToolUse, ToolResult, AssistantText
        assert_eq!(conv.entries.len(), 5);
    }

    #[test]
    fn thinking_blocks_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_jsonl(dir.path());
        let conv = parse_transcript(&path, "test-sess-1").unwrap();

        for entry in &conv.entries {
            if let ConversationEntry::AssistantText(text) = entry {
                assert!(!text.contains("Let me check the auth module"));
            }
        }
    }

    #[test]
    fn conversation_to_markdown_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_jsonl(dir.path());
        let conv = parse_transcript(&path, "test-sess-1").unwrap();
        let md = crate::conversation::conversation_to_markdown(&conv, 1);

        assert!(md.starts_with("# Conversation 001"));
        assert!(md.contains("### User"));
        assert!(md.contains("Can you read the auth module?"));
        assert!(md.contains("### Assistant"));
        assert!(md.contains("**Read**"));
        assert!(md.contains("`/src/auth.rs`"));
        assert!(md.contains("authenticate"));
        // Thinking block should NOT appear
        assert!(!md.contains("Let me check the auth module"));
    }

    #[test]
    fn extract_summary_strips_channel_prefix() {
        let conv = Conversation {
            session_id: "test".to_string(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: 1,
            assistant_message_count: 0,
            entries: vec![ConversationEntry::UserMessage(
                "[Channel: discord | Trust: VERIFIED]\n\nUser message: lets build something"
                    .to_string(),
            )],
        };
        let summary = crate::conversation::extract_summary(&conv);
        assert_eq!(summary, "lets build something");
    }

    #[test]
    fn extract_topics_basic() {
        let conv = Conversation {
            session_id: "test".to_string(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: 1,
            assistant_message_count: 0,
            entries: vec![ConversationEntry::UserMessage(
                "Can you refactor the auth module to use JWT tokens instead of sessions?"
                    .to_string(),
            )],
        };
        let topics = crate::conversation::extract_topics(&conv, 5);
        assert!(topics.contains(&"auth".to_string()));
        assert!(topics.contains(&"jwt".to_string()));
    }

    #[test]
    fn tool_result_truncation() {
        let long_content = "x".repeat(3000);
        let truncated = crate::conversation::truncate(&long_content, 2000);
        assert!(truncated.len() < 3000);
        assert!(truncated.contains("[truncated, 3000 chars total]"));
    }

    #[test]
    fn format_tool_input_read() {
        let input: serde_json::Value = serde_json::json!({"file_path": "/src/main.rs"});
        assert_eq!(format_tool_input("Read", Some(&input)), "`/src/main.rs`");
    }

    #[test]
    fn format_tool_input_grep() {
        let input: serde_json::Value = serde_json::json!({"pattern": "TODO", "path": "/src/"});
        assert_eq!(format_tool_input("Grep", Some(&input)), "`TODO` in `/src/`");
    }
}
