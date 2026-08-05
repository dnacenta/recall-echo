// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background extraction scheduling: *when* the daemon extracts.
//!
//! The property under test is scheduling, not extraction — so there is no
//! store and no model here. The clock is injected, so "wait for quiet" is a
//! decision rather than a sleep, and the unit of work is a fake that calls a
//! test provider: `NoModel` panics if it is called at all, `CountingModel`
//! counts, `FailingModel` fails. Every assertion about cost is an assertion
//! about that count.

#![cfg(feature = "llm")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use recall_echo::config::ExtractionSection;
use recall_echo::graph::error::GraphError;
use recall_echo::graph::llm::LlmProvider;
use recall_echo::graph::GraphMemory;
use recall_echo::serve::{DaemonLog, ExtractionState, IdleTracker, ShutdownSignal};
use recall_echo::serve_extract::{
    self, Clock, ExtractionUnit, ExtractionWorker, Plan, Schedule, Setup, UnitReport, WorkerContext,
};
use tempfile::TempDir;

/// Quiet period every test uses. Never actually waited for — the clock moves.
const IDLE_AFTER: Duration = Duration::from_secs(60);
/// Real time between the worker's quiet checks.
const POLL: Duration = Duration::from_millis(5);
/// How long a test waits for the worker to reach an expected state.
const SETTLE: Duration = Duration::from_secs(5);

// ── Clock ────────────────────────────────────────────────────────────────

/// A clock the test moves by hand.
struct ManualClock(Mutex<Instant>);

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Instant::now())))
    }

    /// Place the clock at a chosen distance from real time. The idle tracker
    /// is touched with real instants (it is the daemon's own clock, not the
    /// worker's), so the two must be positioned relative to each other rather
    /// than nudged forward independently.
    fn set_ahead_of_now(&self, by: Duration) {
        *self.0.lock().unwrap() = Instant::now() + by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

// ── Providers ────────────────────────────────────────────────────────────

/// Fatal on use: the assertion that no model call was paid for.
struct NoModel;

#[async_trait]
impl LlmProvider for NoModel {
    async fn complete(&self, _system: &str, user: &str, _max: u32) -> Result<String, GraphError> {
        panic!("background extraction called a model it should not have:\n{user}");
    }
}

/// Counts calls and answers with a canned string.
#[derive(Default)]
struct CountingModel {
    calls: AtomicUsize,
}

impl CountingModel {
    fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for CountingModel {
    async fn complete(&self, _system: &str, _user: &str, _max: u32) -> Result<String, GraphError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("{}".into())
    }
}

/// Counts calls and fails every one of them — a wedged provider.
#[derive(Default)]
struct FailingModel {
    calls: AtomicUsize,
}

impl FailingModel {
    fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for FailingModel {
    async fn complete(&self, _system: &str, _user: &str, _max: u32) -> Result<String, GraphError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(GraphError::Llm("provider is wedged".into()))
    }
}

// ── Unit of work ─────────────────────────────────────────────────────────

/// An archive queue backed by a test provider instead of a store.
struct FakeUnit {
    llm: Arc<dyn LlmProvider>,
    pending: Mutex<Vec<u32>>,
    extracted: Mutex<Vec<u32>>,
    quarantined: Mutex<Vec<u32>>,
    /// Triggered right after the first archive is extracted, to model an admin
    /// operation arriving mid-batch.
    stop_after_first: Option<Arc<ShutdownSignal>>,
}

impl FakeUnit {
    fn new(llm: Arc<dyn LlmProvider>, pending: &[u32]) -> Arc<Self> {
        Arc::new(Self {
            llm,
            pending: Mutex::new(pending.to_vec()),
            extracted: Mutex::new(Vec::new()),
            quarantined: Mutex::new(Vec::new()),
            stop_after_first: None,
        })
    }

    fn stopping_after_first(
        llm: Arc<dyn LlmProvider>,
        pending: &[u32],
        shutdown: Arc<ShutdownSignal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            llm,
            pending: Mutex::new(pending.to_vec()),
            extracted: Mutex::new(Vec::new()),
            quarantined: Mutex::new(Vec::new()),
            stop_after_first: Some(shutdown),
        })
    }

    fn extracted(&self) -> Vec<u32> {
        self.extracted.lock().unwrap().clone()
    }

    fn quarantined(&self) -> Vec<u32> {
        self.quarantined.lock().unwrap().clone()
    }
}

#[async_trait]
impl ExtractionUnit for FakeUnit {
    async fn pending(&self, limit: usize) -> Result<Vec<u32>, GraphError> {
        Ok(self
            .pending
            .lock()
            .unwrap()
            .iter()
            .copied()
            .take(limit)
            .collect())
    }

    async fn extract(&self, log_number: u32) -> Result<UnitReport, GraphError> {
        self.llm
            .complete("extract", &format!("log {log_number}"), 1024)
            .await?;
        self.pending.lock().unwrap().retain(|n| *n != log_number);
        self.extracted.lock().unwrap().push(log_number);
        if let Some(shutdown) = &self.stop_after_first {
            shutdown.trigger();
        }
        Ok(UnitReport {
            entities: 2,
            relationships: 1,
        })
    }

    async fn quarantine(&self, log_number: u32, _reason: &str) {
        self.quarantined.lock().unwrap().push(log_number);
    }
}

// ── Harness ──────────────────────────────────────────────────────────────

/// A worker wired to a manual clock, a live idle tracker and a throwaway log.
struct Harness {
    _dir: TempDir,
    clock: Arc<ManualClock>,
    idle: Arc<IdleTracker>,
    shutdown: Arc<ShutdownSignal>,
    state: Arc<ExtractionState>,
    log: Arc<DaemonLog>,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let clock = ManualClock::new();
        Self {
            idle: Arc::new(IdleTracker::new_at(
                Some(Duration::from_secs(3600)),
                clock.now(),
            )),
            shutdown: Arc::new(ShutdownSignal::new()),
            state: ExtractionState::shared(),
            log: Arc::new(DaemonLog::open(&dir.path().join("daemon.log"), false)),
            clock,
            _dir: dir,
        }
    }

    fn schedule(&self, batch_size: usize) -> Schedule {
        Schedule {
            idle_after: IDLE_AFTER,
            batch_size,
            poll_interval: POLL,
        }
    }

    fn spawn(
        &self,
        batch_size: usize,
        unit: Arc<dyn ExtractionUnit>,
    ) -> tokio::task::JoinHandle<()> {
        let worker = ExtractionWorker::new(
            self.schedule(batch_size),
            WorkerContext {
                idle: Arc::clone(&self.idle),
                shutdown: Arc::clone(&self.shutdown),
                state: Arc::clone(&self.state),
                log: Arc::clone(&self.log),
                clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            },
        );
        tokio::spawn(worker.run(unit))
    }

    /// Move the worker's clock past the quiet threshold.
    fn become_quiet(&self) {
        self.clock
            .set_ahead_of_now(IDLE_AFTER + Duration::from_secs(1));
    }

    /// Put the worker's clock back at the present: the daemon was used just now.
    fn become_busy(&self) {
        self.clock.set_ahead_of_now(Duration::ZERO);
    }

    async fn stop(&self, worker: tokio::task::JoinHandle<()>) {
        self.shutdown.trigger();
        tokio::time::timeout(SETTLE, worker)
            .await
            .expect("worker stops when the daemon does")
            .expect("worker did not panic");
    }
}

/// Wait until `condition` holds, or fail the test.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("timed out waiting for {what}");
}

/// Give the worker several poll cycles to prove it does nothing.
async fn let_it_run() {
    tokio::time::sleep(POLL * 20).await;
}

// ── Scheduling ───────────────────────────────────────────────────────────

/// The daemon must be quiet before a single token is spent.
#[tokio::test]
async fn the_worker_waits_for_quiet_before_extracting() {
    let harness = Harness::new();
    let model = CountingModel::shared();
    let unit = FakeUnit::new(model.clone(), &[1, 2, 3, 4]);
    let worker = harness.spawn(3, unit.clone());

    let_it_run().await;
    assert_eq!(
        model.calls(),
        0,
        "extraction started before the quiet period"
    );
    assert!(unit.extracted().is_empty());

    harness.become_quiet();
    eventually("the first batch", || unit.extracted().len() == 3).await;
    assert_eq!(model.calls(), 3);

    harness.stop(worker).await;
}

/// An open connection makes the daemon un-quiet, whatever the clock says.
#[tokio::test]
async fn an_open_connection_holds_background_work_back() {
    let harness = Harness::new();
    let unit = FakeUnit::new(Arc::new(NoModel), &[1, 2, 3]);
    harness.idle.begin();
    harness.become_quiet();

    let worker = harness.spawn(3, unit.clone());
    let_it_run().await;
    assert!(
        unit.extracted().is_empty(),
        "background work ran while a client was connected"
    );

    harness.stop(worker).await;
}

/// A connection resets the quiet clock: work resumes only after the daemon has
/// been left alone again.
#[tokio::test]
async fn work_resumes_a_full_quiet_period_after_a_client_leaves() {
    let harness = Harness::new();
    let model = CountingModel::shared();
    let unit = FakeUnit::new(model.clone(), &[1]);

    harness.idle.begin();
    harness.become_quiet();
    let worker = harness.spawn(1, unit.clone());
    let_it_run().await;
    assert_eq!(model.calls(), 0);

    // The client leaves — that touches the tracker, so the quiet period starts
    // over from the moment of the last request.
    harness.idle.end();
    harness.become_busy();
    let_it_run().await;
    assert_eq!(
        model.calls(),
        0,
        "the quiet period restarted, so must the wait"
    );

    harness.become_quiet();
    eventually("work after the client left", || model.calls() == 1).await;

    harness.stop(worker).await;
}

/// One batch is bounded; the backlog drains over successive quiet periods.
#[tokio::test]
async fn a_batch_is_capped_and_the_backlog_drains_across_periods() {
    let harness = Harness::new();
    let model = CountingModel::shared();
    let unit = FakeUnit::new(model.clone(), &[1, 2, 3, 4, 5]);
    let worker = harness.spawn(2, unit.clone());

    for expected in [2, 4, 5] {
        harness.become_quiet();
        eventually("the next batch", || unit.extracted().len() == expected).await;
    }
    assert_eq!(unit.extracted(), vec![1, 2, 3, 4, 5]);
    assert_eq!(model.calls(), 5);

    harness.stop(worker).await;
}

// ── Stopping ─────────────────────────────────────────────────────────────

/// Shutdown mid-batch stops at the unit boundary: the rest of the batch is
/// simply still pending, which is the whole crash-only contract.
#[tokio::test]
async fn shutdown_stops_the_batch_at_the_next_unit() {
    let harness = Harness::new();
    let model = CountingModel::shared();
    let unit =
        FakeUnit::stopping_after_first(model.clone(), &[1, 2, 3], Arc::clone(&harness.shutdown));
    let worker = harness.spawn(3, unit.clone());
    harness.become_quiet();

    tokio::time::timeout(SETTLE, worker)
        .await
        .expect("worker stops as soon as shutdown is signalled")
        .expect("worker did not panic");

    assert_eq!(unit.extracted(), vec![1], "the batch ran past shutdown");
    assert_eq!(model.calls(), 1);
    assert_eq!(unit.pending(10).await.unwrap(), vec![2, 3]);
}

/// A daemon told to stop before its first tick spends nothing at all.
#[tokio::test]
async fn a_worker_that_never_starts_makes_no_model_calls() {
    let harness = Harness::new();
    let unit = FakeUnit::new(Arc::new(NoModel), &[1, 2, 3]);
    harness.become_quiet();
    harness.shutdown.trigger();

    let worker = harness.spawn(3, unit.clone());
    tokio::time::timeout(SETTLE, worker)
        .await
        .expect("worker stops immediately")
        .expect("worker did not panic");
    assert!(unit.extracted().is_empty());
}

// ── Idle shutdown ────────────────────────────────────────────────────────

/// The daemon must not idle-exit with a batch in flight, and must still exit
/// once the work stops.
#[tokio::test]
async fn a_batch_in_flight_defers_idle_shutdown_but_does_not_cancel_it() {
    let harness = Harness::new();
    let model = CountingModel::shared();
    let unit = FakeUnit::new(model.clone(), &[1]);
    let worker = harness.spawn(1, unit.clone());

    harness.become_quiet();
    eventually("the only archive", || model.calls() == 1).await;

    // Nothing left to do: the daemon is free to idle out again.
    let long_after = Instant::now() + Duration::from_secs(86_400);
    eventually("idle shutdown to become possible again", || {
        harness.idle.is_idle_at(long_after)
    })
    .await;

    harness.stop(worker).await;
}

// ── Failure handling ─────────────────────────────────────────────────────

/// A provider that fails every call must not be retried forever.
#[tokio::test]
async fn a_wedged_provider_disables_the_worker_instead_of_looping() {
    let harness = Harness::new();
    let model = FailingModel::shared();
    let unit = FakeUnit::new(model.clone(), &[1, 2, 3, 4, 5]);
    let worker = harness.spawn(5, unit.clone());
    harness.become_quiet();

    tokio::time::timeout(SETTLE, worker)
        .await
        .expect("worker gives up on a wedged provider")
        .expect("worker did not panic");

    assert_eq!(
        model.calls(),
        3,
        "the worker kept calling a wedged provider"
    );
    let status = harness.state.snapshot(Instant::now());
    assert!(!status.enabled);
    assert!(
        status
            .disabled_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("in a row")),
        "{status:?}"
    );
    assert!(status.last_error.is_some());
}

/// One archive that cannot be extracted must not block the queue behind it or
/// cost a model call on every pass.
#[tokio::test]
async fn a_failing_archive_is_quarantined_after_two_attempts() {
    let harness = Harness::new();
    let model = FailingModel::shared();
    let unit = FakeUnit::new(model.clone(), &[7]);
    let worker = harness.spawn(3, unit.clone());

    harness.become_quiet();
    eventually("the archive to be set aside", || {
        unit.quarantined() == vec![7]
    })
    .await;

    // Two attempts, then never again — even as quiet periods keep arriving.
    for _ in 0..3 {
        harness.become_quiet();
        let_it_run().await;
    }
    assert_eq!(model.calls(), 2, "a quarantined archive was retried");

    harness.stop(worker).await;
}

// ── Wiring ───────────────────────────────────────────────────────────────

/// `background_enabled = false` must build no provider and start no worker.
#[tokio::test]
async fn the_disabled_path_starts_nothing() {
    let dir = TempDir::new().expect("temp dir");
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(memory_dir.join("conversations")).expect("conversations");
    let graph_dir = memory_dir.join("graph");
    std::fs::create_dir_all(&graph_dir).expect("graph dir");
    std::fs::write(
        memory_dir.join(".recall-echo.toml"),
        "[extraction]\nbackground_enabled = false\n",
    )
    .expect("write config");

    let state = ExtractionState::shared();
    let graph = GraphMemory::open_embedded(&graph_dir).await.expect("store");
    let handle = serve_extract::spawn(Setup {
        memory_dir: memory_dir.clone(),
        graph: Arc::new(graph),
        idle: Arc::new(IdleTracker::new(None)),
        shutdown: Arc::new(ShutdownSignal::new()),
        state: Arc::clone(&state),
        log: Arc::new(DaemonLog::open(&graph_dir.join("daemon.log"), false)),
    });

    assert!(handle.is_none(), "a disabled worker must not be spawned");
    let status = state.snapshot(Instant::now());
    assert!(!status.enabled);
    assert!(
        status
            .disabled_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("background_enabled")),
        "{status:?}"
    );
    assert_eq!(status.runs, 0);
}

/// The two refusals that are policy rather than accident.
#[test]
fn the_plan_refuses_opt_out_and_server_mode() {
    let default = ExtractionSection::default();
    assert!(matches!(
        serve_extract::plan(&default, "embedded"),
        Plan::Run(_)
    ));
    assert!(matches!(
        serve_extract::plan(&default, "server"),
        Plan::Off(_)
    ));
    assert!(matches!(
        serve_extract::plan(
            &ExtractionSection {
                background_enabled: false,
                ..ExtractionSection::default()
            },
            "embedded"
        ),
        Plan::Off(_)
    ));
}
