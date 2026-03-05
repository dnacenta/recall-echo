use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
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
// Parsed conversation types
// ---------------------------------------------------------------------------

pub struct Conversation {
    pub session_id: String,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub user_message_count: u32,
    pub assistant_message_count: u32,
    pub entries: Vec<ConversationEntry>,
}

pub enum ConversationEntry {
    UserMessage(String),
    AssistantText(String),
    ToolUse { name: String, input_summary: String },
    ToolResult { content: String, is_error: bool },
}

// ---------------------------------------------------------------------------
// JSONL parsing
// ---------------------------------------------------------------------------

pub fn parse_transcript(path: &str, session_id: &str) -> Result<Conversation, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open transcript {path}: {e}"))?;
    let reader = BufReader::new(file);

    let mut conv = Conversation {
        session_id: session_id.to_string(),
        first_timestamp: None,
        last_timestamp: None,
        user_message_count: 0,
        assistant_message_count: 0,
        entries: Vec::new(),
    };

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
                        content: truncate(&text, 2000),
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
            .map(|c| format!("`{}`", truncate(c, 200)))
            .unwrap_or_default(),
        "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| format!("`{p}`"))
            .unwrap_or_default(),
        "Write" => input
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
            truncate(&s, 200)
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let total = s.len();
        format!("{}...\n\n[truncated, {total} chars total]", &s[..max])
    }
}

// ---------------------------------------------------------------------------
// Markdown conversion
// ---------------------------------------------------------------------------

pub fn conversation_to_markdown(conv: &Conversation, log_num: u32) -> String {
    let mut md = format!("# Conversation {log_num:03}\n");

    for entry in &conv.entries {
        match entry {
            ConversationEntry::UserMessage(text) => {
                md.push_str("\n---\n\n### User\n\n");
                md.push_str(text);
                md.push('\n');
            }
            ConversationEntry::AssistantText(text) => {
                md.push_str("\n---\n\n### Assistant\n\n");
                md.push_str(text);
                md.push('\n');
            }
            ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                md.push_str(&format!("\n**Tool: {name}** {input_summary}\n"));
            }
            ConversationEntry::ToolResult { content, is_error } => {
                if *is_error {
                    md.push_str("\n**Tool Result (error)**\n\n```\n");
                } else {
                    md.push_str("\n**Tool Result**\n\n```\n");
                }
                md.push_str(content);
                md.push_str("\n```\n");
            }
        }
    }

    md
}

// ---------------------------------------------------------------------------
// Summary extraction
// ---------------------------------------------------------------------------

pub fn extract_summary(conv: &Conversation) -> String {
    for entry in &conv.entries {
        if let ConversationEntry::UserMessage(text) = entry {
            let cleaned = strip_channel_prefix(text);
            if cleaned.is_empty() {
                continue;
            }
            return truncate_clean(&cleaned, 200);
        }
    }
    "Empty session".to_string()
}

fn strip_channel_prefix(text: &str) -> String {
    let mut s = text.trim().to_string();

    // Strip [Channel: ...] prefix
    if s.starts_with('[') {
        if let Some(end) = s.find("]\n") {
            s = s[end + 2..].trim().to_string();
        } else if let Some(end) = s.find("] ") {
            s = s[end + 2..].trim().to_string();
        }
    }

    // Strip "User message: " prefix (bridge-formatted)
    if let Some(rest) = s.strip_prefix("User message: ") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("User message:") {
        s = rest.trim().to_string();
    }

    s
}

fn truncate_clean(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ---------------------------------------------------------------------------
// Topic extraction
// ---------------------------------------------------------------------------

const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "to", "of", "in", "for", "on", "with", "that",
    "this", "it", "i", "you", "we", "my", "can", "do", "how", "what", "and", "or", "but", "not",
    "be", "have", "has", "had", "at", "by", "from", "up", "about", "into", "over", "after", "if",
    "me", "your", "our", "let", "just", "like", "also", "some", "all", "any", "should", "would",
    "could", "will", "need", "want", "look", "use", "make", "know", "get", "see", "think", "take",
    "come", "there", "here", "when", "where", "which", "who", "them", "then", "than", "been",
    "its", "does", "did", "done", "going", "way", "now", "new", "one", "two",
];

pub fn extract_topics(conv: &Conversation, max_topics: usize) -> Vec<String> {
    let mut word_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut user_msg_count = 0;

    // Extract keywords from user messages
    for entry in &conv.entries {
        if let ConversationEntry::UserMessage(text) = entry {
            let cleaned = strip_channel_prefix(text);
            for word in cleaned.split_whitespace() {
                let w: String = word
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                    .collect::<String>()
                    .to_lowercase();
                if w.len() >= 3 && !STOP_WORDS.contains(&w.as_str()) {
                    *word_counts.entry(w).or_insert(0) += 1;
                }
            }
            user_msg_count += 1;
            if user_msg_count >= 5 {
                break;
            }
        }
    }

    // Extract file/directory names from tool uses (high signal)
    for entry in &conv.entries {
        if let ConversationEntry::ToolUse { input_summary, .. } = entry {
            // Extract meaningful path components from tool summaries
            let summary = input_summary.trim_matches('`');
            if let Some(basename) = summary.rsplit('/').next() {
                let name = basename
                    .split('.')
                    .next()
                    .unwrap_or(basename)
                    .to_lowercase();
                if name.len() >= 3 && !STOP_WORDS.contains(&name.as_str()) {
                    *word_counts.entry(name).or_insert(0) += 2; // Boost tool targets
                }
            }
        }
    }

    let mut words: Vec<(String, u32)> = word_counts.into_iter().collect();
    words.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    words.into_iter().take(max_topics).map(|(w, _)| w).collect()
}

// ---------------------------------------------------------------------------
// Duration calculation
// ---------------------------------------------------------------------------

pub fn calculate_duration(first: &str, last: &str) -> String {
    let t1 = parse_timestamp(first);
    let t2 = parse_timestamp(last);

    match (t1, t2) {
        (Some(a), Some(b)) => {
            let diff = b.abs_diff(a);
            format_duration(diff)
        }
        _ => "unknown".to_string(),
    }
}

/// Parse ISO 8601 timestamp to seconds since midnight UTC.
fn parse_timestamp(ts: &str) -> Option<u64> {
    let t_pos = ts.find('T')?;
    let date_part = &ts[..t_pos];
    let time_part = ts[t_pos + 1..].trim_end_matches('Z');

    let date_parts: Vec<&str> = date_part.split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let year: u64 = date_parts[0].parse().ok()?;
    let month: u64 = date_parts[1].parse().ok()?;
    let day: u64 = date_parts[2].parse().ok()?;

    let time_clean = time_part.split('.').next()?;
    let time_parts: Vec<&str> = time_clean.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let min: u64 = time_parts[1].parse().ok()?;
    let sec: u64 = time_parts[2].parse().ok()?;

    Some(((year * 365 + month * 30 + day) * 86400) + hour * 3600 + min * 60 + sec)
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        "< 1m".to_string()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        let h = seconds / 3600;
        let m = (seconds % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m:02}m")
        }
    }
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

pub fn utc_now() -> String {
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();

    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Extract just the date portion (YYYY-MM-DD) from an ISO 8601 timestamp
pub fn date_from_timestamp(ts: &str) -> String {
    ts.split('T').next().unwrap_or("unknown").to_string()
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
        let md = conversation_to_markdown(&conv, 1);

        assert!(md.starts_with("# Conversation 001"));
        assert!(md.contains("### User"));
        assert!(md.contains("Can you read the auth module?"));
        assert!(md.contains("### Assistant"));
        assert!(md.contains("**Tool: Read**"));
        assert!(md.contains("`/src/auth.rs`"));
        assert!(md.contains("**Tool Result**"));
        assert!(md.contains("authenticate"));
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
        let summary = extract_summary(&conv);
        assert_eq!(summary, "lets build something");
    }

    #[test]
    fn extract_summary_empty_session() {
        let conv = Conversation {
            session_id: "test".to_string(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: 0,
            assistant_message_count: 0,
            entries: vec![],
        };
        assert_eq!(extract_summary(&conv), "Empty session");
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
        let topics = extract_topics(&conv, 5);
        assert!(topics.contains(&"auth".to_string()));
        assert!(topics.contains(&"jwt".to_string()));
    }

    #[test]
    fn calculate_duration_basic() {
        assert_eq!(
            calculate_duration("2026-03-05T14:30:00.000Z", "2026-03-05T15:15:00.000Z"),
            "45m"
        );
    }

    #[test]
    fn calculate_duration_short() {
        assert_eq!(
            calculate_duration("2026-03-05T14:30:00.000Z", "2026-03-05T14:30:30.000Z"),
            "< 1m"
        );
    }

    #[test]
    fn calculate_duration_hours() {
        assert_eq!(
            calculate_duration("2026-03-05T14:00:00.000Z", "2026-03-05T16:30:00.000Z"),
            "2h30m"
        );
    }

    #[test]
    fn calculate_duration_invalid() {
        assert_eq!(calculate_duration("garbage", "nonsense"), "unknown");
    }

    #[test]
    fn tool_result_truncation() {
        let long_content = "x".repeat(3000);
        let truncated = truncate(&long_content, 2000);
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
