// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background transcript capture inside the graph daemon.
//!
//! [`crate::serve_extract`] made entity extraction mechanical; this makes
//! *arrival* mechanical for the CLIs that have no hooks. Claude Code archives
//! itself on `SessionEnd`; Codex and Grok write a transcript to disk and tell
//! nobody, so once the machine has been quiet the daemon reads what they wrote.
//!
//! It borrows the extraction worker's discipline and none of its schedule:
//!
//! - **The extraction worker is untouched.** This worker reads the same
//!   [`IdleTracker`] to learn whether the machine is quiet and never writes to
//!   it, so extraction's quiet period cannot be reset by a capture sweep. The
//!   two are ordinary concurrent users of the daemon's store, like two clients.
//! - **A hot request wins.** The batch stops at the next transcript boundary
//!   when a client connects, and yields the runtime between transcripts.
//! - **Crash-only, per transcript.** One transcript is archived, ingested and
//!   marked before the next is read, so an interrupted sweep leaves complete
//!   archives and work left to do.
//! - **Shutdown wins.** Every wait and every import runs under
//!   [`ShutdownSignal`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::capture::{self, CaptureOptions};
use crate::error::RecallError;
use crate::graph::{GraphMemory, IngestContext};
use crate::serve::{BackgroundGuard, DaemonLog, IdleTracker, ShutdownSignal};
use crate::transcript::{Source, Transcript, TranscriptRef};

/// Transcripts one sweep imports before going back to waiting.
const BATCH_SIZE: usize = 5;
/// Longest the worker sleeps between quiet checks.
const MAX_POLL: Duration = Duration::from_secs(30);
/// Shortest it sleeps, so a short quiet period stays responsive without
/// spinning.
const MIN_POLL: Duration = Duration::from_millis(100);
/// Attempts one transcript gets before the sweep moves past it. A transcript
/// that cannot be parsed twice will not parse on the hundredth try either, and
/// a stuck one must not block every transcript written after it.
const MAX_ATTEMPTS: u32 = 2;

// ── The unit of work ─────────────────────────────────────────────────────

/// One CLI-transcript import, as the scheduler sees it.
///
/// Behind a trait so the scheduling — when it sweeps, when it yields, when it
/// gives up on a file — is exercised without a store, a daemon or a CLI.
#[async_trait]
pub trait CaptureUnit: Send + Sync {
    /// Finished, unarchived transcripts across the configured CLIs, oldest
    /// first, at most `limit` of them.
    async fn pending(&self, limit: usize) -> Vec<TranscriptRef>;

    /// Archive one transcript and ingest the archive. `Ok(0)` means the
    /// transcript held no user turn, so there was nothing to archive.
    async fn import(&self, transcript: &TranscriptRef) -> Result<u32, RecallError>;

    /// Record that the sweep is past this transcript. Called after a successful
    /// import, and after a transcript is given up on.
    async fn mark_swept(&self, transcript: &TranscriptRef);
}

// ── Schedule ─────────────────────────────────────────────────────────────

/// When the worker sweeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Quiet period before a sweep may start.
    pub idle_after: Duration,
    /// How often the worker re-checks whether the daemon is quiet.
    pub poll_interval: Duration,
}

impl Schedule {
    /// A schedule for a given quiet period.
    ///
    /// The quiet period is read from `[extraction] idle_after_secs` rather than
    /// duplicated into `[capture]`: it answers a question about the *machine*
    /// — is anybody working right now — and there is only one answer to that.
    #[must_use]
    pub fn after(idle_after: Duration) -> Self {
        Self {
            idle_after,
            poll_interval: (idle_after / 4).clamp(MIN_POLL, MAX_POLL),
        }
    }
}

/// Whether background capture should run at all in this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Sweep these CLIs, on this schedule.
    Run(Schedule, Vec<Source>),
    /// Do not sweep, for this reason.
    Off(String),
}

/// Decide whether this daemon captures transcripts in the background.
///
/// Server mode is refused for the same reason background extraction is: there,
/// clients talk to SurrealDB directly and may never start a daemon at all, so a
/// daemon's idea of "quiet" is a fact about an idle socket rather than about
/// the user. `recall-echo ingest` remains the way to capture there.
#[must_use]
pub fn plan(config: &crate::config::Config, graph_mode: &str, sources: Vec<Source>) -> Plan {
    if !config.capture.enabled {
        return Plan::Off("[capture] enabled = false".into());
    }
    if graph_mode == "server" {
        return Plan::Off("[graph] mode = \"server\" — use `recall-echo ingest`".into());
    }
    if sources.is_empty() {
        return Plan::Off("no agent CLI transcripts found on this machine".into());
    }
    Plan::Run(Schedule::after(config.extraction.idle_after()), sources)
}

// ── Worker ───────────────────────────────────────────────────────────────

/// Everything the worker shares with the rest of the daemon.
pub struct WorkerContext {
    pub idle: Arc<IdleTracker>,
    pub shutdown: Arc<ShutdownSignal>,
    pub log: Arc<DaemonLog>,
}

/// How a sweep ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepOutcome {
    /// Nothing to import.
    NoWork,
    /// At least one transcript was imported.
    Worked,
    /// Shutdown arrived.
    Stopped,
}

/// The background capture loop.
pub struct CaptureWorker {
    schedule: Schedule,
    context: WorkerContext,
    /// Failed attempts per transcript, within this daemon's life.
    attempts: HashMap<PathBuf, u32>,
    /// Transcripts this daemon has given up on.
    skipped: HashSet<PathBuf>,
}

impl CaptureWorker {
    #[must_use]
    pub fn new(schedule: Schedule, context: WorkerContext) -> Self {
        Self {
            schedule,
            context,
            attempts: HashMap::new(),
            skipped: HashSet::new(),
        }
    }

    /// Capture in the background until the daemon shuts down.
    pub async fn run(mut self, unit: Arc<dyn CaptureUnit>) {
        loop {
            if self
                .context
                .shutdown
                .sleep_until_stopped(self.schedule.poll_interval)
                .await
            {
                return;
            }
            if !self.is_quiet() {
                continue;
            }
            match self.run_sweep(unit.as_ref()).await {
                SweepOutcome::NoWork | SweepOutcome::Worked => {}
                SweepOutcome::Stopped => return,
            }
        }
    }

    fn is_quiet(&self) -> bool {
        self.context
            .idle
            .is_quiet_at(std::time::Instant::now(), self.schedule.idle_after)
    }

    /// Import up to [`BATCH_SIZE`] transcripts, yielding between each.
    async fn run_sweep(&mut self, unit: &dyn CaptureUnit) -> SweepOutcome {
        let Some(pending) = self.take_pending(unit).await else {
            return SweepOutcome::Stopped;
        };
        if pending.is_empty() {
            return SweepOutcome::NoWork;
        }

        // From here on the daemon may not idle-exit: the sweep is in flight.
        let _busy = BackgroundGuard::new(Arc::clone(&self.context.idle));
        let mut outcome = SweepOutcome::NoWork;

        for transcript in pending {
            if self.context.shutdown.is_triggered() {
                return SweepOutcome::Stopped;
            }
            // A hot request beats background work, always.
            if self.context.idle.has_connections() {
                break;
            }

            match self.context.shutdown.guard(unit.import(&transcript)).await {
                None => return SweepOutcome::Stopped,
                Some(Ok(log_number)) => {
                    unit.mark_swept(&transcript).await;
                    self.attempts.remove(&transcript.path);
                    if log_number > 0 {
                        outcome = SweepOutcome::Worked;
                        self.context.log.log(&format!(
                            "captured {} session {} as log {log_number:03}",
                            transcript.source, transcript.session_id
                        ));
                    }
                }
                Some(Err(err)) => {
                    // Stop the sweep here: the watermark still points before
                    // this transcript, so the next quiet period retries it
                    // rather than silently walking past it.
                    self.record_failure(unit, &transcript, &err).await;
                    break;
                }
            }

            // Hand the runtime back between transcripts so a request that
            // arrived mid-sweep is served before the next file is read.
            tokio::task::yield_now().await;
        }

        outcome
    }

    /// Transcripts worth attempting, or `None` when shutdown arrived.
    async fn take_pending(&mut self, unit: &dyn CaptureUnit) -> Option<Vec<TranscriptRef>> {
        // Over-fetch, then drop what this daemon has given up on, so a
        // quarantined transcript cannot occupy a batch slot forever.
        let limit = BATCH_SIZE + self.skipped.len();
        let pending = self.context.shutdown.guard(unit.pending(limit)).await?;
        Some(
            pending
                .into_iter()
                .filter(|transcript| !self.skipped.contains(&transcript.path))
                .take(BATCH_SIZE)
                .collect(),
        )
    }

    /// Account for a transcript that would not import.
    async fn record_failure(
        &mut self,
        unit: &dyn CaptureUnit,
        transcript: &TranscriptRef,
        err: &RecallError,
    ) {
        let attempts = self.attempts.entry(transcript.path.clone()).or_insert(0);
        *attempts += 1;
        if *attempts >= MAX_ATTEMPTS {
            self.skipped.insert(transcript.path.clone());
            unit.mark_swept(transcript).await;
            self.context.log.log(&format!(
                "capture gave up on {} session {} after {MAX_ATTEMPTS} attempts: {err}",
                transcript.source, transcript.session_id
            ));
        } else {
            self.context.log.log(&format!(
                "capture failed on {} session {}: {err}",
                transcript.source, transcript.session_id
            ));
        }
    }
}

// ── The real unit: transcripts on disk, archives, the daemon's own store ──

/// Capture against the daemon's own store.
///
/// Archiving is exactly what `recall-echo ingest` does; ingestion is exactly
/// what a [`crate::serve::Request::IngestArchive`] does, minus the socket —
/// the daemon does not connect to itself.
pub struct GraphCaptureUnit {
    memory_dir: PathBuf,
    graph: Arc<GraphMemory>,
    adapters: Vec<Box<dyn Transcript>>,
    settle: Duration,
}

impl GraphCaptureUnit {
    #[must_use]
    pub fn new(
        memory_dir: PathBuf,
        graph: Arc<GraphMemory>,
        adapters: Vec<Box<dyn Transcript>>,
        settle: Duration,
    ) -> Self {
        Self {
            memory_dir,
            graph,
            adapters,
            settle,
        }
    }

    fn options(&self) -> CaptureOptions {
        CaptureOptions {
            settle: self.settle,
            now: SystemTime::now(),
        }
    }

    fn adapter_for(&self, source: Source) -> Option<&dyn Transcript> {
        self.adapters
            .iter()
            .find(|adapter| adapter.source() == source)
            .map(AsRef::as_ref)
    }
}

#[async_trait]
impl CaptureUnit for GraphCaptureUnit {
    async fn pending(&self, limit: usize) -> Vec<TranscriptRef> {
        let archived = capture::archived_sessions(&self.memory_dir);
        let options = self.options();
        let mut ready = Vec::new();
        for adapter in &self.adapters {
            match capture::pending(&self.memory_dir, adapter.as_ref(), &archived, options) {
                Ok(found) => ready.extend(found.ready),
                Err(err) => eprintln!("recall-echo: capture discovery failed: {err}"),
            }
        }
        ready.sort_by_key(|transcript| transcript.modified);
        ready.truncate(limit);
        ready
    }

    async fn import(&self, transcript: &TranscriptRef) -> Result<u32, RecallError> {
        let adapter = self
            .adapter_for(transcript.source)
            .ok_or_else(|| RecallError::Other(format!("no adapter for {}", transcript.source)))?;
        let archived = capture::archived_sessions(&self.memory_dir);
        let Some(result) =
            capture::archive_transcript(&self.memory_dir, adapter, transcript, &archived)?
        else {
            return Ok(0);
        };
        if result.log_number == 0 {
            return Ok(0);
        }

        let context = IngestContext::new(result.session_id.clone(), Some(result.log_number));
        self.graph
            .ingest_archive(&result.full_content, &context, None)
            .await?;
        Ok(result.log_number)
    }

    async fn mark_swept(&self, transcript: &TranscriptRef) {
        capture::write_watermark(&self.memory_dir, transcript.source, transcript.modified);
    }
}

// ── Wiring ───────────────────────────────────────────────────────────────

/// Everything [`spawn`] needs from the daemon.
pub struct Setup {
    pub memory_dir: PathBuf,
    pub graph: Arc<GraphMemory>,
    pub idle: Arc<IdleTracker>,
    pub shutdown: Arc<ShutdownSignal>,
    pub log: Arc<DaemonLog>,
}

/// Start the background capture worker, if this config wants one.
///
/// Every refusal is quiet, final and stated once, in the daemon log.
pub fn spawn(setup: Setup) -> Option<tokio::task::JoinHandle<()>> {
    let config = crate::config::load_from_dir(&setup.memory_dir);
    let mode = crate::serve_client::graph_mode(&setup.memory_dir);

    let sources = capture::configured_sources(&config.capture);
    let (schedule, sources) = match plan(&config, &mode, sources) {
        Plan::Run(schedule, sources) => (schedule, sources),
        Plan::Off(reason) => {
            setup.log.log(&format!("background capture off: {reason}"));
            return None;
        }
    };

    if !setup.memory_dir.join("conversations").exists() {
        setup
            .log
            .log("background capture off: no conversations/ directory to archive into");
        return None;
    }

    let adapters: Vec<Box<dyn Transcript>> = sources
        .iter()
        .filter_map(|source| crate::transcript::adapter_for(*source))
        .collect();
    if adapters.is_empty() {
        setup
            .log
            .log("background capture off: none of the configured CLIs could be located");
        return None;
    }

    let names: Vec<String> = adapters
        .iter()
        .map(|adapter| adapter.source().to_string())
        .collect();
    setup.log.log(&format!(
        "background capture on: {}, every {}s of quiet, transcripts idle for {}s",
        names.join(", "),
        schedule.idle_after.as_secs(),
        config.capture.settle_secs,
    ));

    let unit: Arc<dyn CaptureUnit> = Arc::new(GraphCaptureUnit::new(
        setup.memory_dir.clone(),
        Arc::clone(&setup.graph),
        adapters,
        config.capture.settle(),
    ));
    let worker = CaptureWorker::new(
        schedule,
        WorkerContext {
            idle: setup.idle,
            shutdown: setup.shutdown,
            log: setup.log,
        },
    );
    Some(tokio::spawn(worker.run(unit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn config(enabled: bool) -> crate::config::Config {
        crate::config::Config {
            capture: crate::config::CaptureSection {
                enabled,
                ..crate::config::CaptureSection::default()
            },
            ..crate::config::Config::default()
        }
    }

    #[test]
    fn the_default_plan_sweeps_the_installed_clis() {
        let Plan::Run(schedule, sources) = plan(&config(true), "embedded", vec![Source::Codex])
        else {
            panic!("the default config must sweep");
        };
        assert_eq!(schedule.idle_after, Duration::from_secs(120));
        assert_eq!(schedule.poll_interval, Duration::from_secs(30));
        assert_eq!(sources, vec![Source::Codex]);
    }

    #[test]
    fn opting_out_turns_the_worker_off() {
        let Plan::Off(reason) = plan(&config(false), "embedded", vec![Source::Codex]) else {
            panic!("enabled = false must be honored");
        };
        assert!(reason.contains("enabled"), "{reason}");
    }

    #[test]
    fn server_mode_never_captures_in_the_background() {
        let Plan::Off(reason) = plan(&config(true), "server", vec![Source::Codex]) else {
            panic!("server mode has no daemon to schedule against");
        };
        assert!(reason.contains("server"), "{reason}");
    }

    #[test]
    fn a_machine_with_no_agent_clis_has_nothing_to_sweep() {
        let Plan::Off(reason) = plan(&config(true), "embedded", Vec::new()) else {
            panic!("no sources means no worker");
        };
        assert!(reason.contains("no agent CLI"), "{reason}");
    }

    // ── Scheduling, against a fake unit ──────────────────────────────────

    #[derive(Default)]
    struct FakeState {
        imported: Vec<String>,
        swept: Vec<String>,
        failing: HashSet<String>,
    }

    struct FakeUnit {
        transcripts: Vec<TranscriptRef>,
        state: Mutex<FakeState>,
    }

    impl FakeUnit {
        fn new(ids: &[&str], failing: &[&str]) -> Self {
            let transcripts = ids
                .iter()
                .enumerate()
                .map(|(index, id)| TranscriptRef {
                    source: Source::Codex,
                    session_id: (*id).to_string(),
                    path: PathBuf::from(format!("/tmp/{id}.jsonl")),
                    modified: SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64),
                    cwd: None,
                })
                .collect();
            Self {
                transcripts,
                state: Mutex::new(FakeState {
                    failing: failing.iter().map(|id| (*id).to_string()).collect(),
                    ..FakeState::default()
                }),
            }
        }

        fn imported(&self) -> Vec<String> {
            self.state.lock().unwrap().imported.clone()
        }

        fn swept(&self) -> Vec<String> {
            self.state.lock().unwrap().swept.clone()
        }
    }

    #[async_trait]
    impl CaptureUnit for FakeUnit {
        async fn pending(&self, limit: usize) -> Vec<TranscriptRef> {
            let swept = self.state.lock().unwrap().swept.clone();
            self.transcripts
                .iter()
                .filter(|t| !swept.contains(&t.session_id))
                .take(limit)
                .cloned()
                .collect()
        }

        async fn import(&self, transcript: &TranscriptRef) -> Result<u32, RecallError> {
            let mut state = self.state.lock().unwrap();
            if state.failing.contains(&transcript.session_id) {
                return Err(RecallError::Other("unreadable".into()));
            }
            state.imported.push(transcript.session_id.clone());
            Ok(state.imported.len() as u32)
        }

        async fn mark_swept(&self, transcript: &TranscriptRef) {
            self.state
                .lock()
                .unwrap()
                .swept
                .push(transcript.session_id.clone());
        }
    }

    fn worker(idle: &Arc<IdleTracker>, shutdown: &Arc<ShutdownSignal>) -> CaptureWorker {
        CaptureWorker::new(
            Schedule::after(Duration::from_secs(0)),
            WorkerContext {
                idle: Arc::clone(idle),
                shutdown: Arc::clone(shutdown),
                // These tests exercise scheduling, not logging.
                log: Arc::new(DaemonLog::open(std::path::Path::new("/dev/null"), false)),
            },
        )
    }

    #[tokio::test]
    async fn a_sweep_imports_and_marks_every_ready_transcript() {
        let idle = Arc::new(IdleTracker::new(None));
        let shutdown = Arc::new(ShutdownSignal::new());
        let unit = FakeUnit::new(&["a", "b"], &[]);

        let mut worker = worker(&idle, &shutdown);
        assert_eq!(worker.run_sweep(&unit).await, SweepOutcome::Worked);

        assert_eq!(unit.imported(), ["a", "b"]);
        assert_eq!(unit.swept(), ["a", "b"]);
    }

    #[tokio::test]
    async fn nothing_to_import_is_not_work() {
        let idle = Arc::new(IdleTracker::new(None));
        let shutdown = Arc::new(ShutdownSignal::new());
        let unit = FakeUnit::new(&[], &[]);

        let mut worker = worker(&idle, &shutdown);
        assert_eq!(worker.run_sweep(&unit).await, SweepOutcome::NoWork);
    }

    /// A failure must not carry the sweep past the transcript that failed —
    /// the watermark is what would then skip it forever.
    #[tokio::test]
    async fn a_failure_stops_the_sweep_without_marking_it_swept() {
        let idle = Arc::new(IdleTracker::new(None));
        let shutdown = Arc::new(ShutdownSignal::new());
        let unit = FakeUnit::new(&["a", "bad", "c"], &["bad"]);

        let mut worker = worker(&idle, &shutdown);
        worker.run_sweep(&unit).await;

        assert_eq!(unit.imported(), ["a"]);
        assert_eq!(unit.swept(), ["a"]);
    }

    /// …but it must not block the queue forever either.
    #[tokio::test]
    async fn a_transcript_that_never_imports_is_given_up_on() {
        let idle = Arc::new(IdleTracker::new(None));
        let shutdown = Arc::new(ShutdownSignal::new());
        let unit = FakeUnit::new(&["bad", "c"], &["bad"]);

        let mut worker = worker(&idle, &shutdown);
        for _ in 0..MAX_ATTEMPTS {
            worker.run_sweep(&unit).await;
        }
        worker.run_sweep(&unit).await;

        assert_eq!(unit.imported(), ["c"]);
        assert!(unit.swept().contains(&"bad".to_string()));
    }

    #[tokio::test]
    async fn shutdown_ends_the_sweep_immediately() {
        let idle = Arc::new(IdleTracker::new(None));
        let shutdown = Arc::new(ShutdownSignal::new());
        shutdown.trigger();
        let unit = FakeUnit::new(&["a"], &[]);

        let mut worker = worker(&idle, &shutdown);
        assert_eq!(worker.run_sweep(&unit).await, SweepOutcome::Stopped);
        assert!(unit.imported().is_empty());
    }

    /// The whole point of the separation: capture never resets the clock the
    /// extraction worker schedules against.
    #[tokio::test]
    async fn a_sweep_does_not_disturb_the_quiet_clock() {
        let start = std::time::Instant::now();
        let idle = Arc::new(IdleTracker::new_at(None, start));
        let shutdown = Arc::new(ShutdownSignal::new());
        let unit = FakeUnit::new(&["a", "b"], &[]);

        let mut worker = worker(&idle, &shutdown);
        worker.run_sweep(&unit).await;

        // Quiet since `start`, still quiet now: nothing in the sweep touched it.
        assert!(idle.is_quiet_at(start + Duration::from_secs(120), Duration::from_secs(60)));
    }
}
