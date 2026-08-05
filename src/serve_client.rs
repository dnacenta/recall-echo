// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client side of the graph daemon — connect, or start one and connect.
//!
//! Every hot graph operation (search, query, traverse, status, ingest, entity
//! and relationship writes) goes through here. There is no embedded fallback:
//! when the daemon cannot be reached *and* cannot be started, that is a named
//! [`RecallError::Daemon`] error, never a silent degradation.
//!
//! Two escapes exist:
//!
//! - `[graph] mode = "server"` (advanced): the store is an external SurrealDB
//!   server, so there is nothing to serialize — requests run in-process.
//! - [`exclusive`]: admin operations (init, gc, extraction, bulk ingest, …)
//!   take an admin lock, stop the daemon and keep the store for themselves;
//!   hot operations that arrive meanwhile wait for the lock, and the next one
//!   after it is released starts a fresh daemon.
//!
//! Both ends of the socket check the peer's uid, and the socket, its
//! directory, the pidfile and the daemon log are all owner-only — see
//! [`crate::serve_security`].

use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::error::RecallError;
use crate::graph::GraphMemory;
use crate::serve::{DaemonInfo, Request, Response};
use crate::serve_security::{
    append_private_file, check_peer_uid, create_new_private_file, create_private_dir, current_uid,
    require_owned_dir, unlink_socket,
};

/// Environment override for the binary used to start the daemon.
/// Defaults to the running executable; tests and wrappers set it explicitly.
pub const DAEMON_BIN_ENV: &str = "RECALL_ECHO_BIN";

/// Environment the detached daemon inherits. Everything else is dropped: the
/// daemon outlives the command that started it by up to an hour, and hook
/// contexts hand their process API credentials to every child they spawn.
const DAEMON_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "XDG_RUNTIME_DIR",
    "RECALL_ECHO_HOME",
    DAEMON_BIN_ENV,
    // fastembed downloads the ONNX model over TLS on first use.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
];

/// `sockaddr_un.sun_path` is 108 bytes on Linux; stay well inside it.
const MAX_SOCKET_PATH_LEN: usize = 100;
/// How long to wait for a freshly spawned daemon to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Polling interval while waiting for a socket to appear or disappear.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// A spawn lockfile older than this belongs to a client that died mid-spawn.
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);
/// How long to wait for a daemon to answer the version handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on a single request/response exchange with the daemon. Long
/// enough for an ingest of a large archive, short enough that a wedged daemon
/// cannot hang a session hook forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Upper bound on connect → spawn → reconnect rounds.
const MAX_CONNECT_ROUNDS: u32 = 3;
/// How long a hot operation waits for an admin operation to release the store
/// before failing with a named error.
pub(crate) const ADMIN_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
/// An admin lock whose owner cannot be found in `/proc` is stale immediately;
/// where process liveness is unknowable, it is stale after this long.
const ADMIN_LOCK_STALE_AGE: Duration = Duration::from_secs(900);
/// How many times `exclusive` re-stops a daemon that raced in behind it.
const MAX_STOP_ROUNDS: u32 = 3;

// ── Socket location ──────────────────────────────────────────────────────

/// Socket path for a memory directory.
///
/// `$XDG_RUNTIME_DIR/recall-echo/<hash>.sock`, falling back to
/// `/tmp/recall-echo-<uid>/<hash>.sock`, unless `[serve] socket_path`
/// overrides it. The hash is taken over the canonical memory directory, so
/// every graph gets its own daemon and symlinked paths share one.
pub fn socket_path(memory_dir: &Path) -> Result<PathBuf, RecallError> {
    let config = crate::config::load_from_dir(memory_dir);
    let path = match config.serve.socket_path.as_deref() {
        Some(configured) if !configured.trim().is_empty() => {
            PathBuf::from(crate::paths::expand_tilde(configured.trim()))
        }
        _ => runtime_dir()?.join(format!("{}.sock", path_hash(&canonical(memory_dir)))),
    };

    if path.as_os_str().as_bytes().len() > MAX_SOCKET_PATH_LEN {
        return Err(RecallError::Daemon(format!(
            "daemon socket path is too long ({} bytes, max {MAX_SOCKET_PATH_LEN}): {}. \
             Set `[serve] socket_path` in .recall-echo.toml to a shorter path.",
            path.as_os_str().as_bytes().len(),
            path.display()
        )));
    }
    Ok(path)
}

/// The directory recall-echo derives its own sockets in.
fn runtime_dir() -> Result<PathBuf, RecallError> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("recall-echo")),
        _ => Ok(PathBuf::from(format!(
            "/tmp/recall-echo-{}",
            current_uid()?
        ))),
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// FNV-1a 64 of a path — stable across processes and releases, unlike
/// `DefaultHasher`, which is what a socket name needs.
fn path_hash(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Make sure the socket directory exists and only this user can write into it.
///
/// recall-echo creates (and then validates as `0o700`) the runtime directory
/// it derives itself. Any other directory comes from `[serve] socket_path`,
/// i.e. from a config file that may not be trustworthy — it is validated but
/// never created and never chmod-ed, so a config file cannot make the tool
/// mutate an arbitrary directory.
pub(crate) fn ensure_socket_dir(dir: &Path) -> Result<(), RecallError> {
    if runtime_dir().is_ok_and(|derived| derived == dir) {
        create_private_dir(dir)
    } else {
        require_owned_dir(dir)
    }
}

// ── Public entry points ──────────────────────────────────────────────────

/// Backend mode from `[graph] mode` — `embedded` (daemon) or `server` (direct).
#[must_use]
pub fn graph_mode(memory_dir: &Path) -> String {
    crate::config::load_from_dir(memory_dir)
        .graph
        .map_or_else(|| "embedded".to_string(), |graph| graph.mode)
}

fn uses_daemon(memory_dir: &Path) -> bool {
    graph_mode(memory_dir) != "server"
}

/// Run a hot graph operation, starting the daemon if necessary.
///
/// In `mode = "server"` the request runs in-process against the external
/// SurrealDB server; the result is identical either way.
pub async fn execute(
    memory_dir: &Path,
    request: &Request,
) -> Result<serde_json::Value, RecallError> {
    if !uses_daemon(memory_dir) {
        return execute_direct(memory_dir, request).await;
    }

    let socket = socket_path(memory_dir)?;
    let mut connection = connect_or_spawn(memory_dir, &socket).await?;
    match connection.call(request).await {
        Ok(response) => response.into_result(),
        // The daemon went away between the handshake and its answer (a crash,
        // or an `exclusive` operation racing us). One fresh daemon, one retry —
        // but only for a request that is safe to apply twice.
        Err(CallError::Disconnected(_)) if request.is_retryable() => {
            let mut connection = connect_or_spawn(memory_dir, &socket).await?;
            connection.call(request).await?.into_result()
        }
        Err(err) => Err(err.into()),
    }
}

async fn execute_direct(
    memory_dir: &Path,
    request: &Request,
) -> Result<serde_json::Value, RecallError> {
    let graph = GraphMemory::open(&memory_dir.join("graph")).await?;
    crate::serve::dispatch_graph(&graph, request)
        .await
        .into_result()
}

/// Take exclusive ownership of the store for an admin operation.
///
/// Takes the admin lock, stops a running daemon, opens the store in-process
/// and runs `operation`. Hot clients that arrive while the lock is held wait
/// for it instead of starting a daemon, so the store has exactly one owner at
/// every instant. The daemon is not restarted here — the next hot operation
/// starts a fresh one.
///
/// The lock is released only after the store has been closed, so the client
/// that wakes up next never races the admin operation for the file lock.
pub async fn exclusive<T, F, Fut>(memory_dir: &Path, operation: F) -> Result<T, RecallError>
where
    F: FnOnce(GraphMemory) -> Fut,
    Fut: std::future::Future<Output = Result<T, RecallError>>,
{
    let graph_dir = memory_dir.join("graph");
    if !uses_daemon(memory_dir) {
        return operation(GraphMemory::open(&graph_dir).await?).await;
    }

    let socket = socket_path(memory_dir)?;
    let admin = acquire_admin_lock(&socket, Instant::now() + ADMIN_WAIT_TIMEOUT).await?;
    stop_daemon_for_admin(memory_dir).await?;

    let graph = GraphMemory::open_embedded(&graph_dir).await?;
    let result = operation(graph).await;
    drop(admin);
    result
}

/// Stop the daemon, and any daemon that races in behind it.
async fn stop_daemon_for_admin(memory_dir: &Path) -> Result<(), RecallError> {
    for _ in 0..MAX_STOP_ROUNDS {
        if !stop_daemon(memory_dir).await? {
            return Ok(());
        }
    }
    Err(RecallError::Daemon(format!(
        "a graph daemon for {} keeps restarting; cannot take the store exclusively",
        memory_dir.display()
    )))
}

/// Identity of the running daemon, or `None` when none is running (or when
/// the store is an external server). Never starts a daemon.
pub async fn daemon_info(memory_dir: &Path) -> Result<Option<DaemonInfo>, RecallError> {
    if !uses_daemon(memory_dir) {
        return Ok(None);
    }
    let socket = socket_path(memory_dir)?;
    let Some(mut connection) = Connection::try_connect(&socket).await else {
        return Ok(None);
    };
    match connection.hello().await {
        Ok(info) => Ok(Some(info)),
        Err(_) => Ok(None),
    }
}

/// Stop the daemon for this memory directory. Returns whether one was running.
pub async fn stop_daemon(memory_dir: &Path) -> Result<bool, RecallError> {
    if !uses_daemon(memory_dir) {
        return Ok(false);
    }
    let socket = socket_path(memory_dir)?;
    let Some(mut connection) = Connection::try_connect(&socket).await else {
        // Nothing listening — clear a stale socket so the next start is clean.
        unlink_socket(&socket)?;
        return Ok(false);
    };

    match connection.call(&Request::Shutdown).await {
        Ok(response) => {
            response.into_result()?;
        }
        // The daemon died (or shut itself down) before it could answer: the
        // end state we asked for is the one we got.
        Err(CallError::Disconnected(_)) => {}
        Err(err) => return Err(err.into()),
    }
    drop(connection);
    wait_for_socket_gone(&socket, Instant::now() + START_TIMEOUT).await?;
    Ok(true)
}

// ── Connection ───────────────────────────────────────────────────────────

/// Why a request over the daemon socket failed.
///
/// The distinction matters on the hook path: a dead daemon is recoverable by
/// starting a fresh one, while a protocol or timeout failure is not and must
/// surface as-is.
enum CallError {
    /// The daemon closed the connection: idle timeout, shutdown, or a crash.
    Disconnected(RecallError),
    /// Anything else — retrying will not help.
    Fatal(RecallError),
}

impl From<CallError> for RecallError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::Disconnected(err) | CallError::Fatal(err) => err,
        }
    }
}

/// A broken pipe or reset means the daemon went away mid-request.
fn classify_io(err: std::io::Error) -> CallError {
    match err.kind() {
        ErrorKind::BrokenPipe
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::UnexpectedEof
        | ErrorKind::NotConnected => CallError::Disconnected(err.into()),
        _ => CallError::Fatal(err.into()),
    }
}

/// One JSON-line conversation with a daemon.
struct Connection {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Connection {
    async fn connect(socket: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        verify_daemon_peer(&stream)?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    /// Connect, treating every failure as "no daemon there".
    async fn try_connect(socket: &Path) -> Option<Self> {
        Self::connect(socket).await.ok()
    }

    /// Send one request and read its response, bounded by [`CALL_TIMEOUT`].
    async fn call(&mut self, request: &Request) -> Result<Response, CallError> {
        match tokio::time::timeout(CALL_TIMEOUT, self.exchange(request)).await {
            Ok(result) => result,
            Err(_) => Err(CallError::Fatal(RecallError::Daemon(format!(
                "the graph daemon did not answer `{}` within {}s — see the daemon log",
                request.op_name(),
                CALL_TIMEOUT.as_secs()
            )))),
        }
    }

    async fn exchange(&mut self, request: &Request) -> Result<Response, CallError> {
        let mut line = serde_json::to_vec(request).map_err(|err| CallError::Fatal(err.into()))?;
        line.push(b'\n');
        self.writer.write_all(&line).await.map_err(classify_io)?;
        self.writer.flush().await.map_err(classify_io)?;

        let mut response_line = String::new();
        let read = self
            .reader
            .read_line(&mut response_line)
            .await
            .map_err(classify_io)?;
        if read == 0 {
            return Err(CallError::Disconnected(RecallError::Daemon(format!(
                "daemon closed the connection while handling `{}` — see the daemon log",
                request.op_name()
            ))));
        }
        serde_json::from_str(&response_line).map_err(|err| CallError::Fatal(err.into()))
    }

    async fn hello(&mut self) -> Result<DaemonInfo, CallError> {
        let response =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.exchange(&Request::Hello)).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(CallError::Fatal(RecallError::Daemon(format!(
                        "daemon did not answer the version handshake within {}s",
                        HANDSHAKE_TIMEOUT.as_secs()
                    ))))
                }
            };
        let data = response.into_result().map_err(CallError::Fatal)?;
        serde_json::from_value(data).map_err(|err| CallError::Fatal(err.into()))
    }
}

/// Refuse to talk to a daemon running as another user: it would see every
/// ingest payload we send and could answer every query with anything it likes.
fn verify_daemon_peer(stream: &UnixStream) -> std::io::Result<()> {
    let peer = stream.peer_cred()?;
    let owner = current_uid().map_err(permission_denied)?;
    check_peer_uid(peer.uid(), owner).map_err(permission_denied)
}

fn permission_denied(err: RecallError) -> std::io::Error {
    std::io::Error::new(ErrorKind::PermissionDenied, err.to_string())
}

/// Connect to the daemon for `socket`, starting one if needed.
async fn connect_or_spawn(memory_dir: &Path, socket: &Path) -> Result<Connection, RecallError> {
    // Establish that the socket lives somewhere only we can write *before*
    // touching anything in it, so an unusable location is reported as such
    // rather than as a puzzling connect failure.
    if let Some(parent) = socket.parent() {
        ensure_socket_dir(parent)?;
    }
    let deadline = Instant::now() + START_TIMEOUT;

    for _ in 0..MAX_CONNECT_ROUNDS {
        match Connection::connect(socket).await {
            Ok(mut connection) => match connection.hello().await {
                Ok(info) if info.version == env!("CARGO_PKG_VERSION") => return Ok(connection),
                Ok(_) => {
                    // Upgraded binary, stale daemon: ask it to go, then respawn.
                    let _ = connection.call(&Request::Shutdown).await;
                    drop(connection);
                    wait_for_socket_gone(socket, deadline).await?;
                }
                // The daemon closed the socket mid-handshake — it went idle or
                // an admin operation stopped it. Clean up and start a fresh one.
                Err(CallError::Disconnected(_)) => {
                    drop(connection);
                    unlink_socket(socket)?;
                }
                Err(CallError::Fatal(err)) => return Err(err),
            },
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                return Err(RecallError::Daemon(format!(
                    "permission denied opening the daemon socket {}: {err}. \
                     Check ownership of the socket directory, or set \
                     `[serve] socket_path` in .recall-echo.toml.",
                    socket.display()
                )));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(_) => {
                // Socket file exists but nothing is listening (crashed daemon).
                unlink_socket(socket)?;
            }
        }

        start_daemon(memory_dir, socket, Instant::now() + START_TIMEOUT).await?;
    }

    Err(RecallError::Daemon(format!(
        "gave up connecting to the graph daemon on {} after {MAX_CONNECT_ROUNDS} attempts",
        socket.display()
    )))
}

/// Start a daemon — exactly one client wins the race; the rest wait.
///
/// An admin operation ([`exclusive`]) owns the store while its lock is held,
/// so a daemon started now would collide with it: wait for the lock first.
async fn start_daemon(
    memory_dir: &Path,
    socket: &Path,
    deadline: Instant,
) -> Result<(), RecallError> {
    wait_for_admin_lock(socket, Instant::now() + ADMIN_WAIT_TIMEOUT).await?;

    match acquire_spawn_lock(socket)? {
        Some(lock) => {
            let result = match spawn_daemon(memory_dir, socket) {
                Ok(mut child) => {
                    wait_for_socket(socket, memory_dir, deadline, Some(&mut child)).await
                }
                Err(err) => Err(err),
            };
            drop(lock);
            result
        }
        None => wait_for_socket(socket, memory_dir, deadline, None).await,
    }
}

/// Holds the `O_EXCL` spawn lockfile; removes it on drop.
struct SpawnLock(PathBuf);

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn lock_path(socket: &Path) -> PathBuf {
    let mut path = socket.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

/// `Some(lock)` — this client spawns the daemon. `None` — another client is
/// already spawning one; wait for its socket.
fn acquire_spawn_lock(socket: &Path) -> Result<Option<SpawnLock>, RecallError> {
    if let Some(parent) = socket.parent() {
        ensure_socket_dir(parent)?;
    }
    let path = lock_path(socket);

    match create_lock_file(&path) {
        Ok(()) => Ok(Some(SpawnLock(path))),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            if lock_is_stale(&path) {
                let _ = std::fs::remove_file(&path);
                return match create_lock_file(&path) {
                    Ok(()) => Ok(Some(SpawnLock(path))),
                    Err(_) => Ok(None),
                };
            }
            Ok(None)
        }
        Err(err) => Err(RecallError::Daemon(format!(
            "cannot create the daemon spawn lock {}: {err}",
            path.display()
        ))),
    }
}

fn create_lock_file(path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = create_new_private_file().open(path)?;
    writeln!(file, "{}", std::process::id())
}

fn lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified.elapsed().unwrap_or_default() > STALE_LOCK_AGE)
        .unwrap_or(true)
}

// ── Admin lock ───────────────────────────────────────────────────────────

/// Exclusive ownership of a store by an admin operation.
///
/// Held for the whole operation — which can run for minutes — and released
/// only after the store has been closed. Hot clients wait for it instead of
/// starting a daemon that would collide with the operation.
///
/// Crash safety comes from the owning pid rather than from a heartbeat: an
/// admin operation blocks its thread inside the ONNX embedder for long
/// stretches, so a timer-based heartbeat would report a healthy operation as
/// dead. Where process liveness cannot be read (`/proc` absent), the lock's
/// mtime bounds how long a crashed holder can block others.
#[derive(Debug)]
struct AdminLock {
    path: PathBuf,
}

impl Drop for AdminLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Path of the admin lockfile that accompanies a socket.
fn admin_lock_path(socket: &Path) -> PathBuf {
    let mut path = socket.as_os_str().to_os_string();
    path.push(".admin");
    PathBuf::from(path)
}

/// Take the admin lock, waiting for another admin operation to finish.
async fn acquire_admin_lock(socket: &Path, deadline: Instant) -> Result<AdminLock, RecallError> {
    if let Some(parent) = socket.parent() {
        ensure_socket_dir(parent)?;
    }
    let path = admin_lock_path(socket);

    loop {
        match create_lock_file(&path) {
            Ok(()) => return Ok(AdminLock { path }),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if !admin_lock_is_live(&path) {
                    let _ = std::fs::remove_file(&path);
                } else if Instant::now() >= deadline {
                    return Err(admin_wait_timeout(&path));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                if Instant::now() >= deadline {
                    return Err(admin_wait_timeout(&path));
                }
            }
            Err(err) => {
                return Err(RecallError::Daemon(format!(
                    "cannot create the admin lock {}: {err}",
                    path.display()
                )))
            }
        }
    }
}

/// Wait until no admin operation owns the store for `socket`.
pub(crate) async fn wait_for_admin_lock(
    socket: &Path,
    deadline: Instant,
) -> Result<(), RecallError> {
    let path = admin_lock_path(socket);
    while admin_lock_is_live(&path) {
        if Instant::now() >= deadline {
            return Err(admin_wait_timeout(&path));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Ok(())
}

fn admin_wait_timeout(path: &Path) -> RecallError {
    RecallError::Daemon(format!(
        "a recall-echo admin operation has owned the graph store for more than {}s \
         (lock {}). Wait for it to finish, or remove the lock if its process is gone.",
        ADMIN_WAIT_TIMEOUT.as_secs(),
        path.display()
    ))
}

/// True when an admin lockfile belongs to a live admin operation.
pub(crate) fn admin_lock_is_held(socket: &Path) -> bool {
    admin_lock_is_live(&admin_lock_path(socket))
}

fn admin_lock_is_live(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    match lock_owner_pid(path).map(process_is_alive) {
        Some(Some(alive)) => alive,
        // Unreadable pid, or a platform without `/proc`: fall back to age.
        _ => meta
            .modified()
            .map(|modified| modified.elapsed().unwrap_or_default() < ADMIN_LOCK_STALE_AGE)
            .unwrap_or(false),
    }
}

fn lock_owner_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// `Some(alive)` where process liveness can be read from `/proc` (Linux),
/// `None` where it cannot.
fn process_is_alive(pid: u32) -> Option<bool> {
    if !Path::new("/proc/self").exists() {
        return None;
    }
    Some(Path::new(&format!("/proc/{pid}")).exists())
}

fn daemon_binary() -> Result<PathBuf, RecallError> {
    if let Some(binary) = std::env::var_os(DAEMON_BIN_ENV) {
        return Ok(PathBuf::from(binary));
    }
    std::env::current_exe().map_err(|err| {
        RecallError::Daemon(format!(
            "cannot locate the recall-echo binary to start the graph daemon: {err}. \
             Set {DAEMON_BIN_ENV} to its path."
        ))
    })
}

/// Spawn `recall-echo serve --dir <memory_dir>` detached, with stdio going to
/// the daemon log so panics and native-library output are captured.
fn spawn_daemon(memory_dir: &Path, socket: &Path) -> Result<std::process::Child, RecallError> {
    use std::os::unix::process::CommandExt as _;

    let binary = daemon_binary()?;
    let log_path = daemon_log_path(memory_dir);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = append_private_file().open(&log_path).map_err(|err| {
        RecallError::Daemon(format!(
            "cannot open the daemon log {}: {err}",
            log_path.display()
        ))
    })?;

    let mut command = std::process::Command::new(&binary);
    command
        .arg("serve")
        .arg("--dir")
        .arg(memory_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        // Own process group: terminal signals aimed at the client never reach
        // the daemon, and the daemon outlives the shell that started it.
        .process_group(0);
    apply_daemon_env(&mut command);

    command.spawn().map_err(|err| {
        RecallError::Daemon(format!(
            "cannot start the graph daemon ({} serve --dir {}): {err}. \
             Socket: {}",
            binary.display(),
            memory_dir.display(),
            socket.display()
        ))
    })
}

/// Hand the daemon a minimal environment.
///
/// The daemon is detached and long-lived; inheriting the client's environment
/// would hand it whatever credentials the calling context happens to export
/// (a Claude Code hook exports API keys) for the rest of its hour-long life.
fn apply_daemon_env(command: &mut std::process::Command) {
    command.env_clear();
    for key in DAEMON_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

/// Daemon log path for a memory directory.
#[must_use]
pub fn daemon_log_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join("graph").join("daemon.log")
}

/// Wait until the daemon accepts connections.
///
/// When `child` is the daemon this client just spawned, its early exit (a
/// locked store, a failed bind) is reported immediately instead of after the
/// full start timeout.
async fn wait_for_socket(
    socket: &Path,
    memory_dir: &Path,
    deadline: Instant,
    mut child: Option<&mut std::process::Child>,
) -> Result<(), RecallError> {
    while Instant::now() < deadline {
        if Connection::try_connect(socket).await.is_some() {
            return Ok(());
        }
        if let Some(child) = child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(RecallError::Daemon(format!(
                    "the graph daemon exited immediately ({status}). Last daemon log lines:\n{}",
                    log_tail(&daemon_log_path(memory_dir), 5)
                )));
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(RecallError::Daemon(format!(
        "the graph daemon did not start within {}s (socket {} never accepted a connection). \
         Last daemon log lines:\n{}",
        START_TIMEOUT.as_secs(),
        socket.display(),
        log_tail(&daemon_log_path(memory_dir), 5)
    )))
}

async fn wait_for_socket_gone(socket: &Path, deadline: Instant) -> Result<(), RecallError> {
    while Instant::now() < deadline {
        if Connection::try_connect(socket).await.is_none() {
            unlink_socket(socket)?;
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(RecallError::Daemon(format!(
        "the graph daemon on {} did not stop when asked",
        socket.display()
    )))
}

/// Last `lines` lines of the daemon log, for error messages.
fn log_tail(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let tail: Vec<&str> = contents
                .lines()
                .rev()
                .take(lines)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if tail.is_empty() {
                "  (daemon log is empty)".to_string()
            } else {
                tail.iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Err(err) => format!("  (no daemon log at {}: {err})", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_hash_is_stable_and_distinct() {
        let first = path_hash(Path::new("/home/echo/memory"));
        assert_eq!(first, path_hash(Path::new("/home/echo/memory")));
        assert_ne!(first, path_hash(Path::new("/home/echo/memory2")));
        assert_eq!(first.len(), 16);
    }

    #[test]
    fn socket_path_is_per_memory_dir() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert_ne!(
            socket_path(first.path()).unwrap(),
            socket_path(second.path()).unwrap()
        );
    }

    #[test]
    fn socket_path_honors_config_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[serve]\nsocket_path = \"/tmp/re-test.sock\"\n",
        )
        .unwrap();
        assert_eq!(
            socket_path(dir.path()).unwrap(),
            PathBuf::from("/tmp/re-test.sock")
        );
    }

    #[test]
    fn socket_path_rejects_paths_over_the_unix_limit() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!("/tmp/{}.sock", "x".repeat(MAX_SOCKET_PATH_LEN));
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            format!("[serve]\nsocket_path = \"{long}\"\n"),
        )
        .unwrap();

        let err = socket_path(dir.path()).unwrap_err();
        assert!(err.to_string().contains("too long"), "{err}");
    }

    #[test]
    fn spawn_lock_admits_one_winner_and_frees_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("graph.sock");

        let winner = acquire_spawn_lock(&socket).unwrap();
        assert!(winner.is_some());
        assert!(acquire_spawn_lock(&socket).unwrap().is_none());

        drop(winner);
        assert!(acquire_spawn_lock(&socket).unwrap().is_some());
    }

    #[test]
    fn stale_spawn_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("graph.sock");
        let lock = lock_path(&socket);

        std::fs::write(&lock, "1\n").unwrap();
        assert!(!lock_is_stale(&lock));

        let old = std::time::SystemTime::now() - STALE_LOCK_AGE - Duration::from_secs(60);
        set_mtime(&lock, old);
        assert!(lock_is_stale(&lock));
        assert!(acquire_spawn_lock(&socket).unwrap().is_some());
    }

    #[test]
    fn graph_mode_defaults_to_embedded_and_reads_server() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(graph_mode(dir.path()), "embedded");
        assert!(uses_daemon(dir.path()));

        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nmode = \"server\"\n",
        )
        .unwrap();
        assert_eq!(graph_mode(dir.path()), "server");
        assert!(!uses_daemon(dir.path()));
    }

    #[test]
    fn log_tail_reports_missing_and_present_logs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        assert!(log_tail(&path, 5).contains("no daemon log"));

        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let tail = log_tail(&path, 2);
        assert!(tail.contains("two") && tail.contains("three"));
        assert!(!tail.contains("one"));
    }

    #[test]
    fn the_derived_runtime_dir_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let derived = runtime_dir().unwrap();
        ensure_socket_dir(&derived).unwrap();

        let mode = std::fs::metadata(&derived).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// A `[serve] socket_path` can point anywhere, so its directory is only
    /// ever validated — never created, never chmod-ed.
    #[test]
    fn a_configured_socket_dir_is_validated_not_created() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("run");

        let err = ensure_socket_dir(&missing).unwrap_err();
        assert!(matches!(err, RecallError::Daemon(_)), "{err}");
        assert!(err.to_string().contains("socket directory"), "{err}");
        assert!(!missing.exists(), "the directory must not be created");

        // An existing directory only we can write into is accepted as-is.
        std::fs::create_dir(&missing).unwrap();
        std::fs::set_permissions(&missing, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_socket_dir(&missing).unwrap();
        let mode = std::fs::metadata(&missing).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "the directory must not be chmod-ed");
    }

    #[test]
    fn socket_dir_failure_is_a_named_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let err = ensure_socket_dir(&blocker.join("run")).unwrap_err();
        assert!(matches!(err, RecallError::Daemon(_)), "{err}");
        assert!(err.to_string().contains("socket directory"), "{err}");
    }

    #[tokio::test]
    async fn admin_lock_admits_one_holder_and_frees_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("graph.sock");

        assert!(!admin_lock_is_held(&socket));
        let held = acquire_admin_lock(&socket, Instant::now() + Duration::from_millis(50))
            .await
            .unwrap();
        assert!(admin_lock_is_held(&socket));

        let err = acquire_admin_lock(&socket, Instant::now() + Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("admin operation"), "{err}");

        drop(held);
        assert!(!admin_lock_is_held(&socket));
        acquire_admin_lock(&socket, Instant::now() + Duration::from_millis(50))
            .await
            .unwrap();
    }

    /// A client that died mid-operation must not block the store forever.
    #[tokio::test]
    async fn an_admin_lock_owned_by_a_dead_process_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("graph.sock");
        let path = admin_lock_path(&socket);

        // pid 0 never names a live process, so `/proc/0` never exists.
        std::fs::write(&path, "0\n").unwrap();
        assert!(!admin_lock_is_held(&socket));
        acquire_admin_lock(&socket, Instant::now() + Duration::from_millis(50))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_live_admin_lock_is_honored_by_waiters() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("graph.sock");
        let path = admin_lock_path(&socket);

        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert!(admin_lock_is_held(&socket));

        let err = wait_for_admin_lock(&socket, Instant::now() + Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, RecallError::Daemon(_)), "{err}");

        std::fs::remove_file(&path).unwrap();
        wait_for_admin_lock(&socket, Instant::now() + Duration::from_millis(50))
            .await
            .unwrap();
    }

    #[test]
    fn the_daemon_environment_is_an_allowlist() {
        assert!(DAEMON_ENV_ALLOWLIST.contains(&"PATH"));
        assert!(DAEMON_ENV_ALLOWLIST.contains(&DAEMON_BIN_ENV));
        for secret in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(
                !DAEMON_ENV_ALLOWLIST.contains(&secret),
                "{secret} must not reach the detached daemon"
            );
        }
    }

    /// Backdate a file's mtime without a libc dependency: rewrite it through a
    /// `File` whose times we set via `filetime`-free `set_times`.
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }
}
