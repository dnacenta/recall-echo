// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gemini CLI chat sessions.
//!
//! # Why this exists
//!
//! Gemini ships `gemini hooks migrate --from-claude`, which copies Claude
//! Code's hook commands into Gemini's own settings. A recall-echo user who runs
//! it gets `recall-echo archive-session` wired into Gemini's `SessionEnd` —
//! where it is handed a **JSON document**, not the JSON *Lines* transcript
//! [`crate::jsonl`] parses. Every line of that document fails to parse, the
//! conversation comes out empty, and the session is silently not archived.
//!
//! So this module reads the shape Gemini writes, and archival sniffs the file
//! rather than assuming its own. A migrated hook then works instead of quietly
//! doing nothing.
//!
//! # This shape is unverified
//!
//! It was read off Gemini's type declarations, not off a file produced by a
//! real session: one document, `{sessionId, projectHash, startTime,
//! lastUpdated, messages[], summary?}`, with `messages[].type` in
//! `user | gemini | info | error | warning`, a `content` of Gemini's
//! `PartListUnion` (a string, a part, or a list of parts), and `toolCalls[]`
//! and `thoughts[]` on model messages.
//!
//! Everything here is therefore written to *decline* rather than guess: a
//! document without a `messages` array is not recognised, a message of an
//! unknown type is dropped, and content in an unexpected shape reads as empty.
//! The cost of being wrong is a session that is not archived — never a session
//! archived as something it was not.
//!
//! There is no discovery half to this adapter for the same reason. Sweeping
//! unverified transcripts into memory unattended is a different risk from
//! parsing one a hook explicitly handed us, and it can be added the day
//! someone confirms the format against a real file.
//!
//! # What is dropped, and why
//!
//! `thoughts[]` is the model's private reasoning. Like Grok's `reasoning` and
//! Codex's `developer` role, it is not something the model *asserted*, so it
//! must not enter the graph as self-authored evidence — see the contract in
//! [`crate::transcript`]. `info`, `error` and `warning` messages are the
//! harness talking to the user; they are not turns at all.

use serde_json::Value;

use crate::conversation::{Conversation, ConversationEntry};

use super::content_text;

/// Message types that carry what one of the two parties said.
const HUMAN_TURN: &str = "user";
const MODEL_TURN: &str = "gemini";

/// Whether a JSON document looks like a Gemini chat session.
///
/// The `messages` array is the load-bearing field — without it there is
/// nothing to read — so it is also the marker. `sessionId` is corroborating
/// but not required: a session file is recognised by what it has to offer.
#[must_use]
pub fn is_session_document(document: &Value) -> bool {
    document.get("messages").is_some_and(Value::is_array)
}

/// Read a Gemini chat session into the universal conversation format.
///
/// `session_id` is the identity the archive is recorded under. The document's
/// own `sessionId` is used when the caller has none — a hook payload that named
/// no session is still a session.
#[must_use]
pub fn parse_session(document: &Value, session_id: &str) -> Option<Conversation> {
    if !is_session_document(document) {
        return None;
    }

    let session_id = if session_id.is_empty() {
        document
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("gemini-session")
    } else {
        session_id
    };

    let mut conv = Conversation::new(session_id);
    conv.first_timestamp = timestamp(document, "startTime");
    conv.last_timestamp = timestamp(document, "lastUpdated");

    for message in document["messages"].as_array().into_iter().flatten() {
        append_message(&mut conv, message);
    }

    Some(conv)
}

/// Fold one message into the conversation, or drop it.
fn append_message(conv: &mut Conversation, message: &Value) {
    let text = message
        .get("content")
        .map(content_text)
        .unwrap_or_default()
        .trim()
        .to_string();

    match message.get("type").and_then(Value::as_str).unwrap_or("") {
        HUMAN_TURN if !text.is_empty() => {
            conv.user_message_count += 1;
            conv.entries.push(ConversationEntry::UserMessage(text));
        }
        MODEL_TURN => {
            if !text.is_empty() {
                conv.assistant_message_count += 1;
                conv.entries.push(ConversationEntry::AssistantText(text));
            }
            append_tool_calls(conv, message);
        }
        // `info`, `error`, `warning`, an empty turn, or a type this build has
        // never heard of: harness text, not a turn.
        _ => {}
    }
}

/// Record what the model did, without pretending to know each tool's schema.
fn append_tool_calls(conv: &mut Conversation, message: &Value) {
    for call in message
        .get("toolCalls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let input_summary = call
            .get("args")
            .map(|args| crate::conversation::truncate(&args.to_string(), 200))
            .unwrap_or_default();
        conv.entries.push(ConversationEntry::ToolUse {
            name,
            input_summary,
        });
    }
}

fn timestamp(document: &Value, field: &str) -> Option<String> {
    document
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Value {
        serde_json::json!({
            "sessionId": "sess-1",
            "projectHash": "abc123",
            "startTime": "2026-08-06T10:00:00Z",
            "lastUpdated": "2026-08-06T10:30:00Z",
            "messages": [
                {"type": "user", "content": "Where does recall-echo live?"},
                {
                    "type": "gemini",
                    "content": [{"text": "Under /opt/recall-echo."}],
                    "thoughts": [{"subject": "plan", "description": "check the path first"}],
                    "toolCalls": [{"name": "read_file", "args": {"path": "/opt/recall-echo"}}],
                },
                {"type": "info", "content": "Model switched to gemini-2.5-pro."},
                {"type": "error", "content": "Quota exceeded."},
            ],
        })
    }

    #[test]
    fn a_session_document_is_recognised_by_its_messages() {
        assert!(is_session_document(&session()));
        assert!(!is_session_document(&serde_json::json!({"sessionId": "s"})));
        assert!(!is_session_document(&serde_json::json!({"messages": "no"})));
    }

    #[test]
    fn both_parties_turns_survive_and_nothing_else_does() {
        let conv = parse_session(&session(), "hook-session").expect("recognised");

        assert_eq!(conv.session_id, "hook-session");
        assert_eq!(conv.user_message_count, 1);
        assert_eq!(conv.assistant_message_count, 1);
        assert_eq!(
            conv.first_timestamp.as_deref(),
            Some("2026-08-06T10:00:00Z")
        );
        assert_eq!(conv.last_timestamp.as_deref(), Some("2026-08-06T10:30:00Z"));

        let markdown = crate::conversation::conversation_to_markdown(&conv, 1);
        assert!(markdown.contains("Where does recall-echo live?"));
        assert!(markdown.contains("Under /opt/recall-echo."));
        assert!(markdown.contains("read_file"));
        assert!(
            !markdown.contains("Model switched"),
            "harness notices are not turns: {markdown}"
        );
        assert!(!markdown.contains("Quota exceeded"), "{markdown}");
    }

    /// The provenance contract: private reasoning is not something the model
    /// asserted, so it must never reach the graph as self-authored evidence.
    #[test]
    fn thoughts_never_become_a_turn() {
        let conv = parse_session(&session(), "s").expect("recognised");
        for entry in &conv.entries {
            if let ConversationEntry::AssistantText(text) = entry {
                assert!(!text.contains("check the path first"), "{text}");
            }
        }
    }

    /// `PartListUnion` is a string, a part, or a list of them — all three from
    /// the same CLI, so all three are read.
    #[test]
    fn every_shape_of_content_reads_the_same() {
        let document = serde_json::json!({"messages": [
            {"type": "user", "content": "bare string"},
            {"type": "user", "content": {"text": "one part"}},
            {"type": "user", "content": [{"text": "two "}, {"text": "parts"}]},
        ]});
        let conv = parse_session(&document, "s").expect("recognised");
        let said: Vec<&str> = conv
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ConversationEntry::UserMessage(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(said, ["bare string", "one part", "two parts"]);
    }

    /// The shape is unverified, so anything unexpected is declined rather than
    /// guessed at.
    #[test]
    fn an_unreadable_message_is_dropped_not_invented() {
        let document = serde_json::json!({"messages": [
            {"type": "user"},
            {"type": "user", "content": 42},
            {"type": "unheard-of", "content": "something new"},
            {"type": "gemini", "content": ""},
        ]});
        let conv = parse_session(&document, "s").expect("recognised");
        assert_eq!(conv.user_message_count, 0);
        assert_eq!(conv.assistant_message_count, 0);
        assert!(conv.entries.is_empty(), "{:?}", conv.entries);
    }

    #[test]
    fn a_document_that_is_not_a_session_is_not_parsed() {
        let document = serde_json::json!({"type": "user", "message": {"role": "user"}});
        assert!(parse_session(&document, "s").is_none());
    }

    /// A payload that named no session still archives, under the session the
    /// document names itself.
    #[test]
    fn the_document_names_the_session_when_the_caller_cannot() {
        let conv = parse_session(&session(), "").expect("recognised");
        assert_eq!(conv.session_id, "sess-1");
    }
}
