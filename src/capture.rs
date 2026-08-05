// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capture — importing sessions from the agent CLIs a user actually runs.
//!
//! [`crate::transcript`] says where each CLI's sessions live and how to read
//! one. This module decides *which* of them to read and turns them into
//! archives, for the two callers that need it: `recall-echo ingest` on demand,
//! and the daemon's background sweep once the machine is quiet.
//!
//! # Not ingesting anything twice
//!
//! Two filters, and the cheap one is not the authoritative one.
//!
//! - A **watermark** per CLI (`capture/<cli>.watermark`) holds the last write
//!   time already dealt with, so a sweep does not re-read a year of transcripts
//!   to learn it has nothing to do. It advances only over transcripts that were
//!   actually handled, and stops advancing at the first failure — losing a
//!   watermark costs a rescan, and that is the only thing it may ever cost.
//! - The **archives themselves** decide. A session whose id already appears in
//!   `conversations/` is skipped, and the check is made twice: once on the id
//!   discovery derived from the file's name, and again on the id the parsed
//!   transcript reports, in case a CLI's two answers ever disagree.
//!
//! # Sessions that are still going
//!
//! A transcript is a live file. Importing one mid-session would archive half a
//! conversation and then mark that session captured forever, so a transcript is
//! only imported once it has been untouched for `[capture] settle_secs`. That
//! is also what stops `recall-echo ingest`, run from inside a Codex or Grok
//! session, from capturing the session it is being run from.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::archive::{self, ArchiveResult};
use crate::config::CaptureSection;
use crate::error::RecallError;
use crate::summarize;
use crate::transcript::{adapter_for, Source, Transcript, TranscriptRef};

/// Directory holding one watermark file per CLI.
const WATERMARK_DIR: &str = "capture";

// ── Options ──────────────────────────────────────────────────────────────

/// How a sweep decides what is ready.
#[derive(Debug, Clone, Copy)]
pub struct CaptureOptions {
    /// How long a transcript must have been untouched to count as finished.
    pub settle: Duration,
    /// The instant the sweep is reasoning about. Injectable so a test does not
    /// have to wait for a file to age.
    pub now: SystemTime,
}

impl CaptureOptions {
    /// The options a `[capture]` section describes, as of now.
    #[must_use]
    pub fn from_config(config: &CaptureSection) -> Self {
        Self {
            settle: config.settle(),
            now: SystemTime::now(),
        }
    }
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self::from_config(&CaptureSection::default())
    }
}

// ── What a sweep found ───────────────────────────────────────────────────

/// The transcripts of one CLI, sorted into what a sweep may do with them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Finished, unarchived transcripts, oldest first.
    pub ready: Vec<TranscriptRef>,
    /// Transcripts still being written.
    pub active: u32,
    /// Transcripts whose session is already archived.
    pub duplicates: u32,
}

/// What one CLI's sweep did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CaptureReport {
    /// Log numbers written, in the order they were written.
    pub archived: Vec<u32>,
    /// Transcripts that held no user turn, so there was nothing to archive.
    pub empty: u32,
    pub duplicates: u32,
    pub active: u32,
    pub failed: u32,
}

impl CaptureReport {
    #[must_use]
    pub fn did_something(&self) -> bool {
        !self.archived.is_empty()
    }

    /// One line for a log or a terminal, or `None` when there is nothing to
    /// say — a sweep that found no work stays silent.
    #[must_use]
    pub fn summary(&self, source: Source) -> Option<String> {
        if self.archived.is_empty() && self.failed == 0 {
            return None;
        }
        let numbers: Vec<String> = self
            .archived
            .iter()
            .map(|number| format!("{number:03}"))
            .collect();
        let mut line = format!(
            "captured {} {source} session{} ({})",
            self.archived.len(),
            if self.archived.len() == 1 { "" } else { "s" },
            if numbers.is_empty() {
                "\u{2014}".to_string()
            } else {
                numbers.join(", ")
            }
        );
        if self.failed > 0 {
            line.push_str(&format!(", {} failed", self.failed));
        }
        if self.duplicates > 0 {
            line.push_str(&format!(", {} already archived", self.duplicates));
        }
        if self.active > 0 {
            line.push_str(&format!(", {} still active", self.active));
        }
        Some(line)
    }
}

// ── Selection ────────────────────────────────────────────────────────────

/// The transcripts of one CLI that are worth reading right now.
///
/// `archived` is passed in rather than read here so a sweep over several CLIs
/// scans `conversations/` once instead of once per CLI.
pub fn pending(
    memory_dir: &Path,
    adapter: &dyn Transcript,
    archived: &HashSet<String>,
    options: CaptureOptions,
) -> Result<Pending, RecallError> {
    let watermark = read_watermark(memory_dir, adapter.source());

    let mut pending = Pending::default();
    for transcript in adapter.discover(watermark)? {
        if transcript.age_at(options.now) < options.settle {
            pending.active += 1;
        } else if archived.contains(&transcript.session_id) {
            pending.duplicates += 1;
        } else {
            pending.ready.push(transcript);
        }
    }
    Ok(pending)
}

/// Session ids already represented in `conversations/`.
#[must_use]
pub fn archived_sessions(memory_dir: &Path) -> HashSet<String> {
    archive::collect_archived_sessions(&memory_dir.join("conversations"))
}

// ── Archiving one transcript ─────────────────────────────────────────────

/// Read one transcript and archive it.
///
/// `Ok(None)` means the transcript turned out to be already archived under the
/// id its contents report — the second half of the double-ingest check.
pub fn archive_transcript(
    memory_dir: &Path,
    adapter: &dyn Transcript,
    transcript: &TranscriptRef,
    archived: &HashSet<String>,
) -> Result<Option<ArchiveResult>, RecallError> {
    let conv = adapter.parse(transcript)?;
    if archived.contains(&conv.session_id) {
        return Ok(None);
    }
    let summary = summarize::algorithmic_summary(&conv);
    let result =
        archive::archive_conversation(memory_dir, &conv, &summary, adapter.source().as_str())?;
    Ok(Some(result))
}

// ── Watermarks ───────────────────────────────────────────────────────────

fn watermark_path(memory_dir: &Path, source: Source) -> PathBuf {
    memory_dir
        .join(WATERMARK_DIR)
        .join(format!("{source}.watermark"))
}

/// The last write time this CLI has been swept up to, if any.
#[must_use]
pub fn read_watermark(memory_dir: &Path, source: Source) -> Option<SystemTime> {
    let raw = std::fs::read_to_string(watermark_path(memory_dir, source)).ok()?;
    let seconds: u64 = raw.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Record how far this CLI has been swept.
///
/// Best effort by design: an unwritable watermark means the next sweep rescans
/// and finds the same archives already there, which is slow, not wrong.
pub fn write_watermark(memory_dir: &Path, source: Source, mark: SystemTime) {
    let Ok(since_epoch) = mark.duration_since(SystemTime::UNIX_EPOCH) else {
        return;
    };
    let path = watermark_path(memory_dir, source);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::write(path, format!("{}\n", since_epoch.as_secs()));
}

/// Tracks how far a sweep may claim to have got.
///
/// The rule is one sentence: never past a transcript that failed. A failure
/// pins the watermark to just before it, so the next sweep tries that
/// transcript again — and the archive check keeps the ones that succeeded
/// after it from being imported twice.
#[derive(Debug, Default)]
pub struct Watermark {
    reached: Option<SystemTime>,
    blocked: bool,
}

impl Watermark {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A transcript was dealt with — archived, empty, or a known duplicate.
    pub fn handled(&mut self, transcript: &TranscriptRef) {
        if !self.blocked {
            self.reached = Some(transcript.modified);
        }
    }

    /// A transcript failed; the watermark stops here.
    pub fn failed(&mut self) {
        self.blocked = true;
    }

    /// How far the sweep got, if anywhere.
    #[must_use]
    pub fn reached(&self) -> Option<SystemTime> {
        self.reached
    }

    /// Persist the mark, when there is one.
    pub fn commit(&self, memory_dir: &Path, source: Source) {
        if let Some(mark) = self.reached {
            write_watermark(memory_dir, source, mark);
        }
    }
}

// ── The synchronous sweep (the `ingest` command) ─────────────────────────

/// Import one CLI's finished, unarchived sessions.
///
/// Each transcript is archived and ingested into the graph before the next one
/// is read, so an interrupted import leaves complete archives and a watermark
/// that points at the last of them.
pub fn sweep(
    memory_dir: &Path,
    adapter: &dyn Transcript,
    options: CaptureOptions,
) -> Result<CaptureReport, RecallError> {
    let mut archived_ids = archived_sessions(memory_dir);
    let found = pending(memory_dir, adapter, &archived_ids, options)?;
    let mut report = CaptureReport {
        active: found.active,
        duplicates: found.duplicates,
        ..CaptureReport::default()
    };
    let mut watermark = Watermark::new();

    for transcript in &found.ready {
        match archive_transcript(memory_dir, adapter, transcript, &archived_ids) {
            Ok(None) => {
                report.duplicates += 1;
                watermark.handled(transcript);
            }
            Ok(Some(result)) => {
                archived_ids.insert(result.session_id.clone());
                if result.log_number == 0 {
                    report.empty += 1;
                } else {
                    report.archived.push(result.log_number);
                    archive::graph_ingest(memory_dir, &result);
                }
                watermark.handled(transcript);
            }
            Err(err) => {
                eprintln!(
                    "recall-echo: skipping {} session {} \u{2014} {err}",
                    adapter.source(),
                    transcript.session_id
                );
                report.failed += 1;
                watermark.failed();
            }
        }
    }

    watermark.commit(memory_dir, adapter.source());
    if report.did_something() {
        archive::pipeline_sync_on_archive(memory_dir);
    }
    Ok(report)
}

/// Import every configured CLI's finished sessions, reporting to stderr.
///
/// This is `recall-echo ingest`. A CLI that is not installed is not an error:
/// it has no sessions, which is exactly nothing to import.
pub fn ingest(memory_dir: &Path, sources: &[Source]) -> Result<(), RecallError> {
    if !memory_dir.join("conversations").exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run `recall-echo init` first.".into(),
        ));
    }

    let config = crate::config::load_from_dir(memory_dir);
    let options = CaptureOptions::from_config(&config.capture);
    let mut total = 0usize;

    for source in sources {
        let Some(adapter) = adapter_for(*source) else {
            continue;
        };
        if !adapter.is_installed() {
            eprintln!(
                "recall-echo: {source} has no sessions at {}",
                adapter.sessions_root().display()
            );
            continue;
        }
        let report = sweep(memory_dir, adapter.as_ref(), options)?;
        total += report.archived.len();
        match report.summary(*source) {
            Some(line) => eprintln!("recall-echo: {line}"),
            None => eprintln!("recall-echo: no new {source} sessions"),
        }
    }

    if total == 0 {
        eprintln!("recall-echo: nothing new to import");
    }
    Ok(())
}

/// The CLIs `ingest` and the daemon sweep work on, given a config.
///
/// An explicit `[capture] sources` wins; otherwise every CLI that has recorded
/// sessions on this machine is captured, which is the behaviour that makes the
/// memory lifecycle mechanical for a user who never read the docs.
#[must_use]
pub fn configured_sources(config: &CaptureSection) -> Vec<Source> {
    match config.sources {
        Some(ref sources) if !sources.is_empty() => sources.clone(),
        _ => crate::transcript::detect_installed()
            .iter()
            .map(|adapter| adapter.source())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::CodexTranscripts;

    const ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-08-05T22:29:00.878Z","type":"session_meta","payload":{"session_id":"SESSION","cwd":"/tmp/probe"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:02.329Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"a question about the parser"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-08-05T22:29:04.028Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"an answer"}]}}"#,
        "\n",
    );

    struct Fixture {
        _tmp: tempfile::TempDir,
        memory: PathBuf,
        sessions: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let memory = tmp.path().join("memory");
            std::fs::create_dir_all(memory.join("conversations")).unwrap();
            let sessions = tmp.path().join("sessions");
            std::fs::create_dir_all(sessions.join("2026/08/05")).unwrap();
            Self {
                _tmp: tmp,
                memory,
                sessions,
            }
        }

        fn write_session(&self, uuid: &str) -> PathBuf {
            let path = self
                .sessions
                .join("2026/08/05")
                .join(format!("rollout-2026-08-05T22-29-00-{uuid}.jsonl"));
            std::fs::write(&path, ROLLOUT.replace("SESSION", uuid)).unwrap();
            path
        }

        fn adapter(&self) -> CodexTranscripts {
            CodexTranscripts::new(self.sessions.clone())
        }
    }

    fn settled() -> CaptureOptions {
        CaptureOptions {
            settle: Duration::from_secs(0),
            now: SystemTime::now(),
        }
    }

    #[test]
    fn a_finished_session_is_archived_once_and_never_again() {
        let fixture = Fixture::new();
        fixture.write_session("019fd40b-55d5-7a72-8ecb-611abc36879e");
        let adapter = fixture.adapter();

        let first = sweep(&fixture.memory, &adapter, settled()).unwrap();
        assert_eq!(first.archived, vec![1]);
        assert!(fixture
            .memory
            .join("conversations/conversation-001.md")
            .exists());

        let second = sweep(&fixture.memory, &adapter, settled()).unwrap();
        assert!(second.archived.is_empty());
        assert_eq!(
            std::fs::read_dir(fixture.memory.join("conversations"))
                .unwrap()
                .count(),
            1
        );
    }

    /// Even with the watermark thrown away, the archives themselves must
    /// prevent a second copy.
    #[test]
    fn losing_the_watermark_does_not_cause_a_second_copy() {
        let fixture = Fixture::new();
        fixture.write_session("019fd40b-55d5-7a72-8ecb-611abc36879e");
        let adapter = fixture.adapter();

        sweep(&fixture.memory, &adapter, settled()).unwrap();
        std::fs::remove_dir_all(fixture.memory.join(WATERMARK_DIR)).unwrap();

        let again = sweep(&fixture.memory, &adapter, settled()).unwrap();
        assert!(again.archived.is_empty());
        assert_eq!(again.duplicates, 1);
    }

    #[test]
    fn the_watermark_records_the_last_transcript_handled() {
        let fixture = Fixture::new();
        let path = fixture.write_session("019fd40b-55d5-7a72-8ecb-611abc36879e");
        let adapter = fixture.adapter();

        sweep(&fixture.memory, &adapter, settled()).unwrap();

        let mark = read_watermark(&fixture.memory, Source::Codex).expect("a watermark");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Second granularity: the watermark may trail the file by under a second.
        assert!(
            modified.duration_since(mark).unwrap_or_default() < Duration::from_secs(1),
            "watermark {mark:?} vs file {modified:?}"
        );
    }

    #[test]
    fn a_live_session_is_left_alone_until_it_settles() {
        let fixture = Fixture::new();
        fixture.write_session("019fd40b-55d5-7a72-8ecb-611abc36879e");
        let adapter = fixture.adapter();

        let options = CaptureOptions {
            settle: Duration::from_secs(3600),
            now: SystemTime::now(),
        };
        let report = sweep(&fixture.memory, &adapter, options).unwrap();
        assert!(report.archived.is_empty());
        assert_eq!(report.active, 1);
        assert!(read_watermark(&fixture.memory, Source::Codex).is_none());
    }

    #[test]
    fn each_session_becomes_its_own_archive() {
        let fixture = Fixture::new();
        fixture.write_session("019fd40b-55d5-7a72-8ecb-611abc36879e");
        fixture.write_session("019fd40c-55d5-7a72-8ecb-611abc36879e");
        let adapter = fixture.adapter();

        let report = sweep(&fixture.memory, &adapter, settled()).unwrap();
        assert_eq!(report.archived.len(), 2);

        let index = std::fs::read_to_string(fixture.memory.join("ARCHIVE.md")).unwrap();
        assert!(index.contains("| 001 |"), "{index}");
        assert!(index.contains("| 002 |"), "{index}");
    }

    #[test]
    fn the_archive_records_which_cli_it_came_from() {
        let fixture = Fixture::new();
        fixture.write_session("019fd40b-55d5-7a72-8ecb-611abc36879e");
        sweep(&fixture.memory, &fixture.adapter(), settled()).unwrap();

        let archive =
            std::fs::read_to_string(fixture.memory.join("conversations/conversation-001.md"))
                .unwrap();
        assert!(archive.contains("source: \"codex\""), "{archive}");
        assert!(
            archive.contains("session_id: \"019fd40b-55d5-7a72-8ecb-611abc36879e\""),
            "{archive}"
        );
    }

    #[test]
    fn a_failed_transcript_pins_the_watermark_before_it() {
        let epoch = SystemTime::UNIX_EPOCH;
        let at = |secs: u64| TranscriptRef {
            source: Source::Codex,
            session_id: format!("s{secs}"),
            path: PathBuf::from("/tmp/x"),
            modified: epoch + Duration::from_secs(secs),
            cwd: None,
        };

        let mut watermark = Watermark::new();
        watermark.handled(&at(10));
        watermark.failed();
        watermark.handled(&at(30));

        assert_eq!(watermark.reached(), Some(epoch + Duration::from_secs(10)));
    }

    #[test]
    fn a_watermark_survives_a_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_watermark(tmp.path(), Source::Grok).is_none());

        let mark = SystemTime::UNIX_EPOCH + Duration::from_secs(1_754_432_940);
        write_watermark(tmp.path(), Source::Grok, mark);
        assert_eq!(read_watermark(tmp.path(), Source::Grok), Some(mark));
    }

    #[test]
    fn configured_sources_prefer_the_config_over_detection() {
        let config = CaptureSection {
            sources: Some(vec![Source::Grok]),
            ..CaptureSection::default()
        };
        assert_eq!(configured_sources(&config), vec![Source::Grok]);
    }

    #[test]
    fn ingest_refuses_an_uninitialized_memory_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ingest(tmp.path(), &[Source::Codex]).is_err());
    }
}
