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
