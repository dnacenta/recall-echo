// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Grok CLI transcripts —
//! `~/.grok/sessions/<percent-encoded cwd>/<session uuid>/chat_history.jsonl`.
//!
//! Two things about the layout. The first directory level is the working
//! directory the session ran in, percent-encoded (`%2Fopt%2Frecall-echo`), so
//! discovery decodes it rather than reporting a mangled path. The second is
//! that each session directory also holds `prompt_history.jsonl`, which
//! contains only the human's prompts — tempting, and wrong: a conversation with
//! the model's side missing is not a conversation.
//!
//! # Shapes
//!
//! ```text
//! {"type":"system",     "content":"<string>"}                    harness
//! {"type":"user",       "content":[{"type":"text","text":"…"}]}  array!
//! {"type":"assistant",  "content":"OK", "tool_calls":[…]}        string!
//! {"type":"reasoning",  "summary":[{"type":"summary_text",…}]}   private
//! {"type":"tool_result","content":"exit: 0\n…"}
//! ```
//!
//! `content` is an array on user turns and a bare string on assistant turns —
//! in the same file. `reasoning` is the model's private thinking; recording it
//! would put unasserted thoughts into memory as though the model had said them,
//! the same hazard the Claude Code parser avoids by dropping thinking blocks.
//!
//! # Which user turns are real
//!
//! Grok injects context under `type: "user"`: a `<user_info>` preamble, project
//! instructions, skill and MCP reminders. It marks them — injected records
//! carry `synthetic_reason`, and a real prompt carries `prompt_index` — so the
//! rule is: when a file marks any prompt with `prompt_index`, only those are
//! turns; when none does, everything without a `synthetic_reason` is. The
//! prompt itself arrives wrapped in `<user_query>`, which is framing for the
//! model and is unwrapped.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::{
    content_text, iso_timestamp, modified_at, newer_than, percent_decode, unwrap_tag, Source,
    Transcript, TranscriptRef,
};
use crate::conversation::{truncate, Conversation, ConversationEntry};
use crate::error::RecallError;

/// The one file in a session directory that holds both sides of the exchange.
const HISTORY_FILE: &str = "chat_history.jsonl";
/// Characters of a tool call's arguments kept in the archive.
const TOOL_INPUT_CHARS: usize = 200;
/// Characters of a tool result kept in the archive.
const TOOL_RESULT_CHARS: usize = 2000;

/// Grok's session records.
#[derive(Debug, Clone)]
pub struct GrokTranscripts {
    sessions_dir: PathBuf,
}

impl GrokTranscripts {
    /// Read sessions from an explicit `sessions/` directory.
    #[must_use]
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Read sessions from this machine's Grok installation.
    #[must_use]
    pub fn detect() -> Option<Self> {
        Some(Self::new(dirs::home_dir()?.join(".grok").join("sessions")))
    }
}

impl Transcript for GrokTranscripts {
    fn source(&self) -> Source {
        Source::Grok
    }

    fn sessions_root(&self) -> &Path {
        &self.sessions_dir
    }

    fn discover(&self, since: Option<SystemTime>) -> Result<Vec<TranscriptRef>, RecallError> {
        let mut found = Vec::new();
        let Ok(workspaces) = std::fs::read_dir(&self.sessions_dir) else {
            return Ok(Vec::new());
        };

        for workspace in workspaces.flatten() {
            let workspace_path = workspace.path();
            if !workspace_path.is_dir() {
                continue;
            }
            let cwd = workspace
                .file_name()
                .to_str()
                .map(percent_decode)
                .filter(|decoded| !decoded.is_empty());

            let Ok(sessions) = std::fs::read_dir(&workspace_path) else {
                continue;
            };
            for session in sessions.flatten() {
                let history = session.path().join(HISTORY_FILE);
                if !history.is_file() {
                    continue;
                }
                let Some(session_id) = session.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                found.push(TranscriptRef {
                    source: Source::Grok,
                    session_id,
                    modified: modified_at(&history),
                    path: history,
                    cwd: cwd.clone(),
                });
            }
        }

        Ok(newer_than(found, since))
    }

    fn parse(&self, transcript: &TranscriptRef) -> Result<Conversation, RecallError> {
        let raw = std::fs::read_to_string(&transcript.path)?;
        let mut conv = parse_history(&raw, &transcript.session_id);
        let (started, ended) = file_span(&transcript.path);
        conv.first_timestamp = Some(iso_timestamp(started));
        conv.last_timestamp = Some(iso_timestamp(ended));
        Ok(conv)
    }
}

/// When the session started and last moved.
///
/// `chat_history.jsonl` carries no timestamps at all, so the file's own times
/// are the only evidence of when the conversation happened — and they are
/// honest ones: the file is created when the session opens and appended to on
/// every turn. Filesystems that do not record a creation time fall back to the
/// last write, which makes the duration zero rather than wrong.
fn file_span(path: &Path) -> (SystemTime, SystemTime) {
    let modified = modified_at(path);
    let created = std::fs::metadata(path)
        .and_then(|meta| meta.created())
        .unwrap_or(modified);
    (created.min(modified), modified)
}

// ── Line model ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChatLine {
    #[serde(rename = "type")]
    kind: String,
    content: Option<serde_json::Value>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    /// Present on the human's own prompts.
    prompt_index: Option<serde_json::Value>,
    /// Present on harness-injected user records.
    synthetic_reason: Option<String>,
}

#[derive(Deserialize)]
struct ToolCall {
    name: Option<String>,
    arguments: Option<serde_json::Value>,
}

fn parse_history(raw: &str, session_id: &str) -> Conversation {
    let lines: Vec<ChatLine> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                eprintln!("recall-echo: skipping malformed grok line");
                None
            }
        })
        .collect();

    let prompts_are_marked = lines
        .iter()
        .any(|line| line.kind == "user" && line.prompt_index.is_some());

    let mut conv = Conversation::new(session_id);
    for line in &lines {
        match line.kind.as_str() {
            "user" if is_real_prompt(line, prompts_are_marked) => {
                let text = unwrap_tag(&line_text(line), "user_query");
                if !text.trim().is_empty() {
                    conv.user_message_count += 1;
                    conv.entries.push(ConversationEntry::UserMessage(text));
                }
            }
            "assistant" => push_assistant(&mut conv, line),
            "tool_result" => conv.entries.push(ConversationEntry::ToolResult {
                content: truncate(line_text(line).trim(), TOOL_RESULT_CHARS),
                is_error: false,
            }),
            // "system" is the harness prompt; "reasoning" is private thinking.
            _ => {}
        }
    }
    conv
}

/// Whether a `user` record is the human speaking.
fn is_real_prompt(line: &ChatLine, prompts_are_marked: bool) -> bool {
    if prompts_are_marked {
        line.prompt_index.is_some()
    } else {
        line.synthetic_reason.is_none()
    }
}

fn push_assistant(conv: &mut Conversation, line: &ChatLine) {
    let text = line_text(line);
    if !text.trim().is_empty() {
        conv.assistant_message_count += 1;
        conv.entries.push(ConversationEntry::AssistantText(text));
    }
    for call in &line.tool_calls {
        let arguments = call
            .arguments
            .as_ref()
            .map(content_text)
            .unwrap_or_default();
        conv.entries.push(ConversationEntry::ToolUse {
            name: call.name.clone().unwrap_or_else(|| "unknown".to_string()),
            input_summary: truncate(arguments.trim(), TOOL_INPUT_CHARS),
        });
    }
}

fn line_text(line: &ChatLine) -> String {
    line.content.as_ref().map(content_text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real shapes from a Grok 4.5 `chat_history.jsonl`, scrubbed: the system
    /// prompt, an unmarked `<user_info>` preamble, two synthetic reminders, the
    /// real prompt, a reasoning record, an assistant turn with a tool call, a
    /// tool result, and the final answer.
    const HISTORY: &str = concat!(
        r#"{"type":"system","content":"You are Grok 4.5 released by xAI. Complete the user's request."}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS Version: linux\nWorkspace Path: /tmp/probe\n</user_info>"}]}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>project instructions</system-reminder>"}],"synthetic_reason":"project_instructions"}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>skills available</system-reminder>"}],"synthetic_reason":"system_reminder"}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nList files, then reply DONE\n</user_query>"}],"prompt_index":0}"#,
        "\n",
        r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"The user wants a directory listing."}],"status":"completed"}"#,
        "\n",
        r#"{"type":"assistant","content":"I'll list the files.","tool_calls":[{"id":"call-1","name":"run_terminal_command","arguments":"{\"command\":\"ls -la\"}"}],"model_id":"grok-4.5-build"}"#,
        "\n",
        r#"{"type":"tool_result","tool_call_id":"call-1","content":"exit: 0\nREADME.md\n"}"#,
        "\n",
        r#"{"type":"assistant","content":"DONE","model_id":"grok-4.5-build"}"#,
        "\n",
    );

    fn fixture_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("%2Ftmp%2Fprobe").join("019fd40b-8e19-7742");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join(HISTORY_FILE), HISTORY).unwrap();
        // The sibling file that must never be mistaken for a transcript.
        std::fs::write(
            tmp.path()
                .join("%2Ftmp%2Fprobe")
                .join("prompt_history.jsonl"),
            "{\"prompt\":\"List files\"}\n",
        )
        .unwrap();
        tmp
    }

    fn parsed() -> Conversation {
        let tmp = fixture_tree();
        let adapter = GrokTranscripts::new(tmp.path().to_path_buf());
        let found = adapter.discover(None).unwrap();
        adapter.parse(&found[0]).unwrap()
    }

    #[test]
    fn discovery_decodes_the_workspace_directory_and_ignores_prompt_history() {
        let tmp = fixture_tree();
        let adapter = GrokTranscripts::new(tmp.path().to_path_buf());

        let found = adapter.discover(None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "019fd40b-8e19-7742");
        assert_eq!(found[0].cwd.as_deref(), Some("/tmp/probe"));
        assert!(found[0].path.ends_with(HISTORY_FILE));
    }

    /// The array/string trap: both sides parse, in one file.
    #[test]
    fn user_content_arrays_and_assistant_content_strings_both_parse() {
        let conv = parsed();
        assert_eq!(conv.user_message_count, 1);
        assert_eq!(conv.assistant_message_count, 2);
        match &conv.entries[0] {
            ConversationEntry::UserMessage(text) => {
                assert_eq!(text, "List files, then reply DONE");
            }
            other => panic!("expected the user turn first, got {other:?}"),
        }
    }

    #[test]
    fn the_system_prompt_and_the_injected_reminders_are_not_turns() {
        let conv = parsed();
        let markdown = crate::conversation::conversation_to_markdown(&conv, 1);
        assert!(!markdown.contains("You are Grok"), "{markdown}");
        assert!(!markdown.contains("project instructions"), "{markdown}");
        assert!(!markdown.contains("user_info"), "{markdown}");
    }

    #[test]
    fn private_reasoning_never_reaches_the_archive() {
        let conv = parsed();
        let markdown = crate::conversation::conversation_to_markdown(&conv, 1);
        assert!(!markdown.contains("directory listing"), "{markdown}");
    }

    #[test]
    fn tool_calls_and_results_survive() {
        let conv = parsed();
        let calls: Vec<&ConversationEntry> = conv
            .entries
            .iter()
            .filter(|e| matches!(e, ConversationEntry::ToolUse { .. }))
            .collect();
        assert_eq!(calls.len(), 1);
        match calls[0] {
            ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                assert_eq!(name, "run_terminal_command");
                assert!(input_summary.contains("ls -la"), "{input_summary}");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert!(conv
            .entries
            .iter()
            .any(|e| matches!(e, ConversationEntry::ToolResult { content, .. } if content.contains("README.md"))));
    }

    /// An older transcript with no `prompt_index` anywhere: fall back to
    /// "everything not marked synthetic", rather than capturing no human side.
    #[test]
    fn unmarked_transcripts_fall_back_to_the_synthetic_flag() {
        let raw = concat!(
            r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>injected</system-reminder>"}],"synthetic_reason":"system_reminder"}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"a real question"}]}"#,
            "\n",
        );
        let conv = parse_history(raw, "s");
        assert_eq!(conv.user_message_count, 1);
        match &conv.entries[0] {
            ConversationEntry::UserMessage(text) => assert_eq!(text, "a real question"),
            other => panic!("expected the unmarked prompt, got {other:?}"),
        }
    }

    #[test]
    fn timestamps_come_from_the_file_because_the_format_has_none() {
        let conv = parsed();
        let first = conv.first_timestamp.expect("a start time");
        let last = conv.last_timestamp.expect("an end time");
        assert!(first.ends_with('Z'), "{first}");
        assert!(last.ends_with('Z'), "{last}");
        assert!(first <= last, "{first} .. {last}");
    }

    #[test]
    fn a_malformed_line_does_not_lose_the_session() {
        let raw = concat!(
            "}{ not json\n",
            r#"{"type":"user","content":[{"type":"text","text":"still here"}],"prompt_index":0}"#,
            "\n",
        );
        let conv = parse_history(raw, "s");
        assert_eq!(conv.user_message_count, 1);
    }
}
