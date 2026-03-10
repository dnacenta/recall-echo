//! LLM-enhanced conversation summarization with algorithmic fallback.
//!
//! When an `LmProvider` is available (e.g., inside pulse-null), calls Claude
//! to generate high-quality summaries, topics, and action items.
//! Falls back to algorithmic extraction when no provider is available.

use echo_system_types::llm::{LmProvider, Message, MessageContent, Role};

use crate::conversation;

/// Structured summary of a conversation.
#[derive(Debug, Clone, Default)]
pub struct ConversationSummary {
    /// 2-3 sentence summary of the conversation
    pub summary: String,
    /// Up to 5 key topics
    pub topics: Vec<String>,
    /// Key decisions made
    pub decisions: Vec<String>,
    /// Outstanding action items
    pub action_items: Vec<String>,
}

const SUMMARIZE_PROMPT: &str = r#"You are a conversation summarizer. Analyze the conversation and return a JSON object with exactly these fields:

{
  "summary": "2-3 sentence summary of what was discussed and accomplished",
  "topics": ["topic1", "topic2", ...],
  "decisions": ["decision1", "decision2", ...],
  "action_items": ["item1", "item2", ...]
}

Rules:
- summary: 2-3 sentences max. Focus on what was accomplished.
- topics: Up to 5 single-word or short-phrase topics. Lowercase.
- decisions: Key decisions made during the conversation. Empty array if none.
- action_items: Outstanding tasks or follow-ups. Empty array if none.
- Return ONLY valid JSON, no markdown fencing, no explanation."#;

/// Summarize a conversation using an LLM provider.
///
/// Sends the conversation to the LLM with a structured prompt,
/// parses the JSON response into a `ConversationSummary`.
pub async fn summarize_conversation(
    provider: &dyn LmProvider,
    messages: &[Message],
) -> Result<ConversationSummary, Box<dyn std::error::Error + Send + Sync>> {
    // Build a condensed version of the conversation for the LLM
    let condensed = condense_for_summary(messages);

    let llm_messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(condensed),
    }];

    let response = provider
        .invoke(SUMMARIZE_PROMPT, &llm_messages, 500, None)
        .await?;

    let text = response.text();
    parse_summary_response(&text)
}

/// Extract summary with fallback: LLM if available, algorithmic otherwise.
///
/// This is the main entry point. It never fails — if the LLM call errors,
/// it falls back to algorithmic extraction silently.
pub async fn extract_with_fallback(
    provider: Option<&dyn LmProvider>,
    messages: &[Message],
) -> ConversationSummary {
    if let Some(p) = provider {
        match summarize_conversation(p, messages).await {
            Ok(summary) => return summary,
            Err(e) => {
                eprintln!("recall-echo: LLM summarization failed, using fallback: {e}");
            }
        }
    }

    // Algorithmic fallback
    algorithmic_summary(messages)
}

/// Pure algorithmic summary — no LLM calls.
pub fn algorithmic_summary(messages: &[Message]) -> ConversationSummary {
    ConversationSummary {
        summary: conversation::extract_summary_algorithmic(messages),
        topics: conversation::extract_topics_algorithmic(messages, 5),
        decisions: Vec::new(),
        action_items: Vec::new(),
    }
}

/// Condense a conversation into a text block suitable for LLM summarization.
/// Keeps it short to minimize token usage.
fn condense_for_summary(messages: &[Message]) -> String {
    let mut condensed = String::new();
    let entries = conversation::flatten_messages(messages);

    for entry in &entries {
        match entry {
            conversation::ConversationEntry::UserMessage(text) => {
                condensed.push_str("User: ");
                // Truncate long messages
                let t: String = text.chars().take(300).collect();
                condensed.push_str(&t);
                if t.len() < text.len() {
                    condensed.push('…');
                }
                condensed.push('\n');
            }
            conversation::ConversationEntry::AssistantText(text) => {
                condensed.push_str("Assistant: ");
                let t: String = text.chars().take(300).collect();
                condensed.push_str(&t);
                if t.len() < text.len() {
                    condensed.push('…');
                }
                condensed.push('\n');
            }
            conversation::ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                condensed.push_str(&format!("[Tool: {name} → {input_summary}]\n"));
            }
            conversation::ConversationEntry::ToolResult { .. } => {
                // Skip tool results in condensed view
            }
        }
    }

    // Cap total length to ~4000 chars (~1000 tokens)
    if condensed.len() > 4000 {
        condensed.truncate(4000);
        condensed.push_str("\n… (conversation truncated)");
    }

    condensed
}

/// Parse the LLM's JSON response into a ConversationSummary.
fn parse_summary_response(
    text: &str,
) -> Result<ConversationSummary, Box<dyn std::error::Error + Send + Sync>> {
    // Strip markdown fencing if present
    let cleaned = text
        .trim()
        .strip_prefix("```json")
        .or(text.trim().strip_prefix("```"))
        .unwrap_or(text.trim());
    let cleaned = cleaned.strip_suffix("```").unwrap_or(cleaned).trim();

    let v: serde_json::Value = serde_json::from_str(cleaned)?;

    Ok(ConversationSummary {
        summary: v
            .get("summary")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        topics: v
            .get("topics")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .take(5)
                    .collect()
            })
            .unwrap_or_default(),
        decisions: v
            .get("decisions")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .take(5)
                    .collect()
            })
            .unwrap_or_default(),
        action_items: v
            .get("action_items")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .take(5)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msgs() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: MessageContent::Text(
                    "Let's set up authentication with JWT tokens".to_string(),
                ),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text(
                    "I'll implement JWT auth. We decided to use RS256 signing.".to_string(),
                ),
            },
        ]
    }

    #[test]
    fn algorithmic_fallback_produces_output() {
        let msgs = make_msgs();
        let summary = algorithmic_summary(&msgs);
        assert!(!summary.summary.is_empty());
        assert!(!summary.topics.is_empty());
    }

    #[test]
    fn parse_valid_json_response() {
        let json = r#"{"summary": "Set up JWT auth.", "topics": ["auth", "jwt"], "decisions": ["Use RS256"], "action_items": ["Add refresh tokens"]}"#;
        let result = parse_summary_response(json).unwrap();
        assert_eq!(result.summary, "Set up JWT auth.");
        assert_eq!(result.topics, vec!["auth", "jwt"]);
        assert_eq!(result.decisions, vec!["Use RS256"]);
        assert_eq!(result.action_items, vec!["Add refresh tokens"]);
    }

    #[test]
    fn parse_json_with_fencing() {
        let json = "```json\n{\"summary\": \"test\", \"topics\": [], \"decisions\": [], \"action_items\": []}\n```";
        let result = parse_summary_response(json).unwrap();
        assert_eq!(result.summary, "test");
    }

    #[test]
    fn parse_malformed_json_returns_error() {
        let result = parse_summary_response("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn condense_truncates_long_messages() {
        let long_text = "x".repeat(500);
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Text(long_text),
        }];
        let condensed = condense_for_summary(&msgs);
        assert!(condensed.len() < 400);
        assert!(condensed.contains('…'));
    }

    #[test]
    fn empty_messages_produce_empty_summary() {
        let summary = algorithmic_summary(&[]);
        assert!(summary.summary.is_empty());
        assert!(summary.topics.is_empty());
    }
}
