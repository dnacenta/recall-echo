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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    pub memory_dir: String,
    pub socket_path: String,
    pub uptime_secs: u64,
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

/// Tracks daemon activity so an unused daemon shuts itself down.
///
/// A daemon is idle when no connection is open *and* the last completed
/// request is older than the configured timeout. `None` disables idle
/// shutdown entirely (`--foreground`, or `idle_timeout_secs = 0`).
#[derive(Debug)]
pub struct IdleTracker {
    timeout: Option<Duration>,
    active: AtomicUsize,
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

    /// Record activity at `now`.
    pub fn touch_at(&self, now: Instant) {
        let mut last = self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *last = now;
    }

    /// True when the daemon has been unused for longer than the timeout.
    #[must_use]
    pub fn is_idle_at(&self, now: Instant) -> bool {
        let Some(timeout) = self.timeout else {
            return false;
        };
        if self.active.load(Ordering::SeqCst) > 0 {
            return false;
        }
        let last = *self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        now.saturating_duration_since(last) >= timeout
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
    idle: IdleTracker,
    shutdown: Notify,
}

impl DaemonContext {
    fn info(&self) -> DaemonInfo {
        DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            memory_dir: self.memory_dir.display().to_string(),
            socket_path: self.socket_path.display().to_string(),
            uptime_secs: self.started.elapsed().as_secs(),
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

    let context = Arc::new(DaemonContext {
        started: Instant::now(),
        memory_dir: options.memory_dir.clone(),
        socket_path: options.socket_path.clone(),
        owner_uid,
        idle: IdleTracker::new(options.idle_timeout),
        shutdown: Notify::new(),
    });
    warm_embedder(Arc::clone(&graph), Arc::clone(&log));
    log.log("ready");

    accept_loop(
        listener,
        Arc::clone(&graph),
        Arc::clone(&context),
        Arc::clone(&log),
    )
    .await;

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
            () = context.shutdown.notified() => {
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
            // `notify_one` stores a permit when the accept loop happens to be
            // between `select!` iterations; `notify_waiters` would be lost
            // there and the daemon would outlive the shutdown request.
            context.shutdown.notify_one();
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
