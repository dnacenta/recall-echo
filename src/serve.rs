// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `recall-echo serve` — the graph daemon.
//!
//! One daemon per memory directory owns the embedded graph store and answers
//! command-level requests over a unix socket, one JSON object per line:
//!
//! ```text
//! → {"op":"search","args":{"query":"rust","limit":5}}
//! ← {"ok":true,"data":[ ... ]}
//! ```
//!
//! The daemon is *crash-only*: it keeps no state outside the database, so it
//! can be killed at any instant. Clients ([`crate::serve_client`]) detect the
//! dead socket, clean it up and start a fresh daemon.
//!
//! Unix only — the socket is a plain `UnixListener`, with no transport
//! abstraction (see RE-29 decisions log).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::error::RecallError;
use crate::graph::error::GraphError;
use crate::graph::types::{
    EntityType, NewEntity, NewRelationship, PipelineDocuments, QueryOptions, SearchOptions,
};
use crate::graph::utility::OutcomeKind;
use crate::graph::{GraphMemory, IngestContext, Provenance};
use crate::serve_security::{
    append_private_file, check_peer_uid, current_uid, unlink_socket, PRIVATE_FILE_MODE,
};

/// Longest idle-poll interval; keeps a long-lived daemon from spinning.
const MAX_IDLE_POLL: Duration = Duration::from_secs(30);
/// Shortest idle-poll interval; keeps short test timeouts responsive.
const MIN_IDLE_POLL: Duration = Duration::from_millis(100);

/// Largest request line the daemon will read. An archive ingest is the biggest
/// legitimate request by far and stays far below this; anything larger is a
/// buggy or hostile client trying to make the daemon buffer without bound.
const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
/// Largest result set a request may ask for. Wire-supplied limits reach the
/// HNSW KNN operator, where an unbounded value is an unbounded scan.
const MAX_LIMIT: usize = 1000;
/// Deepest graph expansion a request may ask for. Expansion is exponential in
/// the branching factor.
const MAX_DEPTH: u32 = 8;
/// How long the daemon waits for its own store to close before giving up and
/// unlinking the socket anyway.
const STORE_RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
/// Polling interval while waiting for connection tasks to release the store.
const STORE_RELEASE_POLL: Duration = Duration::from_millis(10);
/// How long the daemon waits for the background extraction worker to stop
/// before abandoning it and closing up anyway.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(5);

// ── Protocol ─────────────────────────────────────────────────────────────

/// A client request. Wire form is `{"op": "...", "args": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Request {
    /// Version handshake — returns [`DaemonInfo`].
    Hello,
    /// Graph counts.
    Status,
    /// Semantic entity search.
    Search(SearchArgs),
    /// Semantic episode search.
    SearchEpisodes(SearchEpisodesArgs),
    /// Hybrid query: semantic + graph expansion + optional episodes.
    Query(QueryArgs),
    /// Traverse relationships from a named entity.
    Traverse(TraverseArgs),
    /// Create an entity.
    AddEntity(AddEntityArgs),
    /// Create a relationship between two named entities.
    Relate(RelateArgs),
    /// Ingest a conversation archive (episodes only, no LLM extraction).
    IngestArchive(IngestArchiveArgs),
    /// Sync the pipeline documents into the graph (no LLM extraction).
    SyncPipeline(SyncPipelineArgs),
    /// Apply an outcome to the entities a session touched.
    Feedback(FeedbackArgs),
    /// Ask the daemon to exit.
    Shutdown,
}

impl Request {
    /// Short name of the operation, for logs.
    #[must_use]
    pub fn op_name(&self) -> &'static str {
        match self {
            Request::Hello => "hello",
            Request::Status => "status",
            Request::Search(_) => "search",
            Request::SearchEpisodes(_) => "search_episodes",
            Request::Query(_) => "query",
            Request::Traverse(_) => "traverse",
            Request::AddEntity(_) => "add_entity",
            Request::Relate(_) => "relate",
            Request::IngestArchive(_) => "ingest_archive",
            Request::SyncPipeline(_) => "sync_pipeline",
            Request::Feedback(_) => "feedback",
            Request::Shutdown => "shutdown",
        }
    }

    /// Whether repeating this request against a fresh daemon is safe.
    ///
    /// A connection that drops mid-request cannot tell us whether the daemon
    /// applied it before dying, so only read-only or idempotent operations may
    /// be retried. Repeating an archive ingest would duplicate its episodes —
    /// a silently corrupted memory is worse than a reported failure.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Request::Hello
            | Request::Status
            | Request::Search(_)
            | Request::SearchEpisodes(_)
            | Request::Query(_)
            | Request::Traverse(_)
            // Pipeline sync diffs documents against the graph.
            | Request::SyncPipeline(_)
            // Outcome records replace per (entity, session) — reruns correct.
            | Request::Feedback(_)
            | Request::Shutdown => true,
            Request::AddEntity(_) | Request::Relate(_) | Request::IngestArchive(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchArgs {
    pub query: String,
    pub limit: usize,
    #[serde(default)]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchEpisodesArgs {
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryArgs {
    pub query: String,
    pub limit: usize,
    #[serde(default)]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    pub depth: u32,
    pub episodes: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraverseArgs {
    pub entity: String,
    pub depth: u32,
    #[serde(default)]
    pub type_filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddEntityArgs {
    pub name: String,
    pub entity_type: String,
    pub abstract_text: String,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelateArgs {
    pub from: String,
    pub rel_type: String,
    pub to: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestArchiveArgs {
    pub content: String,
    pub session_id: String,
    #[serde(default)]
    pub log_number: Option<u32>,
    /// Force one provenance class on every episode of this run. Absent — the
    /// shape older clients send — means infer per chunk from turn roles.
    #[serde(default)]
    pub provenance: Option<Provenance>,
}

/// Pipeline sync needs no LLM provider, so it runs against the daemon like any
/// other graph operation instead of taking the store exclusively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncPipelineArgs {
    pub docs: PipelineDocuments,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackArgs {
    pub session_id: String,
    pub outcome: OutcomeKind,
}

/// A daemon response. Wire form is `{"ok": true, "data": ...}` or
/// `{"ok": false, "error": {"code": "...", "message": "..."}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// A named failure. `code` is stable and machine-readable; `message` is the
/// human-readable text (already prefixed by the error kind, e.g. `store locked`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

impl Response {
    /// A successful response carrying `data`.
    #[must_use]
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// A failed response with a stable `code` and human-readable `message`.
    #[must_use]
    pub fn failure(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(ResponseError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    /// Convert a graph error into a coded failure response.
    #[must_use]
    pub fn from_graph_error(err: &GraphError) -> Self {
        Self::failure(error_code(err), err.to_string())
    }

    /// Unwrap into the client-side result: data on success, a named
    /// [`RecallError::Remote`] on failure.
    pub fn into_result(self) -> Result<serde_json::Value, RecallError> {
        if self.ok {
            return Ok(self.data.unwrap_or(serde_json::Value::Null));
        }
        let error = self.error.unwrap_or(ResponseError {
            code: "unknown".into(),
            message: "daemon reported failure without a message".into(),
        });
        Err(RecallError::Remote {
            code: error.code,
            message: error.message,
        })
    }
}

/// Stable machine-readable code for a graph error.
fn error_code(err: &GraphError) -> &'static str {
    match err {
        GraphError::Db(_) => "db",
        GraphError::Locked(_) => "locked",
        GraphError::Embed(_) => "embedding",
        GraphError::NotFound(_) => "not_found",
        GraphError::Extraction(_) => "extraction",
        GraphError::Dedup(_) => "dedup",
        GraphError::Llm(_) => "llm",
        GraphError::Parse(_) => "parse",
        GraphError::Io(_) => "io",
        GraphError::Json(_) => "json",
        GraphError::ImmutableMerge(_) => "immutable_merge",
    }
}

/// Identity of a running daemon, returned by [`Request::Hello`].
///
/// `extraction` is additive: a client talking to a daemon that predates
/// background extraction simply sees the default (disabled, nothing done).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    pub memory_dir: String,
    pub socket_path: String,
    pub uptime_secs: u64,
    #[serde(default)]
    pub extraction: ExtractionStatus,
}

/// What this daemon's background extraction worker has done so far.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractionStatus {
    /// Whether background extraction is running in this daemon.
    pub enabled: bool,
    /// Why it is not running, when it is not.
    pub disabled_reason: Option<String>,
    /// Batches that extracted at least one archive.
    pub runs: u64,
    /// Archives extracted since the daemon started.
    pub archives: u64,
    /// Seconds since the last batch finished.
    pub last_run_secs_ago: Option<u64>,
    /// How long the last batch took.
    pub last_run_ms: Option<u64>,
    /// The most recent extraction failure, if any.
    pub last_error: Option<String>,
}

// ── Dispatch ─────────────────────────────────────────────────────────────

/// Execute a graph operation against an open store.
///
/// Control operations ([`Request::Hello`], [`Request::Shutdown`]) are owned by
/// the connection loop and reported as `unsupported` here.
pub async fn dispatch_graph(graph: &GraphMemory, request: &Request) -> Response {
    match execute_graph(graph, request).await {
        Ok(Some(data)) => Response::success(data),
        Ok(None) => Response::failure(
            "unsupported",
            format!(
                "`{}` is a control operation, not a graph operation",
                request.op_name()
            ),
        ),
        Err(err) => Response::from_graph_error(&err),
    }
}

/// Clamp a wire-supplied result limit into a range the store can serve.
fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

/// Clamp a wire-supplied expansion depth.
fn clamp_depth(depth: u32) -> u32 {
    depth.clamp(1, MAX_DEPTH)
}

/// `Ok(None)` means "not a graph operation".
async fn execute_graph(
    graph: &GraphMemory,
    request: &Request,
) -> Result<Option<serde_json::Value>, GraphError> {
    let data = match request {
        Request::Hello | Request::Shutdown => return Ok(None),
        Request::Status => serde_json::to_value(graph.stats().await?)?,
        Request::Search(args) => {
            let options = SearchOptions {
                limit: clamp_limit(args.limit),
                entity_type: args.entity_type.clone(),
                keyword: args.keyword.clone(),
            };
            serde_json::to_value(graph.search_with_options(&args.query, &options).await?)?
        }
        Request::SearchEpisodes(args) => {
            let mut episodes = graph
                .search_episodes(&args.query, clamp_limit(args.limit))
                .await?;
            for result in &mut episodes {
                result.episode.embedding = None;
            }
            serde_json::to_value(episodes)?
        }
        Request::Query(args) => {
            let options = QueryOptions {
                limit: clamp_limit(args.limit),
                entity_type: args.entity_type.clone(),
                keyword: args.keyword.clone(),
                graph_depth: clamp_depth(args.depth),
                include_episodes: args.episodes,
            };
            let mut result = graph.query(&args.query, &options).await?;
            for episode in &mut result.episodes {
                episode.episode.embedding = None;
            }
            serde_json::to_value(result)?
        }
        Request::Traverse(args) => serde_json::to_value(
            graph
                .traverse_filtered(
                    &args.entity,
                    clamp_depth(args.depth),
                    args.type_filter.as_deref(),
                )
                .await?,
        )?,
        Request::AddEntity(args) => {
            let entity_type: EntityType = args
                .entity_type
                .parse()
                .map_err(|e: String| GraphError::Parse(e))?;
            let mut entity = graph
                .add_entity(NewEntity {
                    name: args.name.clone(),
                    entity_type,
                    abstract_text: args.abstract_text.clone(),
                    overview: args.overview.clone(),
                    content: None,
                    attributes: None,
                    source: args.source.clone(),
                })
                .await?;
            // 384 floats of JSON text no client has ever read.
            entity.embedding = None;
            serde_json::to_value(entity)?
        }
        Request::Relate(args) => {
            let relationship = graph
                .add_relationship(NewRelationship {
                    from_entity: args.from.clone(),
                    to_entity: args.to.clone(),
                    rel_type: args.rel_type.clone(),
                    description: args.description.clone(),
                    confidence: None,
                    source: args.source.clone(),
                })
                .await?;
            serde_json::to_value(relationship)?
        }
        Request::IngestArchive(args) => {
            let context = IngestContext::new(args.session_id.clone(), args.log_number)
                .with_override(args.provenance);
            let report = graph.ingest_archive(&args.content, &context, None).await?;
            serde_json::to_value(report)?
        }
        Request::SyncPipeline(args) => {
            serde_json::to_value(graph.sync_pipeline(&args.docs).await?)?
        }
        Request::Feedback(args) => serde_json::to_value(
            graph
                .record_session_outcome(&args.session_id, args.outcome)
                .await?,
        )?,
    };
    Ok(Some(data))
}

// ── Idle tracking ────────────────────────────────────────────────────────

/// Tracks daemon activity so an unused daemon shuts itself down, and so
/// background work only runs while nobody is asking for anything.
///
/// Two different questions are asked of the same clock:
///
/// - *quiet* — no connection is open and the last request is older than some
///   period. Background extraction waits for this.
/// - *idle* — quiet for the configured shutdown timeout, **and** no background
///   batch in flight. The accept loop exits on this.
///
/// A background batch therefore cannot be cut in half by the idle timeout, and
/// a hot request always makes the daemon un-quiet immediately. `None` disables
/// idle shutdown entirely (`--foreground`, or `idle_timeout_secs = 0`) without
/// disabling quiet, which is what keeps a supervised daemon extracting.
#[derive(Debug)]
pub struct IdleTracker {
    timeout: Option<Duration>,
    active: AtomicUsize,
    background: AtomicUsize,
    last_activity: Mutex<Instant>,
}

impl IdleTracker {
    #[must_use]
    pub fn new(timeout: Option<Duration>) -> Self {
        Self::new_at(timeout, Instant::now())
    }

    /// Construct with an explicit start instant (used by tests).
    #[must_use]
    pub fn new_at(timeout: Option<Duration>, start: Instant) -> Self {
        Self {
            timeout,
            active: AtomicUsize::new(0),
            background: AtomicUsize::new(0),
            last_activity: Mutex::new(start),
        }
    }

    /// Register a connection as open.
    pub fn begin(&self) {
        self.active.fetch_add(1, Ordering::SeqCst);
        self.touch_at(Instant::now());
    }

    /// Register a connection as closed.
    pub fn end(&self) {
        let previous = self.active.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "IdleTracker::end without begin");
        self.touch_at(Instant::now());
    }

    /// Register a background batch as running. Holds off idle shutdown until
    /// it finishes, without pretending the daemon is being used.
    pub fn begin_background(&self) {
        self.background.fetch_add(1, Ordering::SeqCst);
    }

    /// Register a background batch as finished.
    pub fn end_background(&self) {
        let previous = self.background.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "IdleTracker::end_background without begin");
    }

    /// Record activity at `now`.
    pub fn touch_at(&self, now: Instant) {
        let mut last = self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *last = now;
    }

    /// True while at least one client connection is open.
    #[must_use]
    pub fn has_connections(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }

    /// True when no connection is open and nothing has been asked of the
    /// daemon for `quiet`.
    #[must_use]
    pub fn is_quiet_at(&self, now: Instant, quiet: Duration) -> bool {
        if self.has_connections() {
            return false;
        }
        let last = *self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        now.saturating_duration_since(last) >= quiet
    }

    /// True when the daemon has been unused for longer than the timeout and no
    /// background batch is in flight.
    #[must_use]
    pub fn is_idle_at(&self, now: Instant) -> bool {
        let Some(timeout) = self.timeout else {
            return false;
        };
        if self.background.load(Ordering::SeqCst) > 0 {
            return false;
        }
        self.is_quiet_at(now, timeout)
    }

    /// The configured idle shutdown timeout, if any.
    #[must_use]
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// How often the accept loop should re-check idleness.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        match self.timeout {
            None => MAX_IDLE_POLL,
            Some(timeout) => (timeout / 10).clamp(MIN_IDLE_POLL, MAX_IDLE_POLL),
        }
    }
}

/// RAII connection counter for [`IdleTracker`].
struct ActivityGuard(Arc<DaemonContext>);

impl ActivityGuard {
    fn new(context: Arc<DaemonContext>) -> Self {
        context.idle.begin();
        Self(context)
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.idle.end();
    }
}

/// RAII background-batch counter for [`IdleTracker`].
///
/// Held for exactly one batch, so an interrupted daemon waits for the unit in
/// flight rather than for the whole backlog.
pub struct BackgroundGuard(Arc<IdleTracker>);

impl BackgroundGuard {
    #[must_use]
    pub fn new(idle: Arc<IdleTracker>) -> Self {
        idle.begin_background();
        Self(idle)
    }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        self.0.end_background();
    }
}

// ── Shutdown ─────────────────────────────────────────────────────────────

/// One-way "stop now" signal, observable by any number of tasks.
///
/// A latched flag rather than a bare [`Notify`]: a task that checks after the
/// signal fired must still see it, or a worker between two units would sleep
/// through the daemon's exit and hold the store open.
#[derive(Debug, Default)]
pub struct ShutdownSignal {
    triggered: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl ShutdownSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask everything watching to stop. Idempotent.
    pub fn trigger(&self) {
        self.triggered.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }

    /// Resolve when shutdown is requested — immediately if it already was.
    pub async fn wait(&self) {
        loop {
            // Register before reading the flag: the reverse order loses a
            // `trigger` that lands in between, and the waiter never wakes.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_triggered() {
                return;
            }
            notified.await;
        }
    }

    /// Run `future` unless shutdown arrives first. `None` means it did.
    pub async fn guard<F: std::future::Future>(&self, future: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            () = self.wait() => None,
            output = future => Some(output),
        }
    }

    /// Sleep for `duration`. `true` means shutdown cut it short.
    pub async fn sleep_until_stopped(&self, duration: Duration) -> bool {
        self.guard(tokio::time::sleep(duration)).await.is_none()
    }
}

// ── Background extraction state ──────────────────────────────────────────

/// What the background extraction worker has done, shared with the connection
/// tasks that answer [`Request::Hello`].
#[derive(Debug, Default)]
pub struct ExtractionState {
    inner: Mutex<ExtractionProgress>,
}

#[derive(Debug, Default)]
struct ExtractionProgress {
    enabled: bool,
    disabled_reason: Option<String>,
    runs: u64,
    archives: u64,
    last_run: Option<Instant>,
    last_run_ms: Option<u64>,
    last_error: Option<String>,
}

impl ExtractionState {
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with<T>(&self, apply: impl FnOnce(&mut ExtractionProgress) -> T) -> T {
        let mut progress = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        apply(&mut progress)
    }

    /// Mark the worker as running.
    pub fn enable(&self) {
        self.with(|progress| {
            progress.enabled = true;
            progress.disabled_reason = None;
        });
    }

    /// Mark the worker as not running, and why.
    pub fn disable(&self, reason: impl Into<String>) {
        self.with(|progress| {
            progress.enabled = false;
            progress.disabled_reason = Some(reason.into());
        });
    }

    /// Record a batch that extracted `archives` archives in `elapsed`.
    pub fn record_batch(&self, archives: u64, elapsed: Duration, finished_at: Instant) {
        self.with(|progress| {
            progress.runs += 1;
            progress.archives += archives;
            progress.last_run = Some(finished_at);
            progress.last_run_ms = Some(elapsed.as_millis() as u64);
        });
    }

    /// Record the most recent failure, replacing any earlier one.
    pub fn record_error(&self, error: impl Into<String>) {
        self.with(|progress| progress.last_error = Some(error.into()));
    }

    /// Wire-facing snapshot as of `now`.
    #[must_use]
    pub fn snapshot(&self, now: Instant) -> ExtractionStatus {
        self.with(|progress| ExtractionStatus {
            enabled: progress.enabled,
            disabled_reason: progress.disabled_reason.clone(),
            runs: progress.runs,
            archives: progress.archives,
            last_run_secs_ago: progress
                .last_run
                .map(|at| now.saturating_duration_since(at).as_secs()),
            last_run_ms: progress.last_run_ms,
            last_error: progress.last_error.clone(),
        })
    }
}

// ── Logging ──────────────────────────────────────────────────────────────

/// Append-only daemon log at `<memory_dir>/graph/daemon.log`.
///
/// Never fails a request: if the file cannot be opened, lines go to stderr.
pub struct DaemonLog {
    file: Mutex<Option<std::fs::File>>,
    echo_stderr: bool,
}

impl DaemonLog {
    #[must_use]
    pub fn open(path: &Path, echo_stderr: bool) -> Self {
        let file = append_private_file().open(path).ok();
        Self {
            file: Mutex::new(file),
            echo_stderr,
        }
    }

    pub fn log(&self, message: &str) {
        use std::io::Write as _;
        let line = format!(
            "[{}] pid={} {message}\n",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
            std::process::id(),
        );
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_mut() {
            Some(file) => {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
                if self.echo_stderr {
                    eprint!("{line}");
                }
            }
            None => eprint!("{line}"),
        }
    }
}

// ── Daemon ───────────────────────────────────────────────────────────────

/// Everything `serve` needs to run.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The memory directory whose `graph/` store this daemon owns.
    pub memory_dir: PathBuf,
    /// Unix socket to listen on.
    pub socket_path: PathBuf,
    /// Idle shutdown timeout; `None` never shuts down.
    pub idle_timeout: Option<Duration>,
    /// Mirror the daemon log to stderr (foreground / systemd mode).
    pub log_to_stderr: bool,
}

impl ServeOptions {
    /// Build options from `[serve]` in the memory directory's config.
    ///
    /// `foreground` (systemd) disables idle shutdown and mirrors the log to
    /// stderr, leaving daemon lifetime to the supervisor.
    pub fn from_config(memory_dir: &Path, foreground: bool) -> Result<Self, RecallError> {
        let config = crate::config::load_from_dir(memory_dir);
        let idle_timeout = match (foreground, config.serve.idle_timeout_secs) {
            (true, _) | (_, 0) => None,
            (false, secs) => Some(Duration::from_secs(secs)),
        };
        Ok(Self {
            memory_dir: memory_dir.to_path_buf(),
            socket_path: crate::serve_client::socket_path(memory_dir)?,
            idle_timeout,
            log_to_stderr: foreground,
        })
    }
}

/// Shared, immutable-ish daemon state.
struct DaemonContext {
    started: Instant,
    memory_dir: PathBuf,
    socket_path: PathBuf,
    /// Only this uid may use the socket.
    owner_uid: u32,
    idle: Arc<IdleTracker>,
    shutdown: Arc<ShutdownSignal>,
    extraction: Arc<ExtractionState>,
}

impl DaemonContext {
    fn info(&self) -> DaemonInfo {
        DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            memory_dir: self.memory_dir.display().to_string(),
            socket_path: self.socket_path.display().to_string(),
            uptime_secs: self.started.elapsed().as_secs(),
            extraction: self.extraction.snapshot(Instant::now()),
        }
    }
}

/// Path of the pidfile that accompanies a socket.
#[must_use]
pub fn pidfile_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_os_string();
    path.push(".pid");
    PathBuf::from(path)
}

/// Run the daemon until it is asked to stop or goes idle.
pub async fn run(options: ServeOptions) -> Result<(), RecallError> {
    let graph_dir = options.memory_dir.join("graph");
    std::fs::create_dir_all(&graph_dir)?;

    let log = Arc::new(DaemonLog::open(
        &graph_dir.join("daemon.log"),
        options.log_to_stderr,
    ));
    log.log(&format!(
        "starting v{} for {} on {}",
        env!("CARGO_PKG_VERSION"),
        options.memory_dir.display(),
        options.socket_path.display()
    ));

    if crate::serve_client::graph_mode(&options.memory_dir) == "server" {
        log.log("warning: [graph] mode = \"server\" — clients bypass the daemon");
    }

    let owner_uid = current_uid()?;

    // Own the store before advertising the socket: clients that connect only
    // after a successful bind never see a half-initialized daemon.
    let graph = open_store(&graph_dir, &options.socket_path, &log).await?;

    let listener = match bind_socket(&options.socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            log.log(&format!("failed to bind socket: {err}"));
            return Err(err);
        }
    };
    write_pidfile(&options.socket_path)?;

    let extraction = ExtractionState::shared();
    let context = Arc::new(DaemonContext {
        started: Instant::now(),
        memory_dir: options.memory_dir.clone(),
        socket_path: options.socket_path.clone(),
        owner_uid,
        idle: Arc::new(IdleTracker::new(options.idle_timeout)),
        shutdown: Arc::new(ShutdownSignal::new()),
        extraction: Arc::clone(&extraction),
    });
    warm_embedder(Arc::clone(&graph), Arc::clone(&log));
    let worker = start_background_extraction(&options, &graph, &context, &log);
    let capture = start_background_capture(&options, &graph, &context, &log);
    log.log("ready");

    accept_loop(
        listener,
        Arc::clone(&graph),
        Arc::clone(&context),
        Arc::clone(&log),
    )
    .await;

    // Whatever ended the accept loop — a shutdown request, an idle timeout —
    // ends the background worker too, and it must let go of the store before
    // we try to close it.
    context.shutdown.trigger();
    stop_background_extraction(worker, &log).await;
    stop_background_capture(capture, &log).await;

    // Close the store *before* the socket disappears: a client waiting for the
    // socket to go treats that as "the store is free", and would otherwise
    // race the SurrealKV file lock we have not released yet.
    release_store(graph, &log).await;
    if let Err(err) = unlink_socket(&options.socket_path) {
        log.log(&format!("socket cleanup: {err}"));
    }
    let _ = std::fs::remove_file(pidfile_path(&options.socket_path));
    log.log("stopped");
    Ok(())
}

/// Open the store, waiting out an admin operation that currently owns it.
async fn open_store(
    graph_dir: &Path,
    socket_path: &Path,
    log: &DaemonLog,
) -> Result<Arc<GraphMemory>, RecallError> {
    let deadline = Instant::now() + crate::serve_client::ADMIN_WAIT_TIMEOUT;
    loop {
        crate::serve_client::wait_for_admin_lock(socket_path, deadline).await?;
        match GraphMemory::open_embedded(graph_dir).await {
            Ok(graph) => return Ok(Arc::new(graph)),
            Err(GraphError::Locked(message))
                if Instant::now() < deadline
                    && crate::serve_client::admin_lock_is_held(socket_path) =>
            {
                log.log(&format!("waiting for an admin operation: {message}"));
            }
            Err(err) => {
                log.log(&format!("failed to open graph store: {err}"));
                return Err(err.into());
            }
        }
    }
}

/// Drop the store once every in-flight connection has let go of it.
async fn release_store(graph: Arc<GraphMemory>, log: &DaemonLog) {
    let deadline = Instant::now() + STORE_RELEASE_TIMEOUT;
    let mut graph = graph;
    loop {
        match Arc::try_unwrap(graph) {
            Ok(store) => {
                drop(store);
                return;
            }
            Err(shared) => {
                if Instant::now() >= deadline {
                    log.log("gave up waiting for in-flight requests to release the store");
                    return;
                }
                graph = shared;
                tokio::time::sleep(STORE_RELEASE_POLL).await;
            }
        }
    }
}

/// Load the ONNX embedding model in the background, so the first request that
/// needs an embedding does not pay for it inline.
///
/// Skipped while the model cache is empty: warming a cold cache downloads the
/// model, which must stay tied to a request that actually needs it rather than
/// happening on every daemon start.
fn warm_embedder(graph: Arc<GraphMemory>, log: Arc<DaemonLog>) {
    let models_dir = graph.path().join("models");
    if !has_cached_model(&models_dir) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        match graph.embedder() {
            Ok(_) => log.log(&format!(
                "embedder warm in {}ms",
                started.elapsed().as_millis()
            )),
            Err(err) => log.log(&format!("embedder warm-up failed: {err}")),
        }
    });
}

/// Start the background extraction worker, when this build and this config
/// allow one. `None` means no worker runs; the reason is in the log and in
/// [`ExtractionStatus::disabled_reason`].
#[cfg(feature = "llm")]
fn start_background_extraction(
    options: &ServeOptions,
    graph: &Arc<GraphMemory>,
    context: &Arc<DaemonContext>,
    log: &Arc<DaemonLog>,
) -> Option<tokio::task::JoinHandle<()>> {
    crate::serve_extract::spawn(crate::serve_extract::Setup {
        memory_dir: options.memory_dir.clone(),
        graph: Arc::clone(graph),
        idle: Arc::clone(&context.idle),
        shutdown: Arc::clone(&context.shutdown),
        state: Arc::clone(&context.extraction),
        log: Arc::clone(log),
    })
}

#[cfg(not(feature = "llm"))]
fn start_background_extraction(
    _options: &ServeOptions,
    _graph: &Arc<GraphMemory>,
    context: &Arc<DaemonContext>,
    log: &Arc<DaemonLog>,
) -> Option<tokio::task::JoinHandle<()>> {
    context
        .extraction
        .disable("this binary was built without the `llm` feature");
    log.log("background extraction off: built without the `llm` feature");
    None
}

/// Start the background transcript-capture worker, when this config wants one.
///
/// Independent of extraction on purpose: a user with no LLM provider still gets
/// their Codex and Grok sessions archived, and a user who has turned capture
/// off still gets entities extracted.
fn start_background_capture(
    options: &ServeOptions,
    graph: &Arc<GraphMemory>,
    context: &Arc<DaemonContext>,
    log: &Arc<DaemonLog>,
) -> Option<tokio::task::JoinHandle<()>> {
    crate::serve_capture::spawn(crate::serve_capture::Setup {
        memory_dir: options.memory_dir.clone(),
        graph: Arc::clone(graph),
        idle: Arc::clone(&context.idle),
        shutdown: Arc::clone(&context.shutdown),
        log: Arc::clone(log),
    })
}

/// Wait for the capture worker to finish the transcript in flight.
async fn stop_background_capture(worker: Option<tokio::task::JoinHandle<()>>, log: &DaemonLog) {
    let Some(mut worker) = worker else {
        return;
    };
    if tokio::time::timeout(WORKER_STOP_TIMEOUT, &mut worker)
        .await
        .is_err()
    {
        worker.abort();
        log.log("background capture did not stop in time — abandoned mid-transcript");
    }
}

/// Wait for the background worker to let go of the store, then take it away.
///
/// The worker stops between units on its own; the timeout covers the case
/// where it is inside a synchronous stretch (the ONNX embedder) that no
/// cancellation point can interrupt.
async fn stop_background_extraction(worker: Option<tokio::task::JoinHandle<()>>, log: &DaemonLog) {
    let Some(mut worker) = worker else {
        return;
    };
    if tokio::time::timeout(WORKER_STOP_TIMEOUT, &mut worker)
        .await
        .is_err()
    {
        worker.abort();
        log.log("background extraction did not stop in time — abandoned mid-archive");
    }
}

fn has_cached_model(models_dir: &Path) -> bool {
    std::fs::read_dir(models_dir).is_ok_and(|mut entries| entries.next().is_some())
}

async fn accept_loop(
    listener: UnixListener,
    graph: Arc<GraphMemory>,
    context: Arc<DaemonContext>,
    log: Arc<DaemonLog>,
) {
    loop {
        let poll = context.idle.poll_interval();
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let graph = Arc::clone(&graph);
                    let context = Arc::clone(&context);
                    let log = Arc::clone(&log);
                    tokio::spawn(async move {
                        handle_connection(stream, graph, context, log).await;
                    });
                }
                Err(err) => log.log(&format!("accept error: {err}")),
            },
            () = context.shutdown.wait() => {
                log.log("shutdown requested");
                break;
            }
            () = tokio::time::sleep(poll) => {
                if context.idle.is_idle_at(Instant::now()) {
                    log.log("idle timeout — exiting");
                    break;
                }
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    graph: Arc<GraphMemory>,
    context: Arc<DaemonContext>,
    log: Arc<DaemonLog>,
) {
    let _activity = ActivityGuard::new(Arc::clone(&context));
    if let Err(err) = authorize_peer(&stream, context.owner_uid) {
        log.log(&format!("rejected connection: {err}"));
        return;
    }

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader.take(MAX_REQUEST_BYTES)).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                // The reader stopped at the byte cap instead of at a newline:
                // the client is sending a request larger than we will read.
                if request_cap_reached(&mut lines) {
                    let response = Response::failure(
                        "bad_request",
                        format!("request exceeds the {MAX_REQUEST_BYTES}-byte limit"),
                    );
                    log.log("rejected an oversized request");
                    let _ = write_response(&mut writer, &response).await;
                }
                break;
            }
            Err(err) => {
                log.log(&format!("read error: {err}"));
                break;
            }
        };
        recharge_request_cap(&mut lines);
        if line.trim().is_empty() {
            continue;
        }

        let (response, stop) = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Hello) => (
                Response::success(
                    serde_json::to_value(context.info()).unwrap_or(serde_json::Value::Null),
                ),
                false,
            ),
            Ok(Request::Shutdown) => (
                Response::success(serde_json::json!({ "stopping": true })),
                true,
            ),
            Ok(request) => {
                let started = Instant::now();
                let response = dispatch_graph(&graph, &request).await;
                log.log(&format!(
                    "{} {} in {}ms",
                    request.op_name(),
                    if response.ok { "ok" } else { "failed" },
                    started.elapsed().as_millis()
                ));
                (response, false)
            }
            Err(err) => (
                Response::failure("bad_request", format!("malformed request: {err}")),
                false,
            ),
        };

        if let Err(err) = write_response(&mut writer, &response).await {
            log.log(&format!("write error: {err}"));
            break;
        }
        context.idle.touch_at(Instant::now());

        if stop {
            // The signal latches, so a wakeup that lands while the accept loop
            // is between `select!` iterations is still seen on the next one:
            // the daemon can never outlive its own shutdown request.
            context.shutdown.trigger();
            break;
        }
    }
}

/// A connection's request reader, capped at [`MAX_REQUEST_BYTES`] per request.
type RequestLines = tokio::io::Lines<BufReader<tokio::io::Take<tokio::net::unix::OwnedReadHalf>>>;

/// True when the reader stopped because the request hit the byte cap.
fn request_cap_reached(lines: &mut RequestLines) -> bool {
    lines.get_mut().get_mut().limit() == 0
}

/// Give the next request on this connection its own full byte budget.
fn recharge_request_cap(lines: &mut RequestLines) {
    lines.get_mut().get_mut().set_limit(MAX_REQUEST_BYTES);
}

/// The socket has no authentication of its own: anyone who can open it can
/// read every ingest payload and forge every answer. Only our own uid may.
fn authorize_peer(stream: &UnixStream, owner_uid: u32) -> Result<(), RecallError> {
    let peer = stream.peer_cred().map_err(|err| {
        RecallError::Daemon(format!("cannot read socket peer credentials: {err}"))
    })?;
    check_peer_uid(peer.uid(), owner_uid)
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> Result<(), RecallError> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

/// Path the socket is bound to before it is published under its real name.
fn staging_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_os_string();
    path.push(".new");
    PathBuf::from(path)
}

/// Bind the listening socket, clearing a stale socket left by a dead daemon.
///
/// The socket is bound under a temporary name in the same directory, made
/// owner-only, and only then renamed into place: `bind` applies the process
/// umask, so publishing first would leave a world-reachable socket for as long
/// as it takes to chmod it.
fn bind_socket(socket_path: &Path) -> Result<UnixListener, RecallError> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = socket_path.parent() {
        crate::serve_client::ensure_socket_dir(parent)?;
    }
    if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
        return Err(RecallError::Daemon(format!(
            "another daemon is already listening on {}",
            socket_path.display()
        )));
    }

    let staged = staging_path(socket_path);
    unlink_socket(&staged)?;
    let listener = UnixListener::bind(&staged).map_err(|err| {
        RecallError::Daemon(format!("cannot listen on {}: {err}", staged.display()))
    })?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE)).map_err(
        |err| {
            RecallError::Daemon(format!(
                "cannot restrict the daemon socket {}: {err}",
                staged.display()
            ))
        },
    )?;

    // Whatever sits at the published path is a socket a dead daemon left.
    unlink_socket(socket_path)?;
    std::fs::rename(&staged, socket_path).map_err(|err| {
        let _ = std::fs::remove_file(&staged);
        RecallError::Daemon(format!(
            "cannot publish the daemon socket at {}: {err}",
            socket_path.display()
        ))
    })?;
    Ok(listener)
}

fn write_pidfile(socket_path: &Path) -> Result<(), RecallError> {
    use std::io::Write as _;

    let contents = serde_json::json!({
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "socket_path": socket_path.display().to_string(),
    });
    let path = pidfile_path(socket_path);
    let _ = std::fs::remove_file(&path);
    let mut file = crate::serve_security::create_new_private_file().open(&path)?;
    writeln!(file, "{contents}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn assert_shareable<T: Send + Sync>() {}

    #[test]
    fn graph_memory_is_shareable_across_tasks() {
        // The daemon hands one Arc<GraphMemory> to every connection task.
        assert_shareable::<GraphMemory>();
    }

    #[test]
    fn request_wire_format_is_op_and_args() {
        let request = Request::Search(SearchArgs {
            query: "rust".into(),
            limit: 5,
            entity_type: None,
            keyword: None,
        });
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["op"], "search");
        assert_eq!(json["args"]["query"], "rust");
        assert_eq!(json["args"]["limit"], 5);
    }

    #[test]
    fn control_requests_serialize_without_args() {
        assert_eq!(
            serde_json::to_string(&Request::Hello).unwrap(),
            r#"{"op":"hello"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Shutdown).unwrap(),
            r#"{"op":"shutdown"}"#
        );
    }

    #[test]
    fn request_round_trips_every_variant() {
        let requests = vec![
            Request::Hello,
            Request::Status,
            Request::Search(SearchArgs {
                query: "q".into(),
                limit: 3,
                entity_type: Some("tool".into()),
                keyword: Some("k".into()),
            }),
            Request::SearchEpisodes(SearchEpisodesArgs {
                query: "q".into(),
                limit: 2,
            }),
            Request::Query(QueryArgs {
                query: "q".into(),
                limit: 10,
                entity_type: None,
                keyword: None,
                depth: 2,
                episodes: true,
            }),
            Request::Traverse(TraverseArgs {
                entity: "Rust".into(),
                depth: 1,
                type_filter: None,
            }),
            Request::AddEntity(AddEntityArgs {
                name: "Rust".into(),
                entity_type: "tool".into(),
                abstract_text: "language".into(),
                overview: None,
                source: Some("test".into()),
            }),
            Request::Relate(RelateArgs {
                from: "D".into(),
                rel_type: "USES".into(),
                to: "Rust".into(),
                description: None,
                source: None,
            }),
            Request::IngestArchive(IngestArchiveArgs {
                content: "# log".into(),
                session_id: "s1".into(),
                log_number: Some(7),
                provenance: Some(Provenance::External),
            }),
            Request::SyncPipeline(SyncPipelineArgs {
                docs: PipelineDocuments {
                    learning: "# learning".into(),
                    ..PipelineDocuments::default()
                },
            }),
            Request::Feedback(FeedbackArgs {
                session_id: "s1".into(),
                outcome: OutcomeKind::Success,
            }),
            Request::Shutdown,
        ];

        for request in requests {
            let line = serde_json::to_string(&request).unwrap();
            let parsed: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed, request, "round trip failed for {line}");
        }
    }

    #[test]
    fn ingest_requests_without_provenance_still_parse() {
        // The wire shape older clients send: absent means "infer from turn
        // roles", so a pre-provenance client keeps working unchanged.
        let parsed: Request = serde_json::from_str(
            r##"{"op":"ingest_archive","args":{"content":"# log","session_id":"s1"}}"##,
        )
        .unwrap();
        assert_eq!(
            parsed,
            Request::IngestArchive(IngestArchiveArgs {
                content: "# log".into(),
                session_id: "s1".into(),
                log_number: None,
                provenance: None,
            })
        );
    }

    #[test]
    fn feedback_is_a_hot_op_with_a_snake_case_outcome() {
        let request = Request::Feedback(FeedbackArgs {
            session_id: "conversation-042".into(),
            outcome: OutcomeKind::Failed,
        });
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["op"], "feedback");
        assert_eq!(json["args"]["session_id"], "conversation-042");
        assert_eq!(json["args"]["outcome"], "failed");
        assert_eq!(request.op_name(), "feedback");
    }

    #[test]
    fn optional_args_may_be_omitted_on_the_wire() {
        let parsed: Request =
            serde_json::from_str(r#"{"op":"search","args":{"query":"q","limit":1}}"#).unwrap();
        assert_eq!(
            parsed,
            Request::Search(SearchArgs {
                query: "q".into(),
                limit: 1,
                entity_type: None,
                keyword: None,
            })
        );
    }

    #[test]
    fn success_response_carries_data_only() {
        let response = Response::success(serde_json::json!({"n": 1}));
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["n"], 1);
        assert!(json.get("error").is_none());
        assert_eq!(response.into_result().unwrap(), serde_json::json!({"n": 1}));
    }

    #[test]
    fn failure_response_maps_to_named_remote_error() {
        let response = Response::from_graph_error(&GraphError::Locked("store busy".into()));
        assert_eq!(response.error.as_ref().unwrap().code, "locked");
        let err = response.into_result().unwrap_err();
        assert!(err.to_string().contains("store locked"));
        assert!(matches!(err, RecallError::Remote { .. }));
    }

    #[test]
    fn failure_response_round_trips() {
        let response = Response::failure("bad_request", "malformed");
        let line = serde_json::to_string(&response).unwrap();
        let parsed: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, response);
        assert!(!parsed.ok);
    }

    #[test]
    fn error_codes_are_distinct_per_kind() {
        assert_eq!(error_code(&GraphError::Locked(String::new())), "locked");
        assert_eq!(
            error_code(&GraphError::NotFound(String::new())),
            "not_found"
        );
        assert_eq!(error_code(&GraphError::Embed(String::new())), "embedding");
    }

    #[test]
    fn idle_timer_fires_after_timeout() {
        let start = Instant::now();
        let tracker = IdleTracker::new_at(Some(Duration::from_secs(60)), start);

        assert!(!tracker.is_idle_at(start + Duration::from_secs(59)));
        assert!(tracker.is_idle_at(start + Duration::from_secs(60)));
    }

    #[test]
    fn idle_timer_never_fires_while_a_connection_is_open() {
        let start = Instant::now();
        let tracker = IdleTracker::new_at(Some(Duration::from_secs(1)), start);

        tracker.begin();
        assert!(!tracker.is_idle_at(start + Duration::from_secs(600)));
        tracker.end();

        tracker.touch_at(start);
        assert!(tracker.is_idle_at(start + Duration::from_secs(600)));
    }

    #[test]
    fn activity_resets_the_idle_timer() {
        let start = Instant::now();
        let tracker = IdleTracker::new_at(Some(Duration::from_secs(10)), start);

        tracker.touch_at(start + Duration::from_secs(9));
        assert!(!tracker.is_idle_at(start + Duration::from_secs(18)));
        assert!(tracker.is_idle_at(start + Duration::from_secs(19)));
    }

    /// A background batch must not be cut in half by the idle timeout — and
    /// must not postpone shutdown once it is done, or the daemon would never
    /// exit again.
    #[test]
    fn a_batch_in_flight_defers_idle_shutdown() {
        let start = Instant::now();
        let tracker = Arc::new(IdleTracker::new_at(Some(Duration::from_secs(60)), start));
        let later = start + Duration::from_secs(600);
        assert!(tracker.is_idle_at(later));

        let guard = BackgroundGuard::new(Arc::clone(&tracker));
        assert!(!tracker.is_idle_at(later), "idle exit cut a batch in half");
        // Background work is not activity: the daemon is still unused, which
        // is what lets the *next* batch start.
        assert!(tracker.is_quiet_at(later, Duration::from_secs(60)));

        drop(guard);
        assert!(tracker.is_idle_at(later));
    }

    /// `--foreground` disables idle shutdown, not background work: a
    /// supervised daemon still has quiet periods to extract in.
    #[test]
    fn quiet_is_independent_of_the_idle_timeout() {
        let start = Instant::now();
        let tracker = IdleTracker::new_at(None, start);
        let later = start + Duration::from_secs(300);

        assert!(!tracker.is_idle_at(later));
        assert!(tracker.is_quiet_at(later, Duration::from_secs(120)));
    }

    #[test]
    fn an_open_connection_is_never_quiet() {
        let start = Instant::now();
        let tracker = IdleTracker::new_at(Some(Duration::from_secs(60)), start);
        let later = start + Duration::from_secs(600);

        tracker.begin();
        assert!(tracker.has_connections());
        assert!(!tracker.is_quiet_at(later, Duration::from_secs(1)));

        tracker.end();
        tracker.touch_at(start);
        assert!(tracker.is_quiet_at(later, Duration::from_secs(1)));
    }

    /// The signal latches: a task that checks after it fired still sees it.
    /// Without that, a worker between two units sleeps through the daemon's
    /// exit and keeps the store open.
    #[tokio::test]
    async fn the_shutdown_signal_latches_for_late_waiters() {
        let signal = ShutdownSignal::new();
        assert!(!signal.is_triggered());
        signal.trigger();
        assert!(signal.is_triggered());

        tokio::time::timeout(Duration::from_secs(5), signal.wait())
            .await
            .expect("a waiter that arrives after the trigger still wakes");
        assert!(signal.guard(std::future::pending::<()>()).await.is_none());
        assert!(signal.sleep_until_stopped(Duration::from_secs(600)).await);
    }

    #[tokio::test]
    async fn the_shutdown_signal_lets_work_finish_when_it_is_not_triggered() {
        let signal = ShutdownSignal::new();
        assert_eq!(signal.guard(async { 7 }).await, Some(7));
        assert!(!signal.sleep_until_stopped(Duration::from_millis(1)).await);
    }

    #[test]
    fn extraction_status_reports_progress_and_refusals() {
        let state = ExtractionState::shared();
        let now = Instant::now();
        assert!(!state.snapshot(now).enabled);

        state.enable();
        state.record_batch(3, Duration::from_millis(250), now);
        let status = state.snapshot(now + Duration::from_secs(9));
        assert!(status.enabled);
        assert_eq!(status.runs, 1);
        assert_eq!(status.archives, 3);
        assert_eq!(status.last_run_secs_ago, Some(9));
        assert_eq!(status.last_run_ms, Some(250));

        state.disable("no usable LLM provider");
        let status = state.snapshot(now);
        assert!(!status.enabled);
        assert_eq!(
            status.disabled_reason.as_deref(),
            Some("no usable LLM provider")
        );
        // What it managed to do before it stopped is still worth reporting.
        assert_eq!(status.archives, 3);
    }

    /// A client built before background extraction sends no `extraction` field.
    #[test]
    fn daemon_info_without_extraction_still_parses() {
        let info: DaemonInfo = serde_json::from_str(
            r#"{"version":"3.0.0","pid":1,"memory_dir":"/m","socket_path":"/s","uptime_secs":5}"#,
        )
        .expect("older daemon info");
        assert_eq!(info.extraction, ExtractionStatus::default());
        assert!(!info.extraction.enabled);
    }

    #[test]
    fn idle_shutdown_disabled_without_timeout() {
        let start = Instant::now();
        let tracker = IdleTracker::new_at(None, start);
        assert!(!tracker.is_idle_at(start + Duration::from_secs(86_400)));
        assert_eq!(tracker.poll_interval(), MAX_IDLE_POLL);
    }

    #[test]
    fn poll_interval_is_bounded() {
        let now = Instant::now();
        assert_eq!(
            IdleTracker::new_at(Some(Duration::from_millis(10)), now).poll_interval(),
            MIN_IDLE_POLL
        );
        assert_eq!(
            IdleTracker::new_at(Some(Duration::from_secs(3600)), now).poll_interval(),
            MAX_IDLE_POLL
        );
        assert_eq!(
            IdleTracker::new_at(Some(Duration::from_secs(100)), now).poll_interval(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn only_repeatable_operations_are_retryable() {
        assert!(Request::Status.is_retryable());
        assert!(Request::Traverse(TraverseArgs {
            entity: "Rust".into(),
            depth: 1,
            type_filter: None,
        })
        .is_retryable());

        // Replaying this one would duplicate every episode of a conversation.
        assert!(!Request::IngestArchive(IngestArchiveArgs {
            content: "# log".into(),
            session_id: "s1".into(),
            log_number: Some(1),
            provenance: None,
        })
        .is_retryable());
        assert!(!Request::AddEntity(AddEntityArgs {
            name: "Rust".into(),
            entity_type: "tool".into(),
            abstract_text: "language".into(),
            overview: None,
            source: None,
        })
        .is_retryable());
    }

    #[test]
    fn wire_limits_are_clamped_into_a_servable_range() {
        assert_eq!(clamp_limit(10), 10);
        assert_eq!(clamp_limit(MAX_LIMIT), MAX_LIMIT);
        // A limit this large overflows the `limit * 4` KNN `ef` computation
        // and asks SurrealDB for an unbounded scan.
        assert_eq!(clamp_limit(usize::MAX), MAX_LIMIT);
        assert_eq!(clamp_limit(0), 1);
    }

    #[test]
    fn wire_depths_are_clamped_into_a_servable_range() {
        assert_eq!(clamp_depth(2), 2);
        assert_eq!(clamp_depth(MAX_DEPTH), MAX_DEPTH);
        assert_eq!(clamp_depth(u32::MAX), MAX_DEPTH);
        assert_eq!(clamp_depth(0), 1);
    }

    #[test]
    fn the_staged_socket_sits_beside_the_published_one() {
        let socket = Path::new("/run/recall-echo/abc.sock");
        let staged = staging_path(socket);
        assert_eq!(staged.parent(), socket.parent());
        assert_ne!(staged, socket);
        // `sockaddr_un.sun_path` is 108 bytes and the published path is capped
        // at 100, so the staging name must stay within the remainder.
        assert!(staged.as_os_str().len() - socket.as_os_str().len() <= 7);
    }

    #[test]
    fn pidfile_sits_beside_the_socket() {
        assert_eq!(
            pidfile_path(Path::new("/run/recall-echo/abc.sock")),
            PathBuf::from("/run/recall-echo/abc.sock.pid")
        );
    }

    #[test]
    fn serve_options_from_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            format!(
                "[serve]\nsocket_path = \"{}/graph.sock\"\n",
                dir.path().display()
            ),
        )
        .unwrap();

        let options = ServeOptions::from_config(dir.path(), false).unwrap();
        assert_eq!(options.idle_timeout, Some(Duration::from_secs(3600)));
        assert!(!options.log_to_stderr);
        assert_eq!(options.socket_path, dir.path().join("graph.sock"));
    }

    #[test]
    fn foreground_disables_idle_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            format!(
                "[serve]\nsocket_path = \"{}/graph.sock\"\nidle_timeout_secs = 30\n",
                dir.path().display()
            ),
        )
        .unwrap();

        let options = ServeOptions::from_config(dir.path(), true).unwrap();
        assert_eq!(options.idle_timeout, None);
        assert!(options.log_to_stderr);
    }

    #[test]
    fn zero_idle_timeout_disables_idle_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            format!(
                "[serve]\nsocket_path = \"{}/graph.sock\"\nidle_timeout_secs = 0\n",
                dir.path().display()
            ),
        )
        .unwrap();

        assert_eq!(
            ServeOptions::from_config(dir.path(), false)
                .unwrap()
                .idle_timeout,
            None
        );
    }
}
