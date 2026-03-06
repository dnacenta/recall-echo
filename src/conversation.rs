//! Conversation processing for pulse-null entities.
//!
//! Converts `Vec<Message>` (from echo-system-types) into markdown archives,
//! and provides algorithmic topic/summary extraction as a fallback when
//! no LLM provider is available.

use std::collections::HashMap;

use echo_system_types::llm::{ContentBlock, Message, MessageContent, Role};

/// A parsed conversation entry for tag extraction and rendering.
#[derive(Debug, Clone)]
pub enum ConversationEntry {
    UserMessage(String),
    AssistantText(String),
    ToolUse { name: String, input_summary: String },
    ToolResult { content: String, is_error: bool },
}

/// Flatten `Vec<Message>` into a sequence of `ConversationEntry` items.
pub fn flatten_messages(messages: &[Message]) -> Vec<ConversationEntry> {
    let mut entries = Vec::new();
    for msg in messages {
        let role = &msg.role;
        match &msg.content {
            MessageContent::Text(text) => {
                if text.trim().is_empty() {
                    continue;
                }
                match role {
                    Role::User => entries.push(ConversationEntry::UserMessage(text.clone())),
                    Role::Assistant => entries.push(ConversationEntry::AssistantText(text.clone())),
                }
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if text.trim().is_empty() {
                                continue;
                            }
                            match role {
                                Role::User => {
                                    entries.push(ConversationEntry::UserMessage(text.clone()))
                                }
                                Role::Assistant => {
                                    entries.push(ConversationEntry::AssistantText(text.clone()))
                                }
                            }
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            let summary = summarize_tool_input(name, input);
                            entries.push(ConversationEntry::ToolUse {
                                name: name.clone(),
                                input_summary: summary,
                            });
                        }
                        ContentBlock::ToolResult {
                            content, is_error, ..
                        } => {
                            entries.push(ConversationEntry::ToolResult {
                                content: content.clone(),
                                is_error: is_error.unwrap_or(false),
                            });
                        }
                    }
                }
            }
        }
    }
    entries
}

/// Summarize tool input JSON into a brief human-readable string.
fn summarize_tool_input(name: &str, input: &serde_json::Value) -> String {
    // Extract the most relevant field based on tool name
    if let Some(path) = input.get("file_path").or(input.get("path")) {
        if let Some(s) = path.as_str() {
            return s.to_string();
        }
    }
    if let Some(cmd) = input.get("command") {
        if let Some(s) = cmd.as_str() {
            let truncated: String = s.chars().take(80).collect();
            return truncated;
        }
    }
    if let Some(pattern) = input.get("pattern") {
        if let Some(s) = pattern.as_str() {
            return format!("pattern: {s}");
        }
    }
    if let Some(query) = input.get("query") {
        if let Some(s) = query.as_str() {
            return format!("query: {s}");
        }
    }
    if let Some(url) = input.get("url") {
        if let Some(s) = url.as_str() {
            return s.to_string();
        }
    }
    format!("{name}(…)")
}

/// Convert messages into a markdown document for archival.
pub fn conversation_to_markdown(messages: &[Message], log_num: u32) -> String {
    let mut md = format!("# Conversation {log_num:03}\n\n");
    let entries = flatten_messages(messages);
    let mut last_role: Option<&str> = None;

    for entry in &entries {
        match entry {
            ConversationEntry::UserMessage(text) => {
                if last_role != Some("user") {
                    md.push_str("---\n\n### User\n\n");
                }
                md.push_str(text);
                md.push_str("\n\n");
                last_role = Some("user");
            }
            ConversationEntry::AssistantText(text) => {
                if last_role != Some("assistant") {
                    md.push_str("---\n\n### Assistant\n\n");
                }
                md.push_str(text);
                md.push_str("\n\n");
                last_role = Some("assistant");
            }
            ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                md.push_str(&format!("> **{name}**: `{input_summary}`\n\n"));
            }
            ConversationEntry::ToolResult { content, is_error } => {
                let label = if *is_error { "Error" } else { "Result" };
                let truncated = if content.len() > 2000 {
                    format!("{}… (truncated)", &content[..2000])
                } else {
                    content.clone()
                };
                md.push_str(&format!("<details><summary>{label}</summary>\n\n```\n{truncated}\n```\n\n</details>\n\n"));
            }
        }
    }

    md
}

/// Count user and assistant messages.
pub fn count_messages(messages: &[Message]) -> (u32, u32) {
    let mut user = 0u32;
    let mut assistant = 0u32;
    for msg in messages {
        match msg.role {
            Role::User => user += 1,
            Role::Assistant => assistant += 1,
        }
    }
    (user, assistant)
}

/// Extract text content from all user messages.
fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .filter_map(|m| match &m.content {
            MessageContent::Text(t) => Some(t.clone()),
            MessageContent::Blocks(blocks) => {
                let texts: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join(" "))
                }
            }
        })
        .collect()
}

// Words to skip during topic extraction
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "to",
    "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "about", "like", "through",
    "after", "over", "between", "out", "up", "down", "off", "then", "than", "too", "very", "just",
    "also", "not", "no", "but", "or", "and", "if", "so", "yet", "both", "this", "that", "these",
    "those", "it", "its", "i", "you", "we", "they", "he", "she", "me", "my", "your", "our",
    "their", "him", "her", "us", "them", "what", "which", "who", "when", "where", "how", "why",
    "all", "each", "every", "some", "any", "most", "other", "new", "old", "first", "last", "next",
    "now", "here", "there", "only", "one", "two", "get", "got", "make", "made", "let", "let's",
    "use", "need", "want", "know", "think", "see", "look", "find", "give", "tell", "say", "said",
    "go", "going", "come", "take", "thing", "things", "way", "work", "right", "good", "yeah",
    "yes", "okay", "ok", "sure", "well", "don't", "doesn't", "didn't", "can't", "won't", "isn't",
    "aren't", "wasn't", "file", "code", "run", "set", "add", "put", "try",
];

/// Algorithmic topic extraction from user messages.
/// Uses keyword frequency with stop-word filtering and tool-target boosting.
pub fn extract_topics_algorithmic(messages: &[Message], max: usize) -> Vec<String> {
    let mut freq: HashMap<String, u32> = HashMap::new();
    let texts = user_texts(messages);

    // Count words from first 5 user messages
    for text in texts.iter().take(5) {
        for word in text.split_whitespace() {
            let clean: String = word
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if clean.len() >= 3 && !STOP_WORDS.contains(&clean.as_str()) {
                *freq.entry(clean).or_default() += 1;
            }
        }
    }

    // Boost tool targets (file paths, commands)
    let entries = flatten_messages(messages);
    for entry in &entries {
        if let ConversationEntry::ToolUse {
            input_summary,
            name,
            ..
        } = entry
        {
            // Extract filename or last path component
            let target = input_summary
                .rsplit('/')
                .next()
                .unwrap_or(input_summary)
                .trim_matches('`')
                .to_lowercase();
            if target.len() >= 3 && !target.contains('(') {
                // Remove extension for cleaner topics
                let stem = target.split('.').next().unwrap_or(&target);
                if !stem.is_empty() {
                    *freq.entry(stem.to_string()).or_default() += 2;
                }
            }
            // Boost the tool name itself
            let tool_lower = name.to_lowercase();
            if !STOP_WORDS.contains(&tool_lower.as_str()) {
                *freq.entry(tool_lower).or_default() += 1;
            }
        }
    }

    let mut sorted: Vec<(String, u32)> = freq.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.into_iter().take(max).map(|(k, _)| k).collect()
}

/// Algorithmic summary extraction — first user message, truncated.
pub fn extract_summary_algorithmic(messages: &[Message]) -> String {
    let texts = user_texts(messages);
    match texts.first() {
        Some(text) => {
            // Strip channel prefixes like "[Channel: discord | ...]"
            let cleaned = if text.starts_with('[') {
                text.find("]\n")
                    .or(text.find("] "))
                    .map(|pos| text[pos + 1..].trim())
                    .unwrap_or(text)
            } else {
                text
            };
            let truncated: String = cleaned.chars().take(120).collect();
            if truncated.len() < cleaned.len() {
                format!("{truncated}…")
            } else {
                truncated
            }
        }
        None => String::new(),
    }
}

/// Get current UTC timestamp in ISO 8601 format.
pub fn utc_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_per_day = 86400u64;
    let days = now / secs_per_day;
    let day_secs = now % secs_per_day;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;

    // Simple date calculation
    let mut y = 1970i64;
    let mut remaining_days = days as i64;
    loop {
        let year_days = if is_leap(y) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md {
            m = i;
            break;
        }
        remaining_days -= md;
    }
    let d = remaining_days + 1;
    format!(
        "{y:04}-{:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z",
        m + 1
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Extract just the date portion from an ISO timestamp.
pub fn date_from_timestamp(ts: &str) -> String {
    ts.split('T').next().unwrap_or(ts).to_string()
}

/// Calculate duration string from two ISO timestamps.
pub fn calculate_duration(start: &str, end: &str) -> String {
    fn parse_secs(ts: &str) -> Option<u64> {
        // Parse "YYYY-MM-DDTHH:MM:SSZ" or similar
        let t = ts.find('T')?;
        let time_part = &ts[t + 1..];
        let parts: Vec<&str> = time_part
            .trim_end_matches('Z')
            .trim_end_matches("+00:00")
            .split(':')
            .collect();
        if parts.len() >= 3 {
            let h: u64 = parts[0].parse().ok()?;
            let m: u64 = parts[1].parse().ok()?;
            let s: u64 = parts[2].split('.').next()?.parse().ok()?;
            Some(h * 3600 + m * 60 + s)
        } else {
            None
        }
    }

    match (parse_secs(start), parse_secs(end)) {
        (Some(s), Some(e)) if e >= s => {
            let diff = e - s;
            let hours = diff / 3600;
            let mins = (diff % 3600) / 60;
            if hours > 0 {
                format!("{hours}h{mins:02}m")
            } else {
                format!("{mins}m")
            }
        }
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
        }
    }

    fn make_assistant_msg(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Text(text.to_string()),
        }
    }

    fn make_tool_use_msg(name: &str, path: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: name.to_string(),
                input: serde_json::json!({"file_path": path}),
            }]),
        }
    }

    #[test]
    fn flatten_simple_messages() {
        let msgs = vec![make_user_msg("hello"), make_assistant_msg("hi there")];
        let entries = flatten_messages(&msgs);
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], ConversationEntry::UserMessage(t) if t == "hello"));
        assert!(matches!(&entries[1], ConversationEntry::AssistantText(t) if t == "hi there"));
    }

    #[test]
    fn flatten_with_tool_use() {
        let msgs = vec![make_tool_use_msg("Read", "/src/main.rs")];
        let entries = flatten_messages(&msgs);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], ConversationEntry::ToolUse { name, input_summary } if name == "Read" && input_summary == "/src/main.rs")
        );
    }

    #[test]
    fn count_messages_basic() {
        let msgs = vec![
            make_user_msg("q1"),
            make_assistant_msg("a1"),
            make_user_msg("q2"),
            make_assistant_msg("a2"),
        ];
        let (u, a) = count_messages(&msgs);
        assert_eq!(u, 2);
        assert_eq!(a, 2);
    }

    #[test]
    fn algorithmic_topic_extraction() {
        let msgs = vec![
            make_user_msg("Let's work on the authentication module for the API"),
            make_user_msg("The authentication needs JWT tokens and rate limiting"),
            make_tool_use_msg("Read", "/src/auth.rs"),
        ];
        let topics = extract_topics_algorithmic(&msgs, 5);
        assert!(!topics.is_empty());
        assert!(topics.iter().any(|t| t.contains("auth")));
    }

    #[test]
    fn algorithmic_summary_extraction() {
        let msgs = vec![
            make_user_msg("Fix the login bug in the auth module"),
            make_assistant_msg("Let me take a look at the auth module."),
        ];
        let summary = extract_summary_algorithmic(&msgs);
        assert!(summary.contains("Fix the login bug"));
    }

    #[test]
    fn summary_strips_channel_prefix() {
        let msgs = vec![make_user_msg(
            "[Channel: discord | Trust: VERIFIED]\nFix the login bug",
        )];
        let summary = extract_summary_algorithmic(&msgs);
        assert!(summary.starts_with("Fix the login bug"));
    }

    #[test]
    fn conversation_to_markdown_basic() {
        let msgs = vec![
            make_user_msg("What is Rust?"),
            make_assistant_msg("Rust is a systems programming language."),
        ];
        let md = conversation_to_markdown(&msgs, 1);
        assert!(md.contains("# Conversation 001"));
        assert!(md.contains("### User"));
        assert!(md.contains("### Assistant"));
        assert!(md.contains("What is Rust?"));
    }

    #[test]
    fn duration_calculation() {
        assert_eq!(
            calculate_duration("2026-03-06T10:00:00Z", "2026-03-06T10:45:00Z"),
            "45m"
        );
        assert_eq!(
            calculate_duration("2026-03-06T10:00:00Z", "2026-03-06T12:30:00Z"),
            "2h30m"
        );
    }

    #[test]
    fn utc_now_format() {
        let ts = utc_now();
        assert!(ts.contains('T'));
        assert!(ts.ends_with('Z'));
        assert!(ts.len() >= 19);
    }

    #[test]
    fn empty_messages_produce_empty_topics() {
        let topics = extract_topics_algorithmic(&[], 5);
        assert!(topics.is_empty());
    }
}
