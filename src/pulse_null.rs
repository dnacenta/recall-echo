//! Pulse-null adapter — converts echo-system-types Messages into Conversations.
//!
//! This module is gated behind the `pulse-null` feature flag.
//! It bridges echo-system-types::Message → conversation::Conversation,
//! allowing recall-echo to work inside pulse-null entities.

use echo_system_types::llm::{ContentBlock, Message, MessageContent, Role};

use crate::conversation::{Conversation, ConversationEntry};

/// Convert pulse-null Messages into a Conversation.
pub fn messages_to_conversation(messages: &[Message], session_id: &str) -> Conversation {
    let mut conv = Conversation::new(session_id);

    for msg in messages {
        let role = &msg.role;
        match &msg.content {
            MessageContent::Text(text) => {
                if text.trim().is_empty() {
                    continue;
                }
                match role {
                    Role::User => {
                        conv.user_message_count += 1;
                        conv.entries
                            .push(ConversationEntry::UserMessage(text.clone()));
                    }
                    Role::Assistant => {
                        conv.assistant_message_count += 1;
                        conv.entries
                            .push(ConversationEntry::AssistantText(text.clone()));
                    }
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
                                    conv.user_message_count += 1;
                                    conv.entries
                                        .push(ConversationEntry::UserMessage(text.clone()));
                                }
                                Role::Assistant => {
                                    conv.assistant_message_count += 1;
                                    conv.entries
                                        .push(ConversationEntry::AssistantText(text.clone()));
                                }
                            }
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            let summary = summarize_tool_input(name, input);
                            conv.entries.push(ConversationEntry::ToolUse {
                                name: name.clone(),
                                input_summary: summary,
                            });
                        }
                        ContentBlock::ToolResult {
                            content, is_error, ..
                        } => {
                            conv.entries.push(ConversationEntry::ToolResult {
                                content: content.clone(),
                                is_error: is_error.unwrap_or(false),
                            });
                        }
                    }
                }
            }
        }
    }

    conv
}

/// Summarize tool input JSON into a brief human-readable string.
fn summarize_tool_input(name: &str, input: &serde_json::Value) -> String {
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
    format!("{name}(\u{2026})")
}

/// Count user and assistant messages from pulse-null Messages.
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
    fn convert_simple_messages() {
        let msgs = vec![make_user_msg("hello"), make_assistant_msg("hi there")];
        let conv = messages_to_conversation(&msgs, "test-sess");
        assert_eq!(conv.session_id, "test-sess");
        assert_eq!(conv.user_message_count, 1);
        assert_eq!(conv.assistant_message_count, 1);
        assert_eq!(conv.entries.len(), 2);
    }

    #[test]
    fn convert_with_tool_use() {
        let msgs = vec![
            make_user_msg("read that file"),
            make_tool_use_msg("Read", "/src/main.rs"),
        ];
        let conv = messages_to_conversation(&msgs, "test");
        assert_eq!(conv.entries.len(), 2);
        assert!(
            matches!(&conv.entries[1], ConversationEntry::ToolUse { name, input_summary } if name == "Read" && input_summary == "/src/main.rs")
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
    fn skips_empty_text() {
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Text("   ".to_string()),
        }];
        let conv = messages_to_conversation(&msgs, "test");
        assert_eq!(conv.user_message_count, 0);
        assert!(conv.entries.is_empty());
    }
}
