// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Claude Code transcripts — `~/.claude/projects/<project>/<session>.jsonl`.
//!
//! The parser itself is [`crate::jsonl`], unchanged: Claude Code's SessionEnd
//! hook has always used it, thousands of archives were written by it, and its
//! output is what every other adapter is measured against. This adapter adds
//! only the two things the hook never needed — where the files are, and which
//! of them are new.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{modified_at, newer_than, walk_files, Source, Transcript, TranscriptRef};
use crate::conversation::Conversation;
use crate::error::RecallError;

/// Claude Code's session records.
#[derive(Debug, Clone)]
pub struct ClaudeCodeTranscripts {
    projects_dir: PathBuf,
}

impl ClaudeCodeTranscripts {
    /// Read sessions from an explicit `projects/` directory.
    #[must_use]
    pub fn new(projects_dir: PathBuf) -> Self {
        Self { projects_dir }
    }

    /// Read sessions from this machine's Claude Code installation.
    ///
    /// Honours [`crate::paths::CLAUDE_DIR_ENV`], like every other Claude Code
    /// path in the crate, so a test never reaches the real `~/.claude`.
    #[must_use]
    pub fn detect() -> Option<Self> {
        let claude_dir = match std::env::var_os(crate::paths::CLAUDE_DIR_ENV) {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()?.join(".claude"),
        };
        Some(Self::new(claude_dir.join("projects")))
    }
}

impl Transcript for ClaudeCodeTranscripts {
    fn source(&self) -> Source {
        Source::ClaudeCode
    }

    fn sessions_root(&self) -> &Path {
        &self.projects_dir
    }

    fn discover(&self, since: Option<SystemTime>) -> Result<Vec<TranscriptRef>, RecallError> {
        let found = walk_files(&self.projects_dir, "jsonl", 0)
            .into_iter()
            .filter_map(|path| {
                let session_id = path.file_stem()?.to_str()?.to_string();
                Some(TranscriptRef {
                    source: Source::ClaudeCode,
                    session_id,
                    modified: modified_at(&path),
                    path,
                    cwd: None,
                })
            })
            .collect();
        Ok(newer_than(found, since))
    }

    fn parse(&self, transcript: &TranscriptRef) -> Result<Conversation, RecallError> {
        crate::jsonl::parse_transcript(&transcript.path.to_string_lossy(), &transcript.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One user turn, one thinking block, one tool call, one tool result — the
    /// same shapes `jsonl`'s own fixture uses, scrubbed of anything personal.
    const TRANSCRIPT: &str = concat!(
        r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-03-05T14:30:00.000Z"}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-03-05T14:30:00.100Z","message":{"role":"user","content":"Can you read the auth module?"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-03-05T14:30:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private","signature":"sig"}]}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-03-05T14:30:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Reading it now."}]}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-03-05T14:30:07.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/src/auth.rs"}}]}}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-03-05T14:30:08.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"pub fn authenticate() {}"}]}}"#,
        "\n",
    );

    fn fixture_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects").join("-home-dev-app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sess-abc.jsonl"), TRANSCRIPT).unwrap();
        tmp
    }

    #[test]
    fn discovery_finds_sessions_under_project_directories() {
        let tmp = fixture_tree();
        let adapter = ClaudeCodeTranscripts::new(tmp.path().join("projects"));

        let found = adapter.discover(None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "sess-abc");
        assert_eq!(found[0].source, Source::ClaudeCode);
        assert!(adapter.is_installed());
    }

    /// The one adapter with existing users: its output must not move.
    #[test]
    fn parsing_is_identical_to_the_original_jsonl_parser() {
        let tmp = fixture_tree();
        let adapter = ClaudeCodeTranscripts::new(tmp.path().join("projects"));
        let found = adapter.discover(None).unwrap();

        let through_adapter = adapter.parse(&found[0]).unwrap();
        let path = found[0].path.to_string_lossy().to_string();
        let directly = crate::jsonl::parse_transcript(&path, "sess-abc").unwrap();

        assert_eq!(
            crate::conversation::conversation_to_markdown(&through_adapter, 1),
            crate::conversation::conversation_to_markdown(&directly, 1)
        );
        assert_eq!(
            through_adapter.user_message_count,
            directly.user_message_count
        );
        assert_eq!(
            through_adapter.assistant_message_count,
            directly.assistant_message_count
        );
        assert_eq!(through_adapter.first_timestamp, directly.first_timestamp);
        assert_eq!(through_adapter.last_timestamp, directly.last_timestamp);
    }

    #[test]
    fn a_missing_installation_discovers_nothing() {
        let adapter = ClaudeCodeTranscripts::new(PathBuf::from("/nonexistent/.claude/projects"));
        assert!(!adapter.is_installed());
        assert!(adapter.discover(None).unwrap().is_empty());
    }
}
