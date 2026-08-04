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
//!   stop the daemon and take the store for themselves; the next hot operation
//!   starts a fresh daemon.

use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::error::RecallError;
use crate::graph::GraphMemory;
use crate::serve::{DaemonInfo, Request, Response};

/// Environment override for the binary used to start the daemon.
/// Defaults to the running executable; tests and wrappers set it explicitly.
pub const DAEMON_BIN_ENV: &str = "RECALL_ECHO_BIN";

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
/// Upper bound on connect → spawn → reconnect rounds.
const MAX_CONNECT_ROUNDS: u32 = 3;

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
        _ => runtime_dir().join(format!("{}.sock", path_hash(&canonical(memory_dir)))),
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

fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("recall-echo"),
        _ => PathBuf::from(format!("/tmp/recall-echo-{}", current_uid())),
    }
}

/// Owner uid, read from the filesystem so no libc dependency is needed.
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))))
        .map(|meta| meta.uid())
        .unwrap_or(0)
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

/// Create the socket directory, owner-only.
pub(crate) fn ensure_socket_dir(dir: &Path) -> Result<(), RecallError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).map_err(|err| {
        RecallError::Daemon(format!(
            "cannot create daemon socket directory {}: {err}",
            dir.display()
        ))
    })?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
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
    connection.call(request).await?.into_result()
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
/// Stops a running daemon, opens the store in-process, and runs `operation`.
/// The daemon is not restarted here — the next hot operation starts a fresh
/// one, so the store has exactly one owner at every instant.
pub async fn exclusive<T, F, Fut>(memory_dir: &Path, operation: F) -> Result<T, RecallError>
where
    F: FnOnce(GraphMemory) -> Fut,
    Fut: std::future::Future<Output = Result<T, RecallError>>,
{
    let graph_dir = memory_dir.join("graph");
    if !uses_daemon(memory_dir) {
        return operation(GraphMemory::open(&graph_dir).await?).await;
    }

    stop_daemon(memory_dir).await?;
    let graph = GraphMemory::open_embedded(&graph_dir).await?;
    operation(graph).await
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
        let _ = std::fs::remove_file(&socket);
        return Ok(false);
    };

    connection.call(&Request::Shutdown).await?.into_result()?;
    drop(connection);
    wait_for_socket_gone(&socket, Instant::now() + START_TIMEOUT).await?;
    Ok(true)
}

// ── Connection ───────────────────────────────────────────────────────────

/// One JSON-line conversation with a daemon.
struct Connection {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Connection {
    async fn connect(socket: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket).await?;
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

    async fn call(&mut self, request: &Request) -> Result<Response, RecallError> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;

        let mut response_line = String::new();
        let read = self.reader.read_line(&mut response_line).await?;
        if read == 0 {
            return Err(RecallError::Daemon(format!(
                "daemon closed the connection while handling `{}` — see the daemon log",
                request.op_name()
            )));
        }
        Ok(serde_json::from_str(&response_line)?)
    }

    async fn hello(&mut self) -> Result<DaemonInfo, RecallError> {
        let response = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.call(&Request::Hello))
            .await
            .map_err(|_| {
                RecallError::Daemon("daemon did not answer the version handshake within 10s".into())
            })??;
        let data = response.into_result()?;
        Ok(serde_json::from_value(data)?)
    }
}

/// Connect to the daemon for `socket`, starting one if needed.
async fn connect_or_spawn(memory_dir: &Path, socket: &Path) -> Result<Connection, RecallError> {
    let deadline = Instant::now() + START_TIMEOUT;

    for _ in 0..MAX_CONNECT_ROUNDS {
        match Connection::connect(socket).await {
            Ok(mut connection) => {
                let info = connection.hello().await?;
                if info.version == env!("CARGO_PKG_VERSION") {
                    return Ok(connection);
                }
                // Upgraded binary, stale daemon: ask it to go, then respawn.
                let _ = connection.call(&Request::Shutdown).await;
                drop(connection);
                wait_for_socket_gone(socket, deadline).await?;
            }
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
                let _ = std::fs::remove_file(socket);
            }
        }

        start_daemon(memory_dir, socket, deadline).await?;
    }

    Err(RecallError::Daemon(format!(
        "gave up connecting to the graph daemon on {} after {MAX_CONNECT_ROUNDS} attempts",
        socket.display()
    )))
}

/// Start a daemon — exactly one client wins the race; the rest wait.
async fn start_daemon(
    memory_dir: &Path,
    socket: &Path,
    deadline: Instant,
) -> Result<(), RecallError> {
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
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(file, "{}", std::process::id())
}

fn lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified.elapsed().unwrap_or_default() > STALE_LOCK_AGE)
        .unwrap_or(true)
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
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| {
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
            let _ = std::fs::remove_file(socket);
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
    fn socket_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("run");
        ensure_socket_dir(&socket_dir).unwrap();

        let mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
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

    /// Backdate a file's mtime without a libc dependency: rewrite it through a
    /// `File` whose times we set via `filetime`-free `set_times`.
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }
}
