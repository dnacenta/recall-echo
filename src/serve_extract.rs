// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background entity extraction inside the graph daemon.
//!
//! recall-echo's claim is that the memory lifecycle is mechanical rather than
//! on the honor system. That was true of episodes — `SessionEnd` ingests them
//! through a hook — and false of everything built on top of them: entities,
//! relationships, confidence and provenance only appeared when a human
//! remembered to run `recall-echo graph extract`. This module closes that gap.
//!
//! The daemon is the natural home for the pass: it is already long-lived, it
//! already owns the embedded store, and it already knows when nobody is using
//! it. Once the machine has been quiet for `[extraction] idle_after_secs`, the
//! worker takes a small batch of un-extracted archives and runs the *same*
//! extraction the CLI runs — this module decides *when* extraction happens,
//! never *how*.
//!
//! Discipline:
//!
//! - **No lock is held across a batch.** The worker shares the daemon's
//!   `Arc<GraphMemory>` like any connection task; a request that arrives is
//!   served concurrently, and the batch stops at the next unit boundary.
//! - **Crash-only, per unit.** `extracted` flips one archive at a time, after
//!   that archive's extraction succeeded, so an interrupted batch leaves a
//!   store that simply has work left to do.
//! - **Shutdown wins.** Every wait and every unit runs under
//!   [`ShutdownSignal`], so an admin operation taking the store never waits for
//!   a batch to finish.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::config::ExtractionSection;
use crate::graph::error::GraphError;
use crate::graph::llm::LlmProvider;
use crate::graph::{GraphMemory, IngestContext};
use crate::serve::{BackgroundGuard, DaemonLog, ExtractionState, IdleTracker, ShutdownSignal};

/// Longest the worker sleeps between quiet checks.
const MAX_POLL: Duration = Duration::from_secs(30);
/// Shortest it sleeps, so a short `idle_after_secs` stays responsive without
/// spinning.
const MIN_POLL: Duration = Duration::from_millis(100);
/// Attempts one archive gets before it is quarantined. The CLI uses the same
/// rule: one retry, then set it aside rather than retry it forever.
const MAX_UNIT_ATTEMPTS: u32 = 2;
/// Consecutive failed units before the worker concludes the provider is wedged
/// and stops for the daemon's remaining life.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

// ── Clock ────────────────────────────────────────────────────────────────

/// Source of the current instant. Injectable so scheduling can be tested
/// without waiting for real time to pass.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

// ── The unit of work ─────────────────────────────────────────────────────

/// What one background batch did to one archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnitReport {
    pub entities: u32,
    pub relationships: u32,
}

/// One archive's worth of extraction, as the scheduler sees it.
///
/// Kept behind a trait so the worker's scheduling — when it waits, when it
/// yields, when it stops — can be exercised without a store or a model.
#[async_trait]
pub trait ExtractionUnit: Send + Sync {
    /// Log numbers still awaiting extraction, at most `limit` of them.
    async fn pending(&self, limit: usize) -> Result<Vec<u32>, GraphError>;

    /// Extract one archive and mark it extracted. Marking is the last thing it
    /// does, so a failure or an interruption leaves the archive pending.
    async fn extract(&self, log_number: u32) -> Result<UnitReport, GraphError>;

    /// Stop offering this archive: it has failed [`MAX_UNIT_ATTEMPTS`] times.
    async fn quarantine(&self, log_number: u32, reason: &str);
}

// ── Schedule ─────────────────────────────────────────────────────────────

/// When and how much the worker extracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Quiet period before a batch may start.
    pub idle_after: Duration,
    /// Archives per batch.
    pub batch_size: usize,
    /// How often the worker re-checks whether the daemon is quiet.
    pub poll_interval: Duration,
}

impl Schedule {
    /// The schedule described by an `[extraction]` section.
    #[must_use]
    pub fn from_config(config: &ExtractionSection) -> Self {
        let idle_after = config.idle_after();
        Self {
            idle_after,
            batch_size: config.effective_batch_size(),
            poll_interval: (idle_after / 4).clamp(MIN_POLL, MAX_POLL),
        }
    }
}

/// Whether background extraction should run at all in this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Run, on this schedule.
    Run(Schedule),
    /// Do not run, for this reason.
    Off(String),
}

/// Decide whether this daemon extracts in the background.
///
/// `graph_mode` is the `[graph] mode` of the memory directory. Server mode is
/// refused deliberately: there, clients talk to SurrealDB directly and never
/// start or consult a daemon, so a daemon's idea of "quiet" is not a fact
/// about the user's activity at all — it is a fact about a socket nobody is
/// connected to. A worker there would extract continuously, against a store
/// other processes are writing to, on behalf of a user who has no reason to
/// know the daemon exists. `graph extract` remains the way to extract in
/// server mode.
#[must_use]
pub fn plan(config: &ExtractionSection, graph_mode: &str) -> Plan {
    if !config.background_enabled {
        return Plan::Off("[extraction] background_enabled = false".into());
    }
    if graph_mode == "server" {
        return Plan::Off("[graph] mode = \"server\" — use `graph extract`".into());
    }
    Plan::Run(Schedule::from_config(config))
}

// ── Worker ───────────────────────────────────────────────────────────────

/// Everything the worker shares with the rest of the daemon.
pub struct WorkerContext {
    pub idle: Arc<IdleTracker>,
    pub shutdown: Arc<ShutdownSignal>,
    pub state: Arc<ExtractionState>,
    pub log: Arc<DaemonLog>,
    pub clock: Arc<dyn Clock>,
}

/// How a batch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchOutcome {
    /// Nothing to extract.
    NoWork,
    /// At least one archive was extracted.
    Worked,
    /// Shutdown arrived.
    Stopped,
    /// The provider has failed too many times in a row.
    Wedged,
}

/// The background extraction loop.
pub struct ExtractionWorker {
    schedule: Schedule,
    context: WorkerContext,
    /// Failed attempts per log number, within this daemon's life.
    attempts: HashMap<u32, u32>,
    /// Log numbers this daemon has given up on.
    skipped: HashSet<u32>,
    consecutive_failures: u32,
}

impl ExtractionWorker {
    #[must_use]
    pub fn new(schedule: Schedule, context: WorkerContext) -> Self {
        Self {
            schedule,
            context,
            attempts: HashMap::new(),
            skipped: HashSet::new(),
            consecutive_failures: 0,
        }
    }

    /// Extract in the background until the daemon shuts down.
    pub async fn run(mut self, unit: Arc<dyn ExtractionUnit>) {
        self.context.state.enable();
        loop {
            if self
                .context
                .shutdown
                .sleep_until_stopped(self.schedule.poll_interval)
                .await
            {
                break;
            }
            if !self.is_quiet() {
                continue;
            }
            match self.run_batch(unit.as_ref()).await {
                BatchOutcome::NoWork | BatchOutcome::Worked => {}
                BatchOutcome::Stopped => break,
                BatchOutcome::Wedged => {
                    let reason = format!(
                        "extraction failed {MAX_CONSECUTIVE_FAILURES} times in a row — \
                         not retrying until the daemon restarts"
                    );
                    self.context
                        .log
                        .log(&format!("background extraction off: {reason}"));
                    self.context.state.disable(reason);
                    return;
                }
            }
        }
        self.context.state.disable("daemon stopping");
    }

    fn is_quiet(&self) -> bool {
        self.context
            .idle
            .is_quiet_at(self.context.clock.now(), self.schedule.idle_after)
    }

    /// Extract up to `batch_size` archives, yielding between each.
    async fn run_batch(&mut self, unit: &dyn ExtractionUnit) -> BatchOutcome {
        let pending = match self.take_pending(unit).await {
            Some(pending) if !pending.is_empty() => pending,
            Some(_) => return BatchOutcome::NoWork,
            None => return BatchOutcome::Stopped,
        };

        // From here on the daemon may not idle-exit: the batch is in flight.
        let _busy = BackgroundGuard::new(Arc::clone(&self.context.idle));
        let started = self.context.clock.now();
        let mut extracted = 0u64;
        let mut outcome = BatchOutcome::Worked;

        for log_number in pending {
            if self.context.shutdown.is_triggered() {
                outcome = BatchOutcome::Stopped;
                break;
            }
            // A hot request beats background work, always. The rest of the
            // batch waits for the next quiet period.
            if self.context.idle.has_connections() {
                break;
            }

            match self.context.shutdown.guard(unit.extract(log_number)).await {
                None => {
                    outcome = BatchOutcome::Stopped;
                    break;
                }
                Some(Ok(report)) => {
                    extracted += 1;
                    self.consecutive_failures = 0;
                    self.attempts.remove(&log_number);
                    self.context.log.log(&format!(
                        "extracted log {log_number:03} in the background: \
                         +{} entities, {} relationships",
                        report.entities, report.relationships
                    ));
                }
                Some(Err(err)) => {
                    if self.record_failure(unit, log_number, &err).await {
                        outcome = BatchOutcome::Wedged;
                        break;
                    }
                }
            }

            // Hand the runtime back between units so a request that arrived
            // mid-batch is picked up before the next archive starts.
            tokio::task::yield_now().await;
        }

        self.finish_batch(extracted, started);
        outcome
    }

    /// Log numbers worth attempting, or `None` when shutdown arrived.
    async fn take_pending(&mut self, unit: &dyn ExtractionUnit) -> Option<Vec<u32>> {
        // Over-fetch, then drop what this daemon has given up on, so a
        // quarantined archive cannot occupy a batch slot forever.
        let limit = self.schedule.batch_size + self.skipped.len();
        match self.context.shutdown.guard(unit.pending(limit)).await? {
            Ok(pending) => Some(
                pending
                    .into_iter()
                    .filter(|log_number| !self.skipped.contains(log_number))
                    .take(self.schedule.batch_size)
                    .collect(),
            ),
            Err(err) => {
                self.context
                    .log
                    .log(&format!("background extraction: cannot list work: {err}"));
                self.context.state.record_error(err.to_string());
                Some(Vec::new())
            }
        }
    }

    /// Account for a failed archive. `true` means the provider looks wedged.
    async fn record_failure(
        &mut self,
        unit: &dyn ExtractionUnit,
        log_number: u32,
        err: &GraphError,
    ) -> bool {
        self.consecutive_failures += 1;
        let attempts = self.attempts.entry(log_number).or_insert(0);
        *attempts += 1;

        self.context.state.record_error(err.to_string());
        if *attempts >= MAX_UNIT_ATTEMPTS {
            self.skipped.insert(log_number);
            unit.quarantine(log_number, &err.to_string()).await;
            self.context.log.log(&format!(
                "background extraction quarantined log {log_number:03} after \
                 {MAX_UNIT_ATTEMPTS} attempts: {err}"
            ));
        } else {
            self.context.log.log(&format!(
                "background extraction failed on log {log_number:03}: {err}"
            ));
        }

        self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
    }

    /// Record what the batch did — and only then let the idle clock restart,
    /// so a daemon with a backlog stays alive and one without it does not.
    fn finish_batch(&self, extracted: u64, started: Instant) {
        if extracted == 0 {
            return;
        }
        let finished = self.context.clock.now();
        let elapsed = finished.saturating_duration_since(started);
        self.context
            .state
            .record_batch(extracted, elapsed, finished);
        self.context.idle.touch_at(Instant::now());
        self.context.log.log(&format!(
            "background extraction: {extracted} archives in {}ms",
            elapsed.as_millis()
        ));
    }
}

// ── The real unit: an archive on disk, a store, a provider ───────────────

/// Extraction against the daemon's own store, with a provider built from
/// config. Runs exactly what `recall-echo graph extract` runs.
pub struct GraphExtractionUnit {
    graph: Arc<GraphMemory>,
    llm: Box<dyn LlmProvider>,
    conversations_dir: PathBuf,
    quarantine_path: PathBuf,
}

impl GraphExtractionUnit {
    #[must_use]
    pub fn new(
        graph: Arc<GraphMemory>,
        llm: Box<dyn LlmProvider>,
        conversations_dir: PathBuf,
        quarantine_path: PathBuf,
    ) -> Self {
        Self {
            graph,
            llm,
            conversations_dir,
            quarantine_path,
        }
    }
}

#[async_trait]
impl ExtractionUnit for GraphExtractionUnit {
    async fn pending(&self, limit: usize) -> Result<Vec<u32>, GraphError> {
        let quarantined = read_quarantine(&self.quarantine_path);
        Ok(self
            .graph
            .unextracted_log_numbers()
            .await?
            .into_iter()
            .filter_map(|log_number| u32::try_from(log_number).ok())
            .filter(|log_number| !quarantined.contains(log_number))
            .take(limit)
            .collect())
    }

    async fn extract(&self, log_number: u32) -> Result<UnitReport, GraphError> {
        let path = crate::graph_cli::find_archive_file(&self.conversations_dir, log_number)
            .map_err(|err| GraphError::NotFound(err.to_string()))?;
        let content = std::fs::read_to_string(&path)?;
        let (session_id, _) = crate::graph_cli::extract_archive_metadata(&content, &path);
        let context = IngestContext::new(session_id, Some(log_number));

        let report = self
            .graph
            .extract_from_archive(&content, &context, self.llm.as_ref())
            .await?;
        self.graph.mark_extracted(log_number).await?;

        Ok(UnitReport {
            entities: report.entities_created + report.entities_merged,
            relationships: report.relationships_created,
        })
    }

    async fn quarantine(&self, log_number: u32, _reason: &str) {
        use std::io::Write as _;

        // Best effort. The worker's in-memory skip list already holds for this
        // daemon's life, so a failed write costs one retry after a restart.
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.quarantine_path)
            .and_then(|mut file| writeln!(file, "{log_number:03}"));
    }
}

/// Log numbers a previous run set aside. Absent or unreadable means none.
fn read_quarantine(path: &Path) -> HashSet<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

// ── Wiring ───────────────────────────────────────────────────────────────

/// Everything [`spawn`] needs from the daemon.
pub struct Setup {
    pub memory_dir: PathBuf,
    pub graph: Arc<GraphMemory>,
    pub idle: Arc<IdleTracker>,
    pub shutdown: Arc<ShutdownSignal>,
    pub state: Arc<ExtractionState>,
    pub log: Arc<DaemonLog>,
}

/// Start the background worker for a daemon, if it should have one.
///
/// Every refusal is quiet, final and stated once: a missing provider, a
/// missing key, a missing conversations directory and a config opt-out all
/// end here, with a line in the daemon log and a reason on the wire — never a
/// retry loop, and never surprise API spend.
pub fn spawn(setup: Setup) -> Option<tokio::task::JoinHandle<()>> {
    let config = crate::config::load_from_dir(&setup.memory_dir);
    let mode = crate::serve_client::graph_mode(&setup.memory_dir);

    let schedule = match plan(&config.extraction, &mode) {
        Plan::Run(schedule) => schedule,
        Plan::Off(reason) => return refuse(&setup, &reason),
    };

    let conversations_dir = match crate::graph_cli::find_conversations_dir(&setup.memory_dir) {
        Ok(dir) => dir,
        Err(err) => return refuse(&setup, &format!("no archives to extract ({err})")),
    };

    // The daemon is started with an allowlisted environment that deliberately
    // excludes API keys, so an API-key provider fails here — which is the
    // point: an auto-started daemon can only spend what a subscription already
    // covers. Running `serve --foreground` with a key exported is the explicit
    // way to opt an API provider in.
    let (llm, model) = match crate::llm_provider::create_provider(&setup.memory_dir, None, None) {
        Ok(provider) => provider,
        Err(err) => return refuse(&setup, &format!("no usable LLM provider ({err})")),
    };

    if let Some(timeout) = setup.idle.timeout() {
        if schedule.idle_after >= timeout {
            setup.log.log(&format!(
                "warning: [extraction] idle_after_secs ({}s) is not shorter than \
                 [serve] idle_timeout_secs ({}s) — the daemon exits before it extracts",
                schedule.idle_after.as_secs(),
                timeout.as_secs()
            ));
        }
    }

    setup.log.log(&format!(
        "background extraction on: {} provider, model {}, every {}s of quiet, {} archives per batch",
        config.llm.provider,
        if model.is_empty() { "default" } else { &model },
        schedule.idle_after.as_secs(),
        schedule.batch_size,
    ));

    let unit: Arc<dyn ExtractionUnit> = Arc::new(GraphExtractionUnit::new(
        Arc::clone(&setup.graph),
        llm,
        conversations_dir,
        setup
            .memory_dir
            .join("graph")
            .join("extraction-quarantine.txt"),
    ));
    let worker = ExtractionWorker::new(
        schedule,
        WorkerContext {
            idle: setup.idle,
            shutdown: setup.shutdown,
            state: setup.state,
            log: setup.log,
            clock: Arc::new(SystemClock),
        },
    );
    Some(tokio::spawn(worker.run(unit)))
}

/// Say once why no background extraction runs, and run none.
fn refuse(setup: &Setup, reason: &str) -> Option<tokio::task::JoinHandle<()>> {
    setup
        .log
        .log(&format!("background extraction off: {reason}"));
    setup.state.disable(reason);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(background_enabled: bool) -> ExtractionSection {
        ExtractionSection {
            background_enabled,
            ..ExtractionSection::default()
        }
    }

    #[test]
    fn the_default_plan_runs_on_the_configured_schedule() {
        let Plan::Run(schedule) = plan(&config(true), "embedded") else {
            panic!("the default config must run");
        };
        assert_eq!(schedule.idle_after, Duration::from_secs(120));
        assert_eq!(schedule.batch_size, 3);
        assert_eq!(schedule.poll_interval, Duration::from_secs(30));
    }

    #[test]
    fn opting_out_turns_the_worker_off() {
        let Plan::Off(reason) = plan(&config(false), "embedded") else {
            panic!("background_enabled = false must be honored");
        };
        assert!(reason.contains("background_enabled"), "{reason}");
    }

    #[test]
    fn server_mode_never_extracts_in_the_background() {
        let Plan::Off(reason) = plan(&config(true), "server") else {
            panic!("server mode has no daemon to schedule against");
        };
        assert!(reason.contains("server"), "{reason}");
    }

    #[test]
    fn poll_interval_is_bounded_at_both_ends() {
        let fast = Schedule::from_config(&ExtractionSection {
            idle_after_secs: 0,
            ..ExtractionSection::default()
        });
        assert_eq!(fast.poll_interval, MIN_POLL);

        let slow = Schedule::from_config(&ExtractionSection {
            idle_after_secs: 86_400,
            ..ExtractionSection::default()
        });
        assert_eq!(slow.poll_interval, MAX_POLL);
    }

    #[test]
    fn a_batch_of_zero_would_never_extract_so_it_is_one() {
        let schedule = Schedule::from_config(&ExtractionSection {
            batch_size: 0,
            ..ExtractionSection::default()
        });
        assert_eq!(schedule.batch_size, 1);
    }

    #[test]
    fn quarantined_log_numbers_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extraction-quarantine.txt");
        assert!(read_quarantine(&path).is_empty());

        std::fs::write(&path, "007\n12\nnot-a-number\n").unwrap();
        let quarantined = read_quarantine(&path);
        assert!(quarantined.contains(&7));
        assert!(quarantined.contains(&12));
        assert_eq!(quarantined.len(), 2);
    }
}
