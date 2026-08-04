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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::error::RecallError;
use crate::graph::error::GraphError;
use crate::graph::types::{EntityType, NewEntity, NewRelationship, QueryOptions, SearchOptions};
use crate::graph::GraphMemory;

/// Longest idle-poll interval; keeps a long-lived daemon from spinning.
const MAX_IDLE_POLL: Duration = Duration::from_secs(30);
/// Shortest idle-poll interval; keeps short test timeouts responsive.
const MIN_IDLE_POLL: Duration = Duration::from_millis(100);

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
            Request::Shutdown => "shutdown",
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
                limit: args.limit,
                entity_type: args.entity_type.clone(),
                keyword: args.keyword.clone(),
            };
            serde_json::to_value(graph.search_with_options(&args.query, &options).await?)?
        }
        Request::SearchEpisodes(args) => {
            serde_json::to_value(graph.search_episodes(&args.query, args.limit).await?)?
        }
        Request::Query(args) => {
            let options = QueryOptions {
                limit: args.limit,
                entity_type: args.entity_type.clone(),
                keyword: args.keyword.clone(),
                graph_depth: args.depth,
                include_episodes: args.episodes,
            };
            serde_json::to_value(graph.query(&args.query, &options).await?)?
        }
        Request::Traverse(args) => serde_json::to_value(
            graph
                .traverse_filtered(&args.entity, args.depth, args.type_filter.as_deref())
                .await?,
        )?,
        Request::AddEntity(args) => {
            let entity_type: EntityType = args
                .entity_type
                .parse()
                .map_err(|e: String| GraphError::Parse(e))?;
            let entity = graph
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
            let report = graph
                .ingest_archive(&args.content, &args.session_id, args.log_number, None)
                .await?;
            serde_json::to_value(report)?
        }
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
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
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

    // Own the store before advertising the socket: clients that connect only
    // after a successful bind never see a half-initialized daemon.
    let graph = match GraphMemory::open_embedded(&graph_dir).await {
        Ok(graph) => Arc::new(graph),
        Err(err) => {
            log.log(&format!("failed to open graph store: {err}"));
            return Err(err.into());
        }
    };

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
        idle: IdleTracker::new(options.idle_timeout),
        shutdown: Notify::new(),
    });
    log.log("ready");

    accept_loop(listener, graph, Arc::clone(&context), Arc::clone(&log)).await;

    let _ = std::fs::remove_file(&options.socket_path);
    let _ = std::fs::remove_file(pidfile_path(&options.socket_path));
    log.log("stopped");
    Ok(())
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
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                log.log(&format!("read error: {err}"));
                break;
            }
        };
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

/// Bind the listening socket, clearing a stale socket left by a dead daemon.
fn bind_socket(socket_path: &Path) -> Result<UnixListener, RecallError> {
    if let Some(parent) = socket_path.parent() {
        crate::serve_client::ensure_socket_dir(parent)?;
    }

    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Err(RecallError::Daemon(format!(
                    "another daemon is already listening on {}",
                    socket_path.display()
                )));
            }
            std::fs::remove_file(socket_path)?;
            UnixListener::bind(socket_path).map_err(|err| {
                RecallError::Daemon(format!("cannot listen on {}: {err}", socket_path.display()))
            })
        }
        Err(err) => Err(RecallError::Daemon(format!(
            "cannot listen on {}: {err}",
            socket_path.display()
        ))),
    }
}

fn write_pidfile(socket_path: &Path) -> Result<(), RecallError> {
    let contents = serde_json::json!({
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "socket_path": socket_path.display().to_string(),
    });
    std::fs::write(pidfile_path(socket_path), format!("{contents}\n"))?;
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
