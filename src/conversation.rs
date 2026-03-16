//! Core conversation types and processing.
//!
//! Defines recall-echo's own conversation types — these are the universal
//! internal format. All input adapters (JSONL transcripts, pulse-null Messages)
//! produce these types, which then flow into the archive pipeline.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A parsed conversation entry — the universal internal format.
/// All input adapters produce these.
#[derive(Debug, Clone)]
pub enum ConversationEntry {
    UserMessage(String),
    AssistantText(String),
    ToolUse { name: String, input_summary: String },
    ToolResult { content: String, is_error: bool },
}

/// A parsed conversation — metadata + entries.
/// Produced by input adapters (JSONL, pulse-null), consumed by archive pipeline.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub session_id: String,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub user_message_count: u32,
    pub assistant_message_count: u32,
    pub entries: Vec<ConversationEntry>,
}

impl Conversation {
    /// Create a new empty conversation with the given session ID.
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: 0,
            assistant_message_count: 0,
            entries: Vec::new(),
        }
    }

    /// Total message count (user + assistant).
    pub fn total_messages(&self) -> u32 {
        self.user_message_count + self.assistant_message_count
    }
}

// ---------------------------------------------------------------------------
// Markdown conversion
// ---------------------------------------------------------------------------

/// Convert conversation entries into a markdown document for archival.
pub fn conversation_to_markdown(conv: &Conversation, log_num: u32) -> String {
    let mut md = format!("# Conversation {log_num:03}\n\n");
    let mut last_role: Option<&str> = None;

    for entry in &conv.entries {
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
                let truncated = truncate(content, 2000);
                md.push_str(&format!(
                    "<details><summary>{label}</summary>\n\n```\n{truncated}\n```\n\n</details>\n\n"
                ));
            }
        }
    }

    md
}

// ---------------------------------------------------------------------------
// Topic extraction
// ---------------------------------------------------------------------------

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

/// Algorithmic topic extraction from conversation entries.
/// Uses keyword frequency with stop-word filtering and tool-target boosting.
pub fn extract_topics(conv: &Conversation, max: usize) -> Vec<String> {
    let mut freq: HashMap<String, u32> = HashMap::new();

    // Count words from first 5 user messages
    let mut user_msg_count = 0;
    for entry in &conv.entries {
        if let ConversationEntry::UserMessage(text) = entry {
            let cleaned = strip_channel_prefix(text);
            for word in cleaned.split_whitespace() {
                let clean: String = word
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if clean.len() >= 3 && !STOP_WORDS.contains(&clean.as_str()) {
                    *freq.entry(clean).or_default() += 1;
                }
            }
            user_msg_count += 1;
            if user_msg_count >= 5 {
                break;
            }
        }
    }

    // Boost tool targets (file paths, commands)
    for entry in &conv.entries {
        if let ConversationEntry::ToolUse {
            input_summary,
            name,
            ..
        } = entry
        {
            let target = input_summary
                .rsplit('/')
                .next()
                .unwrap_or(input_summary)
                .trim_matches('`')
                .to_lowercase();
            if target.len() >= 3 && !target.contains('(') {
                let stem = target.split('.').next().unwrap_or(&target);
                if !stem.is_empty() {
                    *freq.entry(stem.to_string()).or_default() += 2;
                }
            }
            let tool_lower = name.to_lowercase();
            if !STOP_WORDS.contains(&tool_lower.as_str()) {
                *freq.entry(tool_lower).or_default() += 1;
            }
        }
    }

    let mut sorted: Vec<(String, u32)> = freq.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sorted.into_iter().take(max).map(|(k, _)| k).collect()
}

// ---------------------------------------------------------------------------
// Summary extraction
// ---------------------------------------------------------------------------

/// Algorithmic summary extraction — first user message, truncated.
pub fn extract_summary(conv: &Conversation) -> String {
    for entry in &conv.entries {
        if let ConversationEntry::UserMessage(text) = entry {
            let cleaned = strip_channel_prefix(text);
            if cleaned.is_empty() {
                continue;
            }
            let truncated: String = cleaned.chars().take(200).collect();
            if truncated.len() < cleaned.len() {
                return format!("{truncated}...");
            }
            return truncated;
        }
    }
    "Empty session".to_string()
}

// ---------------------------------------------------------------------------
// Timestamp / duration helpers
// ---------------------------------------------------------------------------

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
    fn parse_timestamp(ts: &str) -> Option<u64> {
        let t_pos = ts.find('T')?;
        let date_part = &ts[..t_pos];
        let time_part = ts[t_pos + 1..]
            .trim_end_matches('Z')
            .trim_end_matches("+00:00");

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

    match (parse_timestamp(start), parse_timestamp(end)) {
        (Some(a), Some(b)) => {
            let diff = b.abs_diff(a);
            format_duration(diff)
        }
        _ => "unknown".to_string(),
    }
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
// Helpers
// ---------------------------------------------------------------------------

/// Strip [Channel: ...] and "User message:" prefixes from text.
pub fn strip_channel_prefix(text: &str) -> String {
    let mut s = text.trim().to_string();

    if s.starts_with('[') {
        if let Some(end) = s.find("]\n") {
            s = s[end + 2..].trim().to_string();
        } else if let Some(end) = s.find("] ") {
            s = s[end + 2..].trim().to_string();
        }
    }

    if let Some(rest) = s.strip_prefix("User message: ") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("User message:") {
        s = rest.trim().to_string();
    }

    s
}

/// Truncate a string, appending a notice if it was cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let total = s.len();
        format!("{}...\n\n[truncated, {total} chars total]", &s[..max])
    }
}

/// Condense a conversation into a text block suitable for LLM summarization.
/// Keeps it short to minimize token usage.
pub fn condense_for_summary(conv: &Conversation) -> String {
    let mut condensed = String::new();

    for entry in &conv.entries {
        match entry {
            ConversationEntry::UserMessage(text) => {
                condensed.push_str("User: ");
                let t: String = text.chars().take(300).collect();
                condensed.push_str(&t);
                if t.len() < text.len() {
                    condensed.push('\u{2026}');
                }
                condensed.push('\n');
            }
            ConversationEntry::AssistantText(text) => {
                condensed.push_str("Assistant: ");
                let t: String = text.chars().take(300).collect();
                condensed.push_str(&t);
                if t.len() < text.len() {
                    condensed.push('\u{2026}');
                }
                condensed.push('\n');
            }
            ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                condensed.push_str(&format!("[Tool: {name} \u{2192} {input_summary}]\n"));
            }
            ConversationEntry::ToolResult { .. } => {}
        }
    }

    if condensed.len() > 4000 {
        condensed.truncate(4000);
        condensed.push_str("\n\u{2026} (conversation truncated)");
    }

    condensed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_conv(entries: Vec<ConversationEntry>) -> Conversation {
        let mut user_count = 0u32;
        let mut asst_count = 0u32;
        for e in &entries {
            match e {
                ConversationEntry::UserMessage(_) => user_count += 1,
                ConversationEntry::AssistantText(_) => asst_count += 1,
                _ => {}
            }
        }
        Conversation {
            session_id: "test".to_string(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: user_count,
            assistant_message_count: asst_count,
            entries,
        }
    }

    #[test]
    fn conversation_to_markdown_basic() {
        let conv = make_conv(vec![
            ConversationEntry::UserMessage("What is Rust?".to_string()),
            ConversationEntry::AssistantText("Rust is a systems programming language.".to_string()),
        ]);
        let md = conversation_to_markdown(&conv, 1);
        assert!(md.contains("# Conversation 001"));
        assert!(md.contains("### User"));
        assert!(md.contains("### Assistant"));
        assert!(md.contains("What is Rust?"));
    }

    #[test]
    fn topic_extraction() {
        let conv = make_conv(vec![
            ConversationEntry::UserMessage(
                "Let's work on the authentication module for the API".to_string(),
            ),
            ConversationEntry::UserMessage(
                "The authentication needs JWT tokens and rate limiting".to_string(),
            ),
            ConversationEntry::ToolUse {
                name: "Read".to_string(),
                input_summary: "/src/auth.rs".to_string(),
            },
        ]);
        let topics = extract_topics(&conv, 5);
        assert!(!topics.is_empty());
        assert!(topics.iter().any(|t| t.contains("auth")));
    }

    #[test]
    fn summary_extraction() {
        let conv = make_conv(vec![
            ConversationEntry::UserMessage("Fix the login bug in the auth module".to_string()),
            ConversationEntry::AssistantText("Let me take a look at the auth module.".to_string()),
        ]);
        let summary = extract_summary(&conv);
        assert!(summary.contains("Fix the login bug"));
    }

    #[test]
    fn summary_strips_channel_prefix() {
        let conv = make_conv(vec![ConversationEntry::UserMessage(
            "[Channel: discord | Trust: VERIFIED]\nFix the login bug".to_string(),
        )]);
        let summary = extract_summary(&conv);
        assert!(summary.starts_with("Fix the login bug"));
    }

    #[test]
    fn summary_empty_session() {
        let conv = make_conv(vec![]);
        assert_eq!(extract_summary(&conv), "Empty session");
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
    fn duration_short() {
        assert_eq!(
            calculate_duration("2026-03-05T14:30:00.000Z", "2026-03-05T14:30:30.000Z"),
            "< 1m"
        );
    }

    #[test]
    fn duration_invalid() {
        assert_eq!(calculate_duration("garbage", "nonsense"), "unknown");
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
        let conv = make_conv(vec![]);
        let topics = extract_topics(&conv, 5);
        assert!(topics.is_empty());
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn truncate_long() {
        let long = "x".repeat(3000);
        let result = truncate(&long, 2000);
        assert!(result.len() < 3000);
        assert!(result.contains("[truncated, 3000 chars total]"));
    }

    #[test]
    fn condense_truncates_long_messages() {
        let conv = make_conv(vec![ConversationEntry::UserMessage("x".repeat(500))]);
        let condensed = condense_for_summary(&conv);
        assert!(condensed.len() < 400);
        assert!(condensed.contains('\u{2026}'));
    }
}
