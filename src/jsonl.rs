// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

/// What a SessionEnd hook tells us about the session that just ended.
///
/// Claude Code's payload is the reference shape. The camelCase aliases and the
/// defaults are for the other harnesses that can end up invoking this command —
/// Gemini's `hooks migrate --from-claude` copies our hook straight into its own
/// settings, and a payload that spells a field differently, or omits it, must
/// produce a clear message rather than a deserialization error on every single
/// session. What is missing is reported by
/// [`crate::archive::run_with_hook_input`], which can say what to do about it.
#[derive(Deserialize, Debug, Default)]
pub struct HookInput {
    #[serde(default, alias = "sessionId")]
    pub session_id: String,
    #[serde(
        default,
        alias = "transcriptPath",
        alias = "transcript",
        alias = "transcript_file"
    )]
    pub transcript_path: String,
    #[serde(rename = "cwd")]
    pub _cwd: Option<String>,
    #[serde(rename = "hook_event_name")]
    pub _hook_event_name: Option<String>,
}

pub fn read_hook_input() -> Result<HookInput, crate::error::RecallError> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;

    if buf.trim().is_empty() {
        return Err(crate::error::RecallError::Other(
            "No input on stdin. This command is called by the Claude Code SessionEnd hook."
                .to_string(),
        ));
    }

    Ok(serde_json::from_str(&buf)?)
}

// ---------------------------------------------------------------------------
// JSONL entry types (deserialization)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonlEntry {
    #[serde(rename = "type")]
    entry_type: String,
    timestamp: Option<String>,
    message: Option<RawMessage>,
    /// Harness bookkeeping, not something the person typed.
    #[serde(default, rename = "isMeta")]
    is_meta: bool,
}

#[derive(Deserialize)]
struct RawMessage {
    role: Option<String>,
    content: Option<ContentValue>,
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

/// Whether a file is a JSON *Lines* transcript, as Claude Code writes them.
///
/// A JSON document — Gemini's chat sessions, say — is one object, so its first
/// line either fails to parse (pretty-printed) or parses into something a
/// transcript entry never is. Only the first non-empty line is read: sniffing
/// must not cost a pass over a multi-megabyte transcript.
#[must_use]
pub fn is_jsonl_transcript(path: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .is_some_and(|value| {
            // A whole session document on one line is not a transcript entry,
            // however well-formed it is.
            value.is_object() && !crate::transcript::gemini::is_session_document(&value)
        })
}

/// How many lines [`is_subagent_transcript`] reads before giving up — blank
/// lines included, so a file of newlines cannot stretch the window.
///
/// On a survey of 192 real transcripts the first entry carrying `isSidechain`
/// sat at line 2 at the latest in a subagent's file and line 8 in a session's —
/// metadata entries (`ai-title`, `last-prompt`, `mode`, `queue-operation`, a
/// fork's `fork-context-ref`) come first and carry no flag.
const SIDECHAIN_SNIFF_LINES: usize = 64;

/// How many bytes [`is_subagent_transcript`] reads at most. A line cap alone
/// does not bound the read: one line can be the whole file.
const SIDECHAIN_SNIFF_BYTES: u64 = 256 * 1024;

/// Whether a Claude Code transcript belongs to a subagent rather than a session.
///
/// Claude Code writes each subagent's run to
/// `projects/<project>/<session>/subagents/agent-<id>.jsonl`, and marks every
/// turn in it `isSidechain: true`. Either signal is enough:
///
/// - the file's directory is named `subagents` — free, and today's layout;
/// - the first entry that carries an `isSidechain` field has it `true` — Claude
///   Code's own marker, which survives a layout change. It is the first entry
///   *carrying* the field, not the first line: a fork's transcript opens with a
///   `fork-context-ref` entry that has none.
///
/// A subagent's "user" turns are the parent model's prompts, so archiving one
/// as a session would enter model-written text as something the human said.
/// At most [`SIDECHAIN_SNIFF_LINES`] lines and [`SIDECHAIN_SNIFF_BYTES`] bytes
/// are read, and only lines that mention `"isSidechain"` are parsed; a file
/// that cannot be opened is decided by its path alone.
#[must_use]
pub fn is_subagent_transcript(path: &std::path::Path) -> bool {
    in_subagents_dir(path) || flags_sidechain(path)
}

/// The free half of [`is_subagent_transcript`]: the file sits in a directory
/// named `subagents`. Discovery applies it to every file it walks.
#[must_use]
pub(crate) fn in_subagents_dir(path: &std::path::Path) -> bool {
    path.parent()
        .and_then(std::path::Path::file_name)
        .is_some_and(|dir| dir == "subagents")
}

/// The reading half of [`is_subagent_transcript`]: the first entry carrying
/// `isSidechain` has it `true`. Bounded by [`SIDECHAIN_SNIFF_LINES`] and
/// [`SIDECHAIN_SNIFF_BYTES`]; false for a file that cannot be opened. Capture
/// calls it only for transcripts not already archived, so a finished session
/// is not re-read on every sweep.
#[must_use]
pub(crate) fn flags_sidechain(path: &std::path::Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    BufReader::new(file.take(SIDECHAIN_SNIFF_BYTES))
        .lines()
        .take(SIDECHAIN_SNIFF_LINES)
        // An undecodable line is skipped, not the end of the sniff: stopping
        // there would wave a subagent through on one bad byte.
        .filter_map(Result::ok)
        .filter(|line| line.contains("\"isSidechain\""))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .find_map(|entry| {
            entry
                .get("isSidechain")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
}

/// One `recall-echo: debug:` line naming a skipped subagent transcript and
/// the signal that caught it (`signal`). Only when `RECALL_ECHO_DEBUG` is set
/// and non-empty: the crate has no logger, and the daemon sweeps on a timer.
pub(crate) fn debug_skipped_subagent(path: &std::path::Path, signal: &str) {
    if std::env::var_os("RECALL_ECHO_DEBUG").is_some_and(|value| !value.is_empty()) {
        // `{:?}` escapes control characters: the name is whatever is on disk.
        eprintln!("recall-echo: debug: skipped subagent transcript {path:?} ({signal})");
    }
}

/// What a hook says when it was pointed at a subagent's transcript, `not_done`
/// naming the work it skipped. One line on stderr — stdout may be reserved
/// for JSON by the harness — shared by `archive-session` and `checkpoint`.
pub(crate) fn report_declined_subagent(transcript_path: &str, not_done: &str) {
    eprintln!(
        "recall-echo: {transcript_path} is a subagent transcript (part of a session, not one), \
         {not_done}"
    );
}

/// Parse a Claude Code JSONL transcript into a Conversation.
pub fn parse_transcript(
    path: &str,
    session_id: &str,
) -> Result<Conversation, crate::error::RecallError> {
    let file = File::open(path)?;
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
            "user" if entry.is_meta => parse_meta_user_content(&mut conv, content),
            "user" => parse_user_content(&mut conv, content),
            "assistant" => parse_assistant_content(&mut conv, content),
            _ => {}
        }
    }

    Ok(conv)
}

fn parse_user_content(conv: &mut Conversation, content: ContentValue) {
    match content {
        ContentValue::Text(text) => push_user_message(conv, text),
        ContentValue::Blocks(blocks) => parse_user_blocks(conv, blocks),
    }
}

/// `isMeta` user text is the harness speaking (a skill expansion, for one).
/// Tool results on that same entry are still the tool's output, so they stay.
fn parse_meta_user_content(conv: &mut Conversation, content: ContentValue) {
    let ContentValue::Blocks(blocks) = content else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            push_tool_result(conv, &block);
        }
    }
}

fn push_user_message(conv: &mut Conversation, text: String) {
    conv.user_message_count += 1;
    conv.entries.push(ConversationEntry::UserMessage(text));
}

/// Block-list user content. Consecutive `text` blocks are one user turn,
/// joined with a blank line, so a prompt stored as blocks is archived.
/// A tool result ends the run: the turn stays where it sits among results.
/// Whitespace-only text is dropped, as is a harness interrupt notice.
/// Image and other types stay ignored.
fn parse_user_blocks(conv: &mut Conversation, blocks: Vec<serde_json::Value>) {
    let mut text_parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => collect_user_text(&mut text_parts, &block),
            "tool_result" => {
                flush_user_text(conv, &mut text_parts);
                push_tool_result(conv, &block);
            }
            _ => {}
        }
    }
    flush_user_text(conv, &mut text_parts);
}

/// The harness writes this when it cancels a turn. It is not user speech.
const INTERRUPTED_TURN_PREFIX: &str = "[Request interrupted by user";

fn collect_user_text(parts: &mut Vec<String>, block: &serde_json::Value) {
    let Some(text) = block.get("text").and_then(|value| value.as_str()) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with(INTERRUPTED_TURN_PREFIX) {
        return;
    }
    parts.push(text.to_string());
}

fn flush_user_text(conv: &mut Conversation, parts: &mut Vec<String>) {
    if parts.is_empty() {
        return;
    }
    push_user_message(conv, std::mem::take(parts).join("\n\n"));
}

fn push_tool_result(conv: &mut Conversation, block: &serde_json::Value) {
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

    /// The sniff that keeps a migrated Gemini hook from feeding a JSON
    /// document to a JSON Lines parser.
    #[test]
    fn only_json_lines_reads_as_a_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_jsonl(dir.path());
        assert!(is_jsonl_transcript(&path));

        let session = serde_json::json!({
            "sessionId": "s",
            "messages": [{"type": "user", "content": "hi"}],
        });
        let write = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            path.to_string_lossy().to_string()
        };

        assert!(!is_jsonl_transcript(&write(
            "one-line.json",
            &serde_json::to_string(&session).unwrap()
        )));
        assert!(!is_jsonl_transcript(&write(
            "pretty.json",
            &serde_json::to_string_pretty(&session).unwrap()
        )));
        assert!(!is_jsonl_transcript(&write("empty.jsonl", "")));
        assert!(!is_jsonl_transcript(&write("prose.txt", "hello\nthere")));
        assert!(!is_jsonl_transcript("/nonexistent/transcript.jsonl"));
    }

    // ── Subagent detection ───────────────────────────────────────────────

    const SUBAGENT_TURN: &str = r#"{"type":"user","isSidechain":true,"agentId":"a1","sessionId":"parent","message":{"role":"user","content":"Draft slide 4"}}"#;
    const SESSION_TURN: &str = r#"{"type":"user","isSidechain":false,"sessionId":"s1","message":{"role":"user","content":"hello"}}"#;
    const FORK_CONTEXT_REF: &str = r#"{"type":"fork-context-ref","agentId":"a2","parentSessionId":"parent","contextLength":10}"#;

    fn write_lines(dir: &std::path::Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn subagent_detected_by_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(dir.path(), "s1/subagents/agent-a1.jsonl", &[SUBAGENT_TURN]);
        assert!(is_subagent_transcript(&path));
    }

    #[test]
    fn fork_detected_by_first_flagged_entry_outside_subagents_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            dir.path(),
            "elsewhere/agent-a2.jsonl",
            &[FORK_CONTEXT_REF, SUBAGENT_TURN],
        );
        assert!(is_subagent_transcript(&path));
    }

    #[test]
    fn subagents_dir_without_flag_still_detected() {
        let dir = tempfile::tempdir().unwrap();
        let unflagged = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        let path = write_lines(dir.path(), "s1/subagents/agent-a3.jsonl", &[unflagged]);
        assert!(is_subagent_transcript(&path));
    }

    #[test]
    fn main_transcript_with_metadata_preamble_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let preamble = [
            r#"{"type":"ai-title","title":"t"}"#,
            r#"{"type":"last-prompt","prompt":"p"}"#,
            r#"{"type":"mode","mode":"default"}"#,
            r#"{"type":"queue-operation","operation":"enqueue"}"#,
            r#"{"type":"ai-title","title":"t2"}"#,
            r#"{"type":"last-prompt","prompt":"p2"}"#,
            r#"{"type":"mode","mode":"plan"}"#,
            SESSION_TURN,
            SUBAGENT_TURN,
        ];
        let path = write_lines(dir.path(), "-home-dev-app/s1.jsonl", &preamble);
        assert!(!is_subagent_transcript(&path));
    }

    #[test]
    fn main_transcript_without_flag_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::path::PathBuf::from(write_test_jsonl(dir.path()));
        assert!(!is_subagent_transcript(&path));
        let empty = write_lines(dir.path(), "empty.jsonl", &[]);
        assert!(!is_subagent_transcript(&empty));
    }

    #[test]
    fn malformed_lines_in_window_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            dir.path(),
            "x/agent-a4.jsonl",
            &["{not json", "", "   ", SUBAGENT_TURN],
        );
        assert!(is_subagent_transcript(&path));
    }

    #[test]
    fn flag_beyond_sniff_window_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let filler = r#"{"type":"progress"}"#;
        let mut lines = vec![filler; SIDECHAIN_SNIFF_LINES];
        lines.push(SUBAGENT_TURN);
        let path = write_lines(dir.path(), "x/late.jsonl", &lines);
        assert!(!is_subagent_transcript(&path));

        // One line earlier and it is inside the window.
        lines.remove(0);
        let path = write_lines(dir.path(), "x/in-window.jsonl", &lines);
        assert!(is_subagent_transcript(&path));
    }

    /// A line that is not UTF-8 is skipped like a malformed one; the flag
    /// after it is still read.
    #[test]
    fn invalid_utf8_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-a6.jsonl");
        let mut body = vec![0xff, 0xfe, b'{', b'\n'];
        body.extend_from_slice(SUBAGENT_TURN.as_bytes());
        std::fs::write(&path, body).unwrap();
        assert!(is_subagent_transcript(&path));
    }

    /// Blank lines count toward the line cap: a run of newlines cannot push
    /// the window past [`SIDECHAIN_SNIFF_LINES`].
    #[test]
    fn blank_lines_count_toward_the_sniff_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = vec![""; SIDECHAIN_SNIFF_LINES];
        lines.push(SUBAGENT_TURN);
        let path = write_lines(dir.path(), "x/blank-padded.jsonl", &lines);
        assert!(!is_subagent_transcript(&path));
    }

    /// The read is bounded in bytes too: one enormous first line ends the
    /// sniff, however few lines it is.
    #[test]
    fn the_sniff_stops_at_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let padded =
            |bytes: usize| format!(r#"{{"type":"progress","pad":"{}"}}"#, "x".repeat(bytes));

        let over = padded(SIDECHAIN_SNIFF_BYTES as usize);
        let path = write_lines(dir.path(), "x/over.jsonl", &[&over, SUBAGENT_TURN]);
        assert!(!is_subagent_transcript(&path));

        let under = padded(SIDECHAIN_SNIFF_BYTES as usize / 2);
        let path = write_lines(dir.path(), "x/under.jsonl", &[&under, SUBAGENT_TURN]);
        assert!(is_subagent_transcript(&path));
    }

    #[test]
    fn unopenable_path_decided_by_path_alone() {
        assert!(is_subagent_transcript(std::path::Path::new(
            "/nonexistent/s1/subagents/agent-a5.jsonl"
        )));
        assert!(!is_subagent_transcript(std::path::Path::new(
            "/nonexistent/s1.jsonl"
        )));
    }

    #[test]
    fn a_hook_payload_survives_a_renamed_field() {
        let claude = r#"{"session_id":"a","transcript_path":"/tmp/a.jsonl"}"#;
        let parsed: HookInput = serde_json::from_str(claude).unwrap();
        assert_eq!(parsed.session_id, "a");
        assert_eq!(parsed.transcript_path, "/tmp/a.jsonl");

        // camelCase, and a payload that carries neither: both must parse, so
        // the command can explain itself instead of dying on every session.
        let other = r#"{"sessionId":"b","transcriptPath":"/tmp/b.json"}"#;
        let parsed: HookInput = serde_json::from_str(other).unwrap();
        assert_eq!(parsed.session_id, "b");
        assert_eq!(parsed.transcript_path, "/tmp/b.json");

        let bare: HookInput = serde_json::from_str("{}").unwrap();
        assert!(bare.transcript_path.is_empty());
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

    fn parse_user_line(line: &str) -> Conversation {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, line).unwrap();
        parse_transcript(&path.to_string_lossy(), "sess").unwrap()
    }

    #[test]
    fn ac1_text_block_is_one_user_message() {
        let conv = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        );

        assert_eq!(conv.user_message_count, 1);
        assert_eq!(conv.entries.len(), 1);
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::UserMessage(text) if text == "hi"
        ));
    }

    #[test]
    fn ac2_tool_result_and_text_keep_block_order() {
        let text_then_tool = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"before"},{"type":"tool_result","content":"file body"}]}}"#,
        );
        assert_eq!(text_then_tool.user_message_count, 1);
        assert_eq!(text_then_tool.entries.len(), 2);
        assert!(matches!(
            &text_then_tool.entries[0],
            ConversationEntry::UserMessage(text) if text == "before"
        ));
        assert!(matches!(
            &text_then_tool.entries[1],
            ConversationEntry::ToolResult { content, is_error: false } if content == "file body"
        ));

        let tool_then_text = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"file body"},{"type":"text","text":"after"}]}}"#,
        );
        assert_eq!(tool_then_text.user_message_count, 1);
        assert_eq!(tool_then_text.entries.len(), 2);
        assert!(matches!(
            &tool_then_text.entries[0],
            ConversationEntry::ToolResult { content, is_error: false } if content == "file body"
        ));
        assert!(matches!(
            &tool_then_text.entries[1],
            ConversationEntry::UserMessage(text) if text == "after"
        ));
    }

    #[test]
    fn ac3_tool_result_only_adds_no_user_message() {
        let conv = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok","is_error":false}]}}"#,
        );

        assert_eq!(conv.user_message_count, 0);
        assert_eq!(conv.entries.len(), 1);
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::ToolResult { content, is_error: false } if content == "ok"
        ));
    }

    #[test]
    fn ac4_two_text_blocks_join_with_a_blank_line() {
        let conv = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}}"#,
        );

        assert_eq!(conv.user_message_count, 1);
        assert_eq!(conv.entries.len(), 1);
        assert!(matches!(
            &conv.entries[0],
            ConversationEntry::UserMessage(text) if text == "one\n\ntwo"
        ));
    }

    #[test]
    fn is_meta_block_skill_expansion_is_not_a_user_message() {
        let conv = parse_user_line(
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: /tmp/skill"}]}}"#,
        );
        assert_eq!(conv.user_message_count, 0);
        assert!(conv.entries.is_empty());

        let with_tool = parse_user_line(
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: /tmp/skill"},{"type":"tool_result","content":"ok"}]}}"#,
        );
        assert_eq!(with_tool.user_message_count, 0);
        assert!(with_tool
            .entries
            .iter()
            .all(|entry| !matches!(entry, ConversationEntry::UserMessage(_))));
        assert!(matches!(
            &with_tool.entries[..],
            [ConversationEntry::ToolResult { content, is_error: false }] if content == "ok"
        ));
    }

    #[test]
    fn is_meta_string_is_not_a_user_message() {
        let conv = parse_user_line(
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Base directory for this skill: /tmp/skill"}}"#,
        );
        assert_eq!(conv.user_message_count, 0);
        assert!(conv.entries.is_empty());
    }

    #[test]
    fn interrupt_marker_block_is_not_a_user_message() {
        let conv = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"  [Request interrupted by user for tool use]"}]}}"#,
        );
        assert_eq!(conv.user_message_count, 0);
        assert!(conv.entries.is_empty());

        let kept = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"keep this [Request interrupted by user"}]}}"#,
        );
        assert_eq!(kept.user_message_count, 1);
        assert!(matches!(
            &kept.entries[..],
            [ConversationEntry::UserMessage(text)] if text == "keep this [Request interrupted by user"
        ));
    }

    #[test]
    fn image_block_plus_text_block_keeps_the_text() {
        let conv = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"aaaa"}},{"type":"text","text":"look at this"}]}}"#,
        );
        assert_eq!(conv.user_message_count, 1);
        assert!(matches!(
            &conv.entries[..],
            [ConversationEntry::UserMessage(text)] if text == "look at this"
        ));
    }

    #[test]
    fn text_block_with_missing_or_non_string_text_adds_nothing() {
        let conv = parse_user_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text"},{"type":"text","text":null},{"type":"text","text":1}]}}"#,
        );
        assert_eq!(conv.user_message_count, 0);
        assert!(conv.entries.is_empty());
    }
}
