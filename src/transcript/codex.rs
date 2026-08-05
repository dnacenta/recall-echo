// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Codex CLI transcripts — `~/.codex/sessions/YYYY/MM/DD/rollout-<iso>-<uuid>.jsonl`.
//!
//! Every line is `{"timestamp":…, "type":…, "payload":…}`. Four line types
//! matter and the rest are the CLI talking to itself:
//!
//! ```text
//! session_meta   the session's id, cwd and start time      (first line)
//! response_item  the model conversation, one item per line
//! event_msg      the CLI's own event stream                (see below)
//! world_state    workspace snapshots — ignored
//! turn_context   sandbox and approval settings — ignored
//! ```
//!
//! Inside `response_item`, `payload.role` is `user`, `assistant` or
//! **`developer`** — and `developer` is the harness: permission instructions,
//! agent-team rules, mode switches. It is dropped, or every Codex archive would
//! open with the sandbox policy recorded as something the user said.
//!
//! # Which user turns are real
//!
//! Dropping `developer` is not enough. Codex also injects text under
//! `role: "user"` — this machine's transcripts all begin with a
//! `<recommended_plugins>` catalogue nobody typed — and nothing in the record
//! itself distinguishes it from a prompt.
//!
//! The CLI's own event stream does distinguish it: a real prompt is mirrored as
//! `event_msg` with `payload.type = "user_message"`, whose `message` is byte
//! identical to the prompt, and the injected blocks are not mirrored. So a
//! user-role item counts as a turn when the file mirrors it as a user message —
//! and when a file has no such events at all (a Codex build that does not emit
//! them), every user-role item is kept, because losing the human's side
//! entirely is the worse failure.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::{content_text, modified_at, newer_than, walk_files, Source, Transcript, TranscriptRef};
use crate::conversation::{truncate, Conversation, ConversationEntry};
use crate::error::RecallError;

/// Characters of a tool call's arguments kept in the archive.
const TOOL_INPUT_CHARS: usize = 200;
/// Characters of a tool result kept in the archive — the same budget the
/// Claude Code parser uses, so archives from the two CLIs read alike.
const TOOL_RESULT_CHARS: usize = 2000;
/// Length of a hyphenated UUID.
const UUID_LEN: usize = 36;

/// Codex's session records.
#[derive(Debug, Clone)]
pub struct CodexTranscripts {
    sessions_dir: PathBuf,
}

impl CodexTranscripts {
    /// Read sessions from an explicit `sessions/` directory.
    #[must_use]
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Read sessions from this machine's Codex installation.
    #[must_use]
    pub fn detect() -> Option<Self> {
        Some(Self::new(dirs::home_dir()?.join(".codex").join("sessions")))
    }
}

impl Transcript for CodexTranscripts {
    fn source(&self) -> Source {
        Source::Codex
    }

    fn sessions_root(&self) -> &Path {
        &self.sessions_dir
    }

    fn discover(&self, since: Option<SystemTime>) -> Result<Vec<TranscriptRef>, RecallError> {
        let found = walk_files(&self.sessions_dir, "jsonl", 0)
            .into_iter()
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?;
                Some(TranscriptRef {
                    source: Source::Codex,
                    session_id: session_id_from_stem(stem),
                    modified: modified_at(&path),
                    path,
                    cwd: None,
                })
            })
            .collect();
        Ok(newer_than(found, since))
    }

    fn parse(&self, transcript: &TranscriptRef) -> Result<Conversation, RecallError> {
        let raw = std::fs::read_to_string(&transcript.path)?;
        Ok(parse_rollout(&raw, &transcript.session_id))
    }
}

/// A session id from `rollout-<iso8601>-<uuid>`.
///
/// The trailing UUID is the id Codex records inside the file; the stem is used
/// verbatim when it does not end in one, so an unexpected name still gives a
/// stable, unique key rather than a collision.
fn session_id_from_stem(stem: &str) -> String {
    if stem.len() > UUID_LEN {
        let tail = &stem[stem.len() - UUID_LEN..];
        if is_uuid_shaped(tail) {
            return tail.to_string();
        }
    }
    stem.to_string()
}

fn is_uuid_shaped(candidate: &str) -> bool {
    candidate.len() == UUID_LEN
        && candidate
            .chars()
            .enumerate()
            .all(|(index, ch)| match index {
                8 | 13 | 18 | 23 => ch == '-',
                _ => ch.is_ascii_hexdigit(),
            })
}

// ── Line model ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RolloutLine {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    payload: Option<serde_json::Value>,
}

/// Parse a rollout file's contents into a conversation.
fn parse_rollout(raw: &str, fallback_session_id: &str) -> Conversation {
    let mut conv = Conversation::new(fallback_session_id);
    // Positions of user turns, so the ones the CLI never mirrored as prompts
    // can be dropped once the whole file has been read.
    let mut user_positions: Vec<usize> = Vec::new();
    let mut real_prompts: HashSet<String> = HashSet::new();

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<RolloutLine>(line) else {
            eprintln!("recall-echo: skipping malformed codex line");
            continue;
        };

        if let Some(ref timestamp) = entry.timestamp {
            if conv.first_timestamp.is_none() {
                conv.first_timestamp = Some(timestamp.clone());
            }
            conv.last_timestamp = Some(timestamp.clone());
        }

        let Some(payload) = entry.payload else {
            continue;
        };

        match entry.kind.as_str() {
            "session_meta" => {
                if let Some(id) = payload.get("session_id").and_then(|v| v.as_str()) {
                    conv.session_id = id.to_string();
                }
            }
            "event_msg" => {
                if payload.get("type").and_then(|v| v.as_str()) == Some("user_message") {
                    if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
                        real_prompts.insert(message.trim().to_string());
                    }
                }
            }
            "response_item" => push_item(&mut conv, &payload, &mut user_positions),
            _ => {}
        }
    }

    if !real_prompts.is_empty() {
        retain_real_prompts(&mut conv, &user_positions, &real_prompts);
    }
    conv
}

/// Turn one `response_item` into conversation entries, if it is one.
fn push_item(
    conv: &mut Conversation,
    payload: &serde_json::Value,
    user_positions: &mut Vec<usize>,
) {
    let item_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match item_type {
        "message" => {
            let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let Some(content) = payload.get("content") else {
                return;
            };
            let text = content_text(content);
            if text.trim().is_empty() {
                return;
            }
            match role {
                "user" => {
                    user_positions.push(conv.entries.len());
                    conv.user_message_count += 1;
                    conv.entries.push(ConversationEntry::UserMessage(text));
                }
                "assistant" => {
                    conv.assistant_message_count += 1;
                    conv.entries.push(ConversationEntry::AssistantText(text));
                }
                // `developer` is the harness, and `system` would be too.
                _ => {}
            }
        }
        "custom_tool_call" | "function_call" => {
            let name = payload
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let raw_input = payload
                .get("input")
                .or_else(|| payload.get("arguments"))
                .map(content_or_json)
                .unwrap_or_default();
            conv.entries.push(ConversationEntry::ToolUse {
                name,
                input_summary: truncate(raw_input.trim(), TOOL_INPUT_CHARS),
            });
        }
        "custom_tool_call_output" | "function_call_output" => {
            let content = payload
                .get("output")
                .map(content_or_json)
                .unwrap_or_default();
            conv.entries.push(ConversationEntry::ToolResult {
                content: truncate(content.trim(), TOOL_RESULT_CHARS),
                is_error: false,
            });
        }
        // `reasoning` carries the model's private, unasserted thinking (and an
        // encrypted blob). It is not conversation.
        _ => {}
    }
}

/// Text of a field that is a string, a block list, or something structured.
fn content_or_json(value: &serde_json::Value) -> String {
    let text = content_text(value);
    if text.is_empty() && !value.is_string() {
        return serde_json::to_string(value).unwrap_or_default();
    }
    text
}

/// Drop user turns the CLI never mirrored as prompts — the injected ones.
fn retain_real_prompts(
    conv: &mut Conversation,
    user_positions: &[usize],
    real_prompts: &HashSet<String>,
) {
    let injected: HashSet<usize> = user_positions
        .iter()
        .copied()
        .filter(|position| match conv.entries.get(*position) {
            Some(ConversationEntry::UserMessage(text)) => !real_prompts.contains(text.trim()),
            _ => false,
        })
        .collect();
    if injected.is_empty() {
        return;
    }

    let mut position = 0;
    conv.entries.retain(|_| {
        let keep = !injected.contains(&position);
        position += 1;
        keep
    });
    conv.user_message_count = conv
        .user_message_count
        .saturating_sub(injected.len().try_into().unwrap_or(u32::MAX));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real shapes from a Codex 0.146 rollout, scrubbed: a session_meta line, a
    /// developer instruction, an injected `<recommended_plugins>` user block, a
    /// real prompt with its mirroring event, a reasoning item, an assistant
    /// message, and one tool round trip.
    const ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-08-05T22:29:00.878Z","type":"session_meta","payload":{"session_id":"019fd40b-55d5-7a72-8ecb-611abc36879e","cwd":"/tmp/probe","timestamp":"2026-08-05T22:29:00.107Z","cli_version":"0.146.1"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:00.879Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.293Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>sandbox_mode is read-only.</permissions instructions>"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.295Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>\nHere is a list of plugins that are available but not installed.\n</recommended_plugins>"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.296Z","type":"world_state","payload":{"full":true,"state":{}}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.298Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/tmp/probe"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.329Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"List the files here, then reply DONE"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.330Z","type":"event_msg","payload":{"type":"user_message","message":"List the files here, then reply DONE","images":null}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:03.595Z","type":"response_item","payload":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAAAsecret"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:04.028Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"I'll check the directory."}],"phase":"commentary"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:04.977Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_1","name":"exec","input":"const r = await tools.exec_command({\"cmd\":\"ls\"});"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:05.394Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"Script completed\nOutput:\nREADME.md"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:06.921Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"DONE"}],"phase":"final_answer"}}"#,
        "\n",
    );

    fn fixture_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026/08/05");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-2026-08-05T22-29-00-019fd40b-55d5-7a72-8ecb-611abc36879e.jsonl"),
            ROLLOUT,
        )
        .unwrap();
        tmp
    }

    fn parsed() -> Conversation {
        let tmp = fixture_tree();
        let adapter = CodexTranscripts::new(tmp.path().to_path_buf());
        let found = adapter.discover(None).unwrap();
        adapter.parse(&found[0]).unwrap()
    }

    #[test]
    fn discovery_walks_the_date_nested_directories() {
        let tmp = fixture_tree();
        let adapter = CodexTranscripts::new(tmp.path().to_path_buf());

        let found = adapter.discover(None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "019fd40b-55d5-7a72-8ecb-611abc36879e");
        assert_eq!(found[0].source, Source::Codex);
    }

    #[test]
    fn session_ids_come_from_the_trailing_uuid() {
        assert_eq!(
            session_id_from_stem(
                "rollout-2026-08-05T22-29-00-019fd40b-55d5-7a72-8ecb-611abc36879e"
            ),
            "019fd40b-55d5-7a72-8ecb-611abc36879e"
        );
        assert_eq!(session_id_from_stem("odd-name"), "odd-name");
    }

    /// The trap this adapter exists for: `developer` is the system prompt.
    #[test]
    fn developer_turns_are_not_conversation() {
        let conv = parsed();
        for entry in &conv.entries {
            let text = match entry {
                ConversationEntry::UserMessage(t) | ConversationEntry::AssistantText(t) => t,
                _ => continue,
            };
            assert!(!text.contains("permissions instructions"), "{text}");
        }
    }

    #[test]
    fn injected_user_blocks_are_dropped_and_the_real_prompt_is_kept() {
        let conv = parsed();
        let users: Vec<&String> = conv
            .entries
            .iter()
            .filter_map(|e| match e {
                ConversationEntry::UserMessage(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0], "List the files here, then reply DONE");
        assert_eq!(conv.user_message_count, 1);
    }

    /// No mirroring events at all: keep every user-role item rather than
    /// capture a session with no human side.
    #[test]
    fn without_mirroring_events_every_user_item_is_kept() {
        let raw = concat!(
            r#"{"timestamp":"2026-08-05T22:29:02.329Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-08-05T22:29:02.330Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}}"#,
            "\n",
        );
        let conv = parse_rollout(raw, "fallback");
        assert_eq!(conv.user_message_count, 2);
        assert_eq!(conv.session_id, "fallback");
    }

    #[test]
    fn reasoning_never_reaches_the_archive() {
        let conv = parsed();
        let markdown = crate::conversation::conversation_to_markdown(&conv, 1);
        assert!(!markdown.contains("gAAAAAsecret"), "{markdown}");
    }

    #[test]
    fn tool_calls_and_results_survive() {
        let conv = parsed();
        let tools: Vec<&ConversationEntry> = conv
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ConversationEntry::ToolUse { .. } | ConversationEntry::ToolResult { .. }
                )
            })
            .collect();
        assert_eq!(tools.len(), 2);
        match tools[0] {
            ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                assert_eq!(name, "exec");
                assert!(input_summary.contains("exec_command"), "{input_summary}");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        match tools[1] {
            ConversationEntry::ToolResult { content, is_error } => {
                assert!(content.contains("README.md"), "{content}");
                assert!(!is_error);
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn metadata_comes_from_the_session_and_the_line_timestamps() {
        let conv = parsed();
        assert_eq!(conv.session_id, "019fd40b-55d5-7a72-8ecb-611abc36879e");
        assert_eq!(conv.assistant_message_count, 2);
        assert_eq!(
            conv.first_timestamp.as_deref(),
            Some("2026-08-05T22:29:00.878Z")
        );
        assert_eq!(
            conv.last_timestamp.as_deref(),
            Some("2026-08-05T22:29:06.921Z")
        );
    }

    #[test]
    fn a_malformed_line_does_not_lose_the_session() {
        let raw = concat!(
            "not json at all\n",
            r#"{"timestamp":"2026-08-05T22:29:02.329Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"still here"}]}}"#,
            "\n",
        );
        let conv = parse_rollout(raw, "s");
        assert_eq!(conv.user_message_count, 1);
    }
}
