//! Client ↔ daemon integration: auto-start, stale sockets, spawn races,
//! version mismatch, and named failures.
//!
//! These tests start real daemons against throwaway temp directories. They
//! only use operations that never embed (status, hello), so nothing here
//! touches the network.

use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use recall_echo::error::RecallError;
use recall_echo::serve::{DaemonInfo, Request, Response};
use recall_echo::serve_client;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

static BIN_ENV: Once = Once::new();

/// Point the client at the binary cargo just built, instead of the test
/// harness executable that `current_exe()` would return.
fn use_test_binary() {
    BIN_ENV.call_once(|| {
        std::env::set_var(
            serve_client::DAEMON_BIN_ENV,
            env!("CARGO_BIN_EXE_recall-echo"),
        );
    });
}

struct Fixture {
    _dir: TempDir,
    entity_root: PathBuf,
    memory_dir: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_config("")
    }

    /// A memory directory with its own socket, plus optional extra config.
    fn with_config(extra: &str) -> Self {
        use_test_binary();

        let dir = TempDir::new().expect("temp dir");
        let entity_root = dir.path().join("e");
        let memory_dir = entity_root.join("memory");
        std::fs::create_dir_all(memory_dir.join("graph")).expect("memory dir");

        let socket = dir.path().join("g.sock");
        std::fs::write(
            memory_dir.join(".recall-echo.toml"),
            format!(
                "[serve]\nsocket_path = \"{}\"\nidle_timeout_secs = 120\n{extra}",
                socket.display()
            ),
        )
        .expect("write config");

        Self {
            _dir: dir,
            entity_root,
            memory_dir,
            socket,
        }
    }

    async fn status(&self) -> Result<serde_json::Value, RecallError> {
        serve_client::execute(&self.memory_dir, &Request::Status).await
    }

    async fn info(&self) -> Option<DaemonInfo> {
        serve_client::daemon_info(&self.memory_dir).await.unwrap()
    }

    /// Stop the daemon, asserting the request was actually honored.
    async fn stop(&self) {
        serve_client::stop_daemon(&self.memory_dir)
            .await
            .expect("daemon stops when asked");
    }

    /// Run `recall-echo graph <args>` against this fixture's entity root.
    fn graph_cli(&self, args: &[&str]) -> std::process::Child {
        std::process::Command::new(env!("CARGO_BIN_EXE_recall-echo"))
            .arg("graph")
            .arg("--entity-root")
            .arg(&self.entity_root)
            .args(args)
            .env(
                serve_client::DAEMON_BIN_ENV,
                env!("CARGO_BIN_EXE_recall-echo"),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn recall-echo")
    }
}

#[tokio::test]
async fn first_request_starts_a_daemon_and_completes() {
    let fixture = Fixture::new();
    assert!(fixture.info().await.is_none(), "no daemon before first use");

    let stats = fixture.status().await.expect("status through daemon");
    assert_eq!(stats["entity_count"], 0);
    assert_eq!(stats["episode_count"], 0);

    let info = fixture.info().await.expect("daemon running");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert!(info.pid > 0);
    assert!(fixture.socket.exists());

    fixture.stop().await;
    assert!(fixture.info().await.is_none(), "daemon stopped");
}

#[tokio::test]
async fn shutdown_request_stops_the_daemon_promptly() {
    let fixture = Fixture::new();
    fixture.status().await.expect("start the daemon");

    let started = std::time::Instant::now();
    assert!(
        serve_client::stop_daemon(&fixture.memory_dir)
            .await
            .expect("stop succeeds"),
        "a daemon was running"
    );

    // Regression: a lost shutdown wakeup left the daemon alive until its idle
    // timeout, and the client waited out the full stop timeout.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stopping took {:?}",
        started.elapsed()
    );
    assert!(!fixture.socket.exists(), "socket cleaned up on exit");
}

#[tokio::test]
async fn concurrent_spawn_race_has_a_single_winner() {
    let fixture = Fixture::new();

    let (a, b, c, d) = tokio::join!(
        fixture.status(),
        fixture.status(),
        fixture.status(),
        fixture.status()
    );
    for result in [a, b, c, d] {
        result.expect("every racing client completes");
    }

    let info = fixture.info().await.expect("exactly one daemon survived");
    assert!(info.uptime_secs < 120);

    fixture.stop().await;
}

#[tokio::test]
async fn client_cleans_stale_socket_and_respawns() {
    let fixture = Fixture::new();

    // A socket file with nobody listening — what a crashed daemon leaves behind.
    let listener = std::os::unix::net::UnixListener::bind(&fixture.socket).expect("bind");
    drop(listener);
    assert!(fixture.socket.exists());

    fixture.status().await.expect("stale socket is cleaned up");
    assert!(fixture.info().await.is_some(), "fresh daemon took over");

    fixture.stop().await;
}

#[tokio::test]
async fn killed_daemon_is_replaced_on_the_next_request() {
    let fixture = Fixture::new();
    fixture.status().await.expect("first status");
    let first = fixture.info().await.expect("daemon running");

    let killed = std::process::Command::new("kill")
        .args(["-9", &first.pid.to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    wait_until_gone(&fixture.socket).await;

    fixture.status().await.expect("status after kill -9");
    let second = fixture.info().await.expect("replacement daemon");
    assert_ne!(first.pid, second.pid);

    fixture.stop().await;
}

#[tokio::test]
async fn version_mismatch_replaces_the_running_daemon() {
    let fixture = Fixture::new();
    let socket = fixture.socket.clone();

    // A daemon from an older build: answers Hello with a foreign version and
    // exits when asked to shut down.
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind mock");
    let mock_socket = socket.clone();
    let mock = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let request: Request = serde_json::from_str(&line).expect("parse request");
            let response = match request {
                Request::Hello => Response::success(serde_json::json!({
                    "version": "0.0.0-ancient",
                    "pid": 1,
                    "memory_dir": "",
                    "socket_path": "",
                    "uptime_secs": 1,
                })),
                _ => Response::success(serde_json::json!({ "stopping": true })),
            };
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
            writer.flush().await.unwrap();
            if !matches!(request, Request::Hello) {
                break;
            }
        }
        drop(listener);
        let _ = std::fs::remove_file(&mock_socket);
    });

    let stats = fixture.status().await.expect("status after version swap");
    assert_eq!(stats["entity_count"], 0);

    let info = fixture
        .info()
        .await
        .expect("current-version daemon running");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));

    mock.await.expect("mock daemon exits");
    fixture.stop().await;
}

#[tokio::test]
async fn unstartable_daemon_reports_a_named_error() {
    let dir = TempDir::new().expect("temp dir");
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(memory_dir.join("graph")).expect("memory dir");

    // The socket's parent is a regular file, so the directory can never exist.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").expect("write blocker");
    std::fs::write(
        memory_dir.join(".recall-echo.toml"),
        format!("[serve]\nsocket_path = \"{}/g.sock\"\n", blocker.display()),
    )
    .expect("write config");
    use_test_binary();

    let err = serve_client::execute(&memory_dir, &Request::Status)
        .await
        .expect_err("daemon cannot start");
    assert!(matches!(err, RecallError::Daemon(_)), "{err}");
    assert!(err.to_string().contains("socket directory"), "{err}");
}

#[tokio::test]
async fn server_mode_bypasses_the_daemon() {
    let fixture = Fixture::with_config("\n[graph]\nmode = \"server\"\n");

    assert_eq!(serve_client::graph_mode(&fixture.memory_dir), "server");
    assert!(
        fixture.info().await.is_none(),
        "server mode never consults a daemon"
    );
    assert!(!serve_client::stop_daemon(&fixture.memory_dir)
        .await
        .expect("stop is a no-op in server mode"),);
    assert!(!fixture.socket.exists(), "no socket is created");
}

#[tokio::test]
async fn two_concurrent_cli_invocations_share_one_daemon() {
    let fixture = Fixture::new();

    let first = fixture.graph_cli(&["status"]);
    let second = fixture.graph_cli(&["status"]);

    for child in [first, second] {
        let output = child.wait_with_output().expect("cli finished");
        assert!(
            output.status.success(),
            "concurrent `graph status` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert!(
        fixture.info().await.is_some(),
        "one shared daemon is still serving"
    );
    fixture.stop().await;
}

/// `exclusive` must actually be exclusive: the daemon that owned the store is
/// gone for the whole operation, and the next hot operation starts a new one.
#[tokio::test]
async fn exclusive_takes_the_store_from_the_daemon_and_hands_it_back() {
    let fixture = Fixture::new();
    fixture.status().await.expect("start the daemon");
    let before = fixture.info().await.expect("daemon running").pid;

    let seen_inside = serve_client::exclusive(&fixture.memory_dir, |_graph| async {
        // No daemon may be serving while an admin operation owns the store.
        Ok(serve_client::daemon_info(&fixture.memory_dir)
            .await
            .unwrap())
    })
    .await
    .expect("exclusive operation runs");
    assert!(seen_inside.is_none(), "the daemon must be stopped");
    assert!(!fixture.socket.exists(), "the socket is gone with it");

    fixture.status().await.expect("hot op respawns the daemon");
    let after = fixture.info().await.expect("replacement daemon").pid;
    assert_ne!(before, after);

    fixture.stop().await;
}

/// A hot operation that arrives while an admin operation owns the store waits
/// for it instead of starting a daemon that would collide with it.
#[tokio::test]
async fn a_hot_operation_waits_for_a_running_admin_operation() {
    let fixture = Fixture::new();

    let memory_dir = fixture.memory_dir.clone();
    let admin = tokio::spawn(async move {
        serve_client::exclusive(&memory_dir, |_graph| async {
            tokio::time::sleep(Duration::from_millis(750)).await;
            Ok(())
        })
        .await
    });

    // Give the admin operation time to take the lock, then race it.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = std::time::Instant::now();
    fixture
        .status()
        .await
        .expect("hot op completes after waiting");

    admin.await.expect("join").expect("admin operation");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "the hot operation did not wait for the admin lock ({:?})",
        started.elapsed()
    );

    fixture.stop().await;
}

/// A daemon that goes away between `connect` and the handshake — an idle
/// timeout, or another process's `exclusive` — must not fail the caller. On
/// the SessionEnd hook path that would silently lose a conversation ingest.
#[tokio::test]
async fn a_daemon_that_dies_mid_handshake_is_replaced() {
    let fixture = Fixture::new();

    let listener = tokio::net::UnixListener::bind(&fixture.socket).expect("bind mock");
    let mock = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        // Closing without answering is exactly what an exiting daemon looks like.
        drop(stream);
        drop(listener);
    });

    let stats = fixture
        .status()
        .await
        .expect("status despite the dead daemon");
    assert_eq!(stats["entity_count"], 0);
    assert!(fixture.info().await.is_some(), "a fresh daemon took over");

    mock.await.expect("mock daemon exits");
    fixture.stop().await;
}

/// An oversized request is rejected by code, and the daemon keeps serving.
#[tokio::test]
async fn an_oversized_request_is_refused_and_the_daemon_survives() {
    let fixture = Fixture::new();
    fixture.status().await.expect("start the daemon");
    let before = fixture.info().await.expect("daemon running").pid;

    let stream = tokio::net::UnixStream::connect(&fixture.socket)
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();

    // 9 MiB of a syntactically valid request, one MiB past the cap.
    let opening = br#"{"op":"search","args":{"limit":1,"query":""#;
    writer.write_all(opening).await.expect("write opening");
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..(9 * 16) {
        if writer.write_all(&chunk).await.is_err() {
            break;
        }
    }
    let _ = writer.write_all(b"\"}}\n").await;
    let _ = writer.flush().await;

    let mut response = String::new();
    BufReader::new(reader)
        .read_line(&mut response)
        .await
        .expect("read response");
    let parsed: Response = serde_json::from_str(&response).expect("parse response");
    assert!(!parsed.ok, "{response}");
    assert_eq!(parsed.error.expect("error").code, "bad_request");

    let after = fixture.info().await.expect("daemon still serving").pid;
    assert_eq!(
        before, after,
        "the daemon must survive an oversized request"
    );
    fixture.status().await.expect("still answering requests");

    fixture.stop().await;
}

/// The daemon socket is owner-only and only the owning uid may use it.
#[tokio::test]
async fn the_socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.status().await.expect("start the daemon");

    let mode = std::fs::metadata(&fixture.socket)
        .expect("socket exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");

    let pidfile = PathBuf::from(format!("{}.pid", fixture.socket.display()));
    let mode = std::fs::metadata(&pidfile)
        .expect("pidfile exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "pidfile mode {mode:o}");

    fixture.stop().await;
}

/// A `[serve] socket_path` must not make recall-echo unlink arbitrary files.
#[tokio::test]
async fn a_configured_socket_path_pointing_at_a_regular_file_is_refused() {
    use_test_binary();

    let dir = TempDir::new().expect("temp dir");
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(memory_dir.join("graph")).expect("memory dir");

    let precious = dir.path().join("precious.toml");
    std::fs::write(&precious, b"keep me").expect("write file");
    std::fs::write(
        memory_dir.join(".recall-echo.toml"),
        format!("[serve]\nsocket_path = \"{}\"\n", precious.display()),
    )
    .expect("write config");

    let err = serve_client::execute(&memory_dir, &Request::Status)
        .await
        .expect_err("daemon cannot use a regular file as its socket");
    assert!(matches!(err, RecallError::Daemon(_)), "{err}");
    assert!(precious.exists(), "the file must survive");
    assert_eq!(
        std::fs::read_to_string(&precious).unwrap(),
        "keep me",
        "the file must be untouched"
    );
}

/// Poll until nothing is listening on `socket`.
async fn wait_until_gone(socket: &std::path::Path) {
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(socket).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("socket {} still accepting connections", socket.display());
}
