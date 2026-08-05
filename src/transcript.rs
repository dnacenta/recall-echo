// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Transcript adapters — reading any agent CLI's session records.
//!
//! recall-echo's claim is that the memory lifecycle is mechanical rather than
//! on the honor system. Until this module existed that was true for exactly one
//! editor: `init` installed hooks into Claude Code, and [`crate::jsonl`] parsed
//! Claude Code's transcript format, so a Codex or Grok user could *read* memory
//! over MCP while nothing ever wrote any.
//!
//! Every agent CLI already records its sessions to disk. An adapter says where
//! those records live and how to read one; everything downstream — archival,
//! EPHEMERAL.md, graph ingest, per-turn provenance — is unchanged, because an
//! adapter's only output is the [`Conversation`] the rest of the crate already
//! speaks.
//!
//! # The contract every adapter owes
//!
//! [`Transcript::parse`] returns *what the two parties said*, and nothing else:
//!
//! - a human turn becomes [`ConversationEntry::UserMessage`] — `user` evidence
//!   to the confidence model,
//! - a model turn becomes [`ConversationEntry::AssistantText`] — `self`
//!   evidence, worth far less,
//! - harness text is not a turn at all. System prompts, developer instructions,
//!   injected reminders and private reasoning are dropped.
//!
//! That last line is the whole reason provenance means anything. A system
//! prompt recorded under `role: "user"` would enter the graph as something the
//! user asserted, and the model's own unasserted thinking would enter as
//! something it concluded. Each adapter documents the *verified* signal it uses
//! to tell a real turn from an injected one.

pub mod claude_code;
pub mod codex;
pub mod grok;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::conversation::Conversation;
use crate::error::RecallError;

pub use claude_code::ClaudeCodeTranscripts;
pub use codex::CodexTranscripts;
pub use grok::GrokTranscripts;

/// How deep discovery walks below a CLI's session root.
///
/// Codex nests by `YYYY/MM/DD`, Grok by `<encoded cwd>/<session>`, Claude Code
/// by project directory. Four levels covers all three with room to spare, and
/// bounds the walk on a directory that is not what we think it is.
const MAX_DISCOVERY_DEPTH: usize = 4;

// ── Source ───────────────────────────────────────────────────────────────

/// An agent CLI whose transcripts recall-echo can read.
///
/// The string form is the one used everywhere a human names a CLI:
/// `[capture] sources`, `recall-echo ingest --from`, and the `source:` field of
/// an archive's frontmatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    ClaudeCode,
    Codex,
    Grok,
}

impl Source {
    /// Every CLI with an adapter, in a stable order.
    pub const ALL: [Source; 3] = [Source::ClaudeCode, Source::Codex, Source::Grok];

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::ClaudeCode => "claude-code",
            Source::Codex => "codex",
            Source::Grok => "grok",
        }
    }

    pub fn from_str_loose(s: &str) -> Result<Self, RecallError> {
        match s.trim().to_lowercase().as_str() {
            "claude-code" | "claudecode" | "claude" => Ok(Source::ClaudeCode),
            "codex" | "codex-cli" => Ok(Source::Codex),
            "grok" | "grok-cli" => Ok(Source::Grok),
            other => Err(RecallError::Config(format!(
                "unknown transcript source: {other} (use 'claude-code', 'codex', or 'grok')"
            ))),
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── A discovered session ─────────────────────────────────────────────────

/// One session record on disk, as discovery found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRef {
    pub source: Source,
    /// The CLI's own session identifier — what archives are deduplicated on.
    pub session_id: String,
    pub path: PathBuf,
    /// Last write. Both the ordering key and the watermark.
    pub modified: SystemTime,
    /// Working directory the session ran in, when the CLI records one.
    pub cwd: Option<String>,
}

impl TranscriptRef {
    /// Age at `now`, or zero for a file written in the future (clock skew).
    #[must_use]
    pub fn age_at(&self, now: SystemTime) -> std::time::Duration {
        now.duration_since(self.modified).unwrap_or_default()
    }
}

// ── The adapter ──────────────────────────────────────────────────────────

/// A CLI's on-disk session records, as recall-echo reads them.
///
/// Implementors are constructed against an explicit root, so a test drives one
/// over a tempdir tree and the real thing over `$HOME`.
pub trait Transcript: Send + Sync {
    /// Which CLI this adapter reads.
    fn source(&self) -> Source;

    /// Directory the CLI records sessions in — whether or not it exists.
    fn sessions_root(&self) -> &Path;

    /// Sessions written strictly after `since`, oldest first.
    ///
    /// A missing root is not an error: a CLI that has never run has no
    /// sessions, which is exactly an empty list.
    fn discover(&self, since: Option<SystemTime>) -> Result<Vec<TranscriptRef>, RecallError>;

    /// Read one discovered session into the universal conversation format.
    fn parse(&self, transcript: &TranscriptRef) -> Result<Conversation, RecallError>;

    /// True when this CLI has recorded at least one session on this machine.
    fn is_installed(&self) -> bool {
        self.sessions_root().exists()
    }
}

/// The adapter for one source, rooted at that CLI's default location.
///
/// `None` when the home directory cannot be determined — the only case in
/// which no adapter can be built at all.
#[must_use]
pub fn adapter_for(source: Source) -> Option<Box<dyn Transcript>> {
    match source {
        Source::ClaudeCode => ClaudeCodeTranscripts::detect().map(boxed),
        Source::Codex => CodexTranscripts::detect().map(boxed),
        Source::Grok => GrokTranscripts::detect().map(boxed),
    }
}

fn boxed<T: Transcript + 'static>(adapter: T) -> Box<dyn Transcript> {
    Box::new(adapter)
}

/// Every adapter whose CLI has actually recorded sessions here.
///
/// This is what `[capture] sources` defaults to: capture from the CLIs the user
/// demonstrably uses, and stay silent about the ones they do not.
#[must_use]
pub fn detect_installed() -> Vec<Box<dyn Transcript>> {
    Source::ALL
        .iter()
        .filter_map(|source| adapter_for(*source))
        .filter(|adapter| adapter.is_installed())
        .collect()
}

// ── Shared parsing helpers ───────────────────────────────────────────────

/// The text of a content field, whichever shape the CLI chose for it.
///
/// This is not defensive coding, it is the actual disagreement: within a single
/// Grok transcript a user turn's `content` is an array of `{type,text}` blocks
/// and an assistant turn's `content` is a bare string. Codex always uses an
/// array of `input_text` / `output_text` blocks. One helper, so no adapter has
/// to care twice.
pub(crate) fn content_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<String> = blocks.iter().map(content_text).collect();
            parts
                .iter()
                .filter(|part| !part.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("")
        }
        serde_json::Value::Object(map) => map
            .get("text")
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Strip `<tag>` … `</tag>` when the text is entirely that wrapper.
///
/// Grok wraps the human's prompt in `<user_query>`; the wrapper is addressed to
/// the model, not part of what the human said.
pub(crate) fn unwrap_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let trimmed = text.trim();
    match trimmed
        .strip_prefix(&open)
        .and_then(|rest| rest.strip_suffix(&close))
    {
        Some(inner) => inner.trim().to_string(),
        None => text.to_string(),
    }
}

/// Decode a percent-encoded path segment.
///
/// Grok names each session directory after the working directory it ran in,
/// percent-encoded (`%2Froot`). Undoing that is a few lines of hex, which is
/// cheaper than a dependency and cannot drift from what we need it to do.
/// Invalid escapes are left verbatim rather than dropped, so a decode failure
/// degrades to a slightly ugly label instead of a wrong path.
#[must_use]
pub(crate) fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &encoded[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| encoded.to_string())
}

/// A filesystem timestamp as the ISO 8601 string conversations use.
pub(crate) fn iso_timestamp(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Last-write time, or the epoch when the filesystem will not say.
pub(crate) fn modified_at(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Files with the given extension under `dir`, walking at most
/// [`MAX_DISCOVERY_DEPTH`] levels. Unreadable directories are skipped.
pub(crate) fn walk_files(dir: &Path, extension: &str, depth: usize) -> Vec<PathBuf> {
    if depth > MAX_DISCOVERY_DEPTH {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path, extension, depth + 1));
        } else if path.extension().is_some_and(|ext| ext == extension) {
            files.push(path);
        }
    }
    files
}

/// Order oldest-first and drop anything at or before the watermark.
///
/// Oldest-first matters downstream: archives are numbered in the order they are
/// written, so ingesting in write order keeps conversation numbers in the same
/// order the conversations happened.
pub(crate) fn newer_than(
    mut found: Vec<TranscriptRef>,
    since: Option<SystemTime>,
) -> Vec<TranscriptRef> {
    if let Some(watermark) = since {
        found.retain(|transcript| transcript.modified > watermark);
    }
    found.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_round_trip() {
        for source in Source::ALL {
            assert_eq!(Source::from_str_loose(source.as_str()).unwrap(), source);
        }
        assert_eq!(
            Source::from_str_loose("  CLAUDE  ").unwrap(),
            Source::ClaudeCode
        );
        assert!(Source::from_str_loose("cursor").is_err());
    }

    #[test]
    fn content_text_reads_a_bare_string() {
        assert_eq!(content_text(&serde_json::json!("OK")), "OK");
    }

    /// Grok's own trap: user content is an array, assistant content is a
    /// string, in the same file.
    #[test]
    fn content_text_reads_an_array_of_blocks() {
        let value = serde_json::json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": " second"},
        ]);
        assert_eq!(content_text(&value), "first second");
    }

    #[test]
    fn content_text_ignores_blocks_without_text() {
        let value = serde_json::json!([{"type": "image", "url": "http://x"}, {"text": "kept"}]);
        assert_eq!(content_text(&value), "kept");
    }

    #[test]
    fn unwrap_tag_strips_only_a_whole_wrapper() {
        assert_eq!(
            unwrap_tag("<user_query>\nhello\n</user_query>", "user_query"),
            "hello"
        );
        assert_eq!(
            unwrap_tag("prefix <user_query>hello</user_query>", "user_query"),
            "prefix <user_query>hello</user_query>"
        );
    }

    #[test]
    fn percent_decode_handles_grok_session_dirs() {
        assert_eq!(percent_decode("%2Froot"), "/root");
        assert_eq!(percent_decode("%2Fopt%2Frecall-echo"), "/opt/recall-echo");
        assert_eq!(percent_decode("plain"), "plain");
        // A stray percent is data, not an escape.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn newer_than_drops_the_watermark_itself_and_sorts_oldest_first() {
        let epoch = SystemTime::UNIX_EPOCH;
        let make = |id: &str, secs: u64| TranscriptRef {
            source: Source::Codex,
            session_id: id.to_string(),
            path: PathBuf::from(format!("/tmp/{id}")),
            modified: epoch + std::time::Duration::from_secs(secs),
            cwd: None,
        };
        let found = vec![make("c", 30), make("a", 10), make("b", 20)];
        let kept = newer_than(found, Some(epoch + std::time::Duration::from_secs(10)));
        let ids: Vec<&str> = kept.iter().map(|t| t.session_id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn walking_a_missing_directory_finds_nothing() {
        assert!(walk_files(Path::new("/nonexistent/nowhere"), "jsonl", 0).is_empty());
    }

    #[test]
    fn walking_finds_nested_files_and_ignores_other_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("2026/08/05");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("rollout-a.jsonl"), "").unwrap();
        std::fs::write(nested.join("notes.txt"), "").unwrap();

        let found = walk_files(tmp.path(), "jsonl", 0);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("rollout-a.jsonl"));
    }
}
