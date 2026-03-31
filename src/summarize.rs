//! Conversation summarization with optional LLM enhancement.
//!
//! When the `pulse-null` feature is enabled and an `LmProvider` is available,
//! uses the LLM for high-quality summaries. Otherwise falls back to
//! algorithmic extraction from conversation entries.

use crate::conversation::{self, Conversation};

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

/// Pure algorithmic summary — no LLM calls. Always available.
pub fn algorithmic_summary(conv: &Conversation) -> ConversationSummary {
    ConversationSummary {
        summary: conversation::extract_summary(conv),
        topics: conversation::extract_topics(conv, 5),
        decisions: Vec::new(),
        action_items: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// LLM-enhanced summarization — behind pulse-null feature
// ---------------------------------------------------------------------------

#[cfg(feature = "pulse-null")]
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

/// Extract summary with fallback: LLM if available, algorithmic otherwise.
///
/// This is the main entry point for pulse-null usage. It never fails — if the
/// LLM call errors, it falls back to algorithmic extraction silently.
#[cfg(feature = "pulse-null")]
pub async fn extract_with_fallback(
    provider: Option<&dyn pulse_system_types::llm::LmProvider>,
    conv: &Conversation,
) -> ConversationSummary {
    if let Some(p) = provider {
        match summarize_conversation(p, conv).await {
            Ok(summary) => return summary,
            Err(e) => {
                eprintln!("recall-echo: LLM summarization failed, using fallback: {e}");
            }
        }
    }

    algorithmic_summary(conv)
}

/// Summarize using an LLM provider.
#[cfg(feature = "pulse-null")]
pub async fn summarize_conversation(
    provider: &dyn pulse_system_types::llm::LmProvider,
    conv: &Conversation,
) -> Result<ConversationSummary, Box<dyn std::error::Error + Send + Sync>> {
    use pulse_system_types::llm::{Message, MessageContent, Role};

    let condensed = conversation::condense_for_summary(conv);

    let llm_messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(condensed),
        source: None,
    }];

    let response = provider
        .invoke(SUMMARIZE_PROMPT, &llm_messages, 500, None)
        .await?;

    let text = response.text();
    parse_summary_response(&text)
}

#[cfg(feature = "pulse-null")]
fn parse_summary_response(
    text: &str,
) -> Result<ConversationSummary, Box<dyn std::error::Error + Send + Sync>> {
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

    #[test]
    fn algorithmic_fallback_produces_output() {
        let conv = Conversation {
            session_id: "test".to_string(),
            first_timestamp: None,
            last_timestamp: None,
            user_message_count: 1,
            assistant_message_count: 1,
            entries: vec![
                conversation::ConversationEntry::UserMessage(
                    "Let's set up authentication with JWT tokens".to_string(),
                ),
                conversation::ConversationEntry::AssistantText(
                    "I'll implement JWT auth. We decided to use RS256 signing.".to_string(),
                ),
            ],
        };
        let summary = algorithmic_summary(&conv);
        assert!(!summary.summary.is_empty());
        assert!(!summary.topics.is_empty());
    }

    #[cfg(feature = "pulse-null")]
    #[test]
    fn parse_valid_json_response() {
        let json = r#"{"summary": "Set up JWT auth.", "topics": ["auth", "jwt"], "decisions": ["Use RS256"], "action_items": ["Add refresh tokens"]}"#;
        let result = parse_summary_response(json).unwrap();
        assert_eq!(result.summary, "Set up JWT auth.");
        assert_eq!(result.topics, vec!["auth", "jwt"]);
        assert_eq!(result.decisions, vec!["Use RS256"]);
        assert_eq!(result.action_items, vec!["Add refresh tokens"]);
    }

    #[cfg(feature = "pulse-null")]
    #[test]
    fn parse_json_with_fencing() {
        let json = "```json\n{\"summary\": \"test\", \"topics\": [], \"decisions\": [], \"action_items\": []}\n```";
        let result = parse_summary_response(json).unwrap();
        assert_eq!(result.summary, "test");
    }

    #[cfg(feature = "pulse-null")]
    #[test]
    fn parse_malformed_json_returns_error() {
        let result = parse_summary_response("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn empty_conversation_produces_empty_summary() {
        let conv = Conversation::new("test");
        let summary = algorithmic_summary(&conv);
        assert_eq!(summary.summary, "Empty session");
        assert!(summary.topics.is_empty());
    }
}
