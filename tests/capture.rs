// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for capturing sessions from several agent CLIs at once.
//!
//! The unit tests pin each format's traps; these pin the thing a user sees: a
//! machine where Claude Code, Codex and Grok have all been used ends up with
//! one archive per session, none of them containing harness text, and running
//! the import again changes nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use recall_echo::capture::{self, CaptureOptions};
use recall_echo::transcript::{
    ClaudeCodeTranscripts, CodexTranscripts, GrokTranscripts, Source, Transcript,
};
use tempfile::TempDir;

// ── Fixtures ─────────────────────────────────────────────────────────────
//
// Real shapes, scrubbed and cut down: enough of each format to exercise the
// role handling, never a real transcript.

const CLAUDE_CODE: &str = concat!(
    r#"{"type":"user","timestamp":"2026-08-01T09:00:00.000Z","message":{"role":"user","content":"Explain the retry policy"}}"#,
    "\n",
    r#"{"type":"assistant","timestamp":"2026-08-01T09:00:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"CLAUDE PRIVATE THOUGHT","signature":"s"}]}}"#,
    "\n",
    r#"{"type":"assistant","timestamp":"2026-08-01T09:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"It retries twice, then quarantines."}]}}"#,
    "\n",
);

const CODEX: &str = concat!(
    r#"{"timestamp":"2026-08-02T10:00:00.000Z","type":"session_meta","payload":{"session_id":"019fd40b-55d5-7a72-8ecb-611abc36879e","cwd":"/tmp/probe"}}"#,
    "\n",
    r#"{"timestamp":"2026-08-02T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"CODEX DEVELOPER INSTRUCTIONS"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-08-02T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>CODEX PLUGIN CATALOGUE</recommended_plugins>"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-08-02T10:00:03.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Add a watermark to the importer"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-08-02T10:00:04.000Z","type":"event_msg","payload":{"type":"user_message","message":"Add a watermark to the importer"}}"#,
    "\n",
    r#"{"timestamp":"2026-08-02T10:00:05.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"CODEX ENCRYPTED REASONING"}}"#,
    "\n",
    r#"{"timestamp":"2026-08-02T10:00:06.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done — the watermark advances per transcript."}]}}"#,
    "\n",
);

const GROK: &str = concat!(
    r#"{"type":"system","content":"GROK SYSTEM PROMPT"}"#,
    "\n",
    r#"{"type":"user","content":[{"type":"text","text":"<user_info>GROK USER INFO</user_info>"}]}"#,
    "\n",
    r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>GROK PROJECT INSTRUCTIONS</system-reminder>"}],"synthetic_reason":"project_instructions"}"#,
    "\n",
    r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nCheck the socket permissions\n</user_query>"}],"prompt_index":0}"#,
    "\n",
    r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"GROK PRIVATE REASONING"}],"status":"completed"}"#,
    "\n",
    r#"{"type":"assistant","content":"The socket is owned by the daemon's uid.","model_id":"grok-4.5-build"}"#,
    "\n",
);

/// Text that must never survive into an archive: system prompts, developer
/// instructions, injected catalogues, private reasoning.
const HARNESS_TEXT: &[&str] = &[
    "CLAUDE PRIVATE THOUGHT",
    "CODEX DEVELOPER INSTRUCTIONS",
    "CODEX PLUGIN CATALOGUE",
    "CODEX ENCRYPTED REASONING",
    "GROK SYSTEM PROMPT",
    "GROK USER INFO",
    "GROK PROJECT INSTRUCTIONS",
    "GROK PRIVATE REASONING",
];

// ── Harness ──────────────────────────────────────────────────────────────

struct Machine {
    _tmp: TempDir,
    memory: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    grok: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let memory = root.join("memory");
        std::fs::create_dir_all(memory.join("conversations")).unwrap();

        let claude = root.join(".claude/projects/-home-dev-app");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("cc-session-1.jsonl"), CLAUDE_CODE).unwrap();

        let codex = root.join(".codex/sessions/2026/08/02");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-2026-08-02T10-00-00-019fd40b-55d5-7a72-8ecb-611abc36879e.jsonl"),
            CODEX,
        )
        .unwrap();

        let grok = root.join(".grok/sessions/%2Ftmp%2Fprobe/019fd40b-8e19-7742");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("chat_history.jsonl"), GROK).unwrap();

        Self {
            memory: memory.clone(),
            claude: root.join(".claude/projects"),
            codex: root.join(".codex/sessions"),
            grok: root.join(".grok/sessions"),
            _tmp: tmp,
        }
    }

    fn adapters(&self) -> Vec<Box<dyn Transcript>> {
        vec![
            Box::new(ClaudeCodeTranscripts::new(self.claude.clone())),
            Box::new(CodexTranscripts::new(self.codex.clone())),
            Box::new(GrokTranscripts::new(self.grok.clone())),
        ]
    }

    fn sweep_all(&self) -> Vec<u32> {
        let mut archived = Vec::new();
        for adapter in self.adapters() {
            let report = capture::sweep(&self.memory, adapter.as_ref(), settled()).unwrap();
            archived.extend(report.archived);
        }
        archived
    }

    fn archives(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.memory.join("conversations"))
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        files.sort();
        files
            .iter()
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect()
    }
}

fn settled() -> CaptureOptions {
    CaptureOptions {
        settle: Duration::ZERO,
        now: SystemTime::now(),
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// ── Tests ────────────────────────────────────────────────────────────────

#[test]
fn every_cli_on_the_machine_becomes_an_archive() {
    let machine = Machine::new();
    let archived = machine.sweep_all();

    assert_eq!(archived, vec![1, 2, 3]);
    let archives = machine.archives();
    assert_eq!(archives.len(), 3);

    let all = archives.join("\n");
    assert!(all.contains("source: \"claude-code\""), "{all}");
    assert!(all.contains("source: \"codex\""), "{all}");
    assert!(all.contains("source: \"grok\""), "{all}");

    // Each CLI's human turn is what the archive opens on.
    assert!(all.contains("Explain the retry policy"), "{all}");
    assert!(all.contains("Add a watermark to the importer"), "{all}");
    assert!(all.contains("Check the socket permissions"), "{all}");
}

/// The reason the adapters bother distinguishing turn kinds at all: what ends
/// up in memory as `user` evidence must be something the user actually said.
#[test]
fn no_harness_text_survives_into_any_archive() {
    let machine = Machine::new();
    machine.sweep_all();

    let all = machine.archives().join("\n");
    for forbidden in HARNESS_TEXT {
        assert!(
            !all.contains(forbidden),
            "{forbidden} leaked into an archive"
        );
    }
}

#[test]
fn the_index_and_the_ephemeral_window_see_every_session() {
    let machine = Machine::new();
    machine.sweep_all();

    let index = read(&machine.memory.join("ARCHIVE.md"));
    assert!(index.contains("| 001 |"), "{index}");
    assert!(index.contains("| 002 |"), "{index}");
    assert!(index.contains("| 003 |"), "{index}");

    let ephemeral = read(&machine.memory.join("EPHEMERAL.md"));
    assert!(ephemeral.contains("conversation-001.md"), "{ephemeral}");
    assert!(ephemeral.contains("conversation-003.md"), "{ephemeral}");
}

#[test]
fn importing_twice_imports_nothing_the_second_time() {
    let machine = Machine::new();
    machine.sweep_all();
    let second = machine.sweep_all();

    assert!(second.is_empty(), "second import archived {second:?}");
    assert_eq!(machine.archives().len(), 3);
}

/// The watermark is an optimisation, not the record. Delete it and the
/// archives themselves still refuse a second copy.
#[test]
fn deleting_the_watermarks_does_not_cause_duplicates() {
    let machine = Machine::new();
    machine.sweep_all();
    std::fs::remove_dir_all(machine.memory.join("capture")).unwrap();

    assert!(machine.sweep_all().is_empty());
    assert_eq!(machine.archives().len(), 3);
}

#[test]
fn a_watermark_is_written_for_each_cli_that_was_swept() {
    let machine = Machine::new();
    machine.sweep_all();

    for source in Source::ALL {
        assert!(
            capture::read_watermark(&machine.memory, source).is_some(),
            "no watermark for {source}"
        );
    }
}

/// A new session arriving after an import is the only thing the next import
/// picks up.
#[test]
fn a_session_written_later_is_picked_up_on_the_next_import() {
    let machine = Machine::new();
    machine.sweep_all();

    let later = machine
        .codex
        .join("2026/08/02/rollout-2026-08-02T11-00-00-019fd40c-55d5-7a72-8ecb-611abc36879e.jsonl");
    std::fs::write(
        &later,
        CODEX.replace(
            "019fd40b-55d5-7a72-8ecb-611abc36879e",
            "019fd40c-55d5-7a72-8ecb-611abc36879e",
        ),
    )
    .unwrap();

    assert_eq!(machine.sweep_all(), vec![4]);
    assert_eq!(machine.archives().len(), 4);
}

#[test]
fn a_live_transcript_is_not_imported_until_it_settles() {
    let machine = Machine::new();
    let options = CaptureOptions {
        settle: Duration::from_secs(3600),
        now: SystemTime::now(),
    };

    for adapter in machine.adapters() {
        let report = capture::sweep(&machine.memory, adapter.as_ref(), options).unwrap();
        assert!(report.archived.is_empty());
        assert_eq!(report.active, 1);
    }
    assert!(machine.archives().is_empty());
}

#[test]
fn discovery_respects_a_watermark() {
    let machine = Machine::new();
    let adapter = CodexTranscripts::new(machine.codex.clone());

    let all = adapter.discover(None).unwrap();
    assert_eq!(all.len(), 1);

    let after = adapter.discover(Some(all[0].modified)).unwrap();
    assert!(after.is_empty());
}
