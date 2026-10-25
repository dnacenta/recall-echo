// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `graph extract --pipeline v2 --rebuild` against a temp memory directory.
//!
//! The provider is scripted. The stopper is injected, so no daemon socket is
//! opened. Nothing here reads `~/.config/recall-echo` or a store outside the
//! temp dir.

#![cfg(all(feature = "llm", feature = "embedded"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use recall_echo::error::RecallError;
use recall_echo::graph::error::GraphError;
use recall_echo::graph::llm::LlmProvider;
use recall_echo::graph::store;
use recall_echo::graph::GraphMemory;
use recall_echo::graph::PipelineVersion;
use recall_echo::graph_cli::{rebuild, ExtractOptions, RebuildStop};
use sha2::{Digest, Sha256};

const RUST: &str = "Use Rust for the shelf service.";
const PYTHON: &str = "Use Python for the shelf service.";
const GO: &str = "Use Go for the shelf service.";
const ZIG: &str = "Use Zig for the shelf service.";

struct IdleStop {
    called: AtomicBool,
}

impl IdleStop {
    fn new() -> Self {
        Self {
            called: AtomicBool::new(false),
        }
    }

    fn called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

impl RebuildStop for IdleStop {
    fn stop<'a>(
        &'a self,
        _memory_dir: &'a Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, RecallError>> + Send + 'a>> {
        self.called.store(true, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(false)))
    }
}

struct FailStop;

impl RebuildStop for FailStop {
    fn stop<'a>(
        &'a self,
        _memory_dir: &'a Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, RecallError>> + Send + 'a>> {
        Box::pin(std::future::ready(Err(RecallError::Daemon(
            "still running".into(),
        ))))
    }
}

struct Scripted {
    fail_on: Option<&'static str>,
    seen: Mutex<Vec<String>>,
}

impl Scripted {
    fn answering() -> Self {
        Self {
            fail_on: None,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn failing(needle: &'static str) -> Self {
        Self {
            fail_on: Some(needle),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn saw(&self, needle: &str) -> bool {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .any(|message| message.contains(needle))
    }
}

#[async_trait]
impl LlmProvider for Scripted {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        _max_tokens: u32,
    ) -> Result<String, GraphError> {
        if system_prompt.starts_with("You link NEW MENTIONS") {
            return Ok(r#"{"entities":[]}"#.to_string());
        }
        self.seen
            .lock()
            .expect("seen")
            .push(user_message.to_string());
        if let Some(needle) = self.fail_on.filter(|needle| user_message.contains(needle)) {
            return Err(GraphError::Llm(format!("scripted failure: {needle}")));
        }
        let (object, quote) = match user_message {
            message if message.contains(RUST) => ("Rust", RUST),
            message if message.contains(PYTHON) => ("Python", PYTHON),
            message if message.contains(GO) => ("Go", GO),
            message if message.contains(ZIG) => ("Zig", ZIG),
            _ => {
                return Err(GraphError::Llm(format!(
                    "no scripted fact for {user_message}"
                )))
            }
        };
        Ok(format!(
            r#"{{"facts":[{{"subject":"shelf","subject_type":"project","predicate":"uses","role":"language","key":"","object":"{object}","object_type":"technology","status":"decided","replaces":"","turn":0,"quote":"{quote}","confidence":0.9}}],"bugs":[],"releases":[]}}"#
        ))
    }
}

fn options(pipeline: PipelineVersion, dry_run: bool) -> ExtractOptions {
    ExtractOptions {
        log: Some(99),
        all: true,
        dry_run,
        model: Some("ignored-model".into()),
        provider: Some("ignored-provider".into()),
        delay_ms: 250,
        max_tokens: 0,
        pipeline: Some(pipeline),
        rebuild: true,
    }
}

fn markdown(log: u32, sentence: &str) -> String {
    format!(
        "---\nlog: {log}\ndate: \"2026-03-01T00:00:00Z\"\nsession_id: \"s{log}\"\n---\n\n### User\n\n{sentence}\n"
    )
}

fn sidecar(log: u32, sentence: &str) -> String {
    format!(
        r#"{{"version":1,"session_id":"s{log}","date":"2026-03-01T00:00:00Z","cwd":null,"project":{{"name":"shelf","aliases":[]}},"turns":[{{"i":0,"role":"user","text":"{sentence}","ts":null}}],"tools":[]}}"#
    )
}

fn write_config(memory: &Path, server: bool) {
    let mode = if server { "server" } else { "embedded" };
    let run = memory.join("run");
    std::fs::create_dir_all(&run).expect("socket dir");
    let socket = run.join("graph.sock");
    std::fs::write(
        memory.join(".recall-echo.toml"),
        format!(
            "[llm]\nmodel = \"kept-model\"\nnum_ctx = 4096\n\n\
             [capture]\nenabled = false\n\n\
             [graph]\nmode = \"{mode}\"\n\n\
             [graph.provenance]\nweight_user = 0.5\n\n\
             [serve]\nsocket_path = \"{}\"\n",
            socket.display()
        ),
    )
    .expect("config");
}

fn write_archives(memory: &Path) {
    let conversations = memory.join("conversations");
    std::fs::create_dir_all(&conversations).expect("conversations");
    std::fs::write(conversations.join("conversation-001.md"), markdown(1, RUST))
        .expect("archive 1");
    std::fs::write(
        conversations.join("conversation-002.md"),
        markdown(2, PYTHON),
    )
    .expect("archive 2");
    std::fs::write(conversations.join("conversation-003.md"), markdown(3, GO)).expect("archive 3");
    let turns = memory.join("turns");
    std::fs::create_dir_all(&turns).expect("turns");
    std::fs::write(turns.join("conversation-001.json"), sidecar(1, RUST)).expect("sidecar 1");
    std::fs::write(turns.join("conversation-003.json"), sidecar(3, GO)).expect("sidecar 3");
}

async fn seed(memory: &Path, server: bool) -> BTreeMap<String, Vec<u8>> {
    write_archives(memory);
    write_config(memory, server);
    {
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .expect("init v1 store");
        drop(gm);
    }
    std::fs::write(memory.join("graph/KEEP"), b"keep").expect("sentinel");
    // SurrealKV compacts the WAL on the runtime after the connection drops.
    // A byte compare is only meaningful once that has finished.
    settled(&memory.join("graph")).await
}

async fn settled(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut previous = snapshot(root);
    let mut stable = 0u32;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let current = snapshot(root);
        if current == previous {
            stable += 1;
            if stable == 4 {
                return current;
            }
        } else {
            stable = 0;
            previous = current;
        }
    }
    panic!("store at {} did not settle", root.display());
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .map(|entry| entry.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let rel = path
            .strip_prefix(root)
            .expect("prefix")
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            out.insert(format!("{rel}/"), Vec::new());
            walk(root, &path, out);
        } else {
            let bytes =
                std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            out.insert(rel, bytes);
        }
    }
}

fn assert_unchanged(root: &Path, before: &BTreeMap<String, Vec<u8>>) {
    let after = snapshot(root);
    if after == *before {
        return;
    }
    let mut diffs = Vec::new();
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let left = before.get(key).map(Vec::len);
        let right = after.get(key).map(Vec::len);
        if before.get(key) != after.get(key) {
            diffs.push(format!(
                "{key} before={left:?} after={right:?} hash {} -> {}",
                before
                    .get(key)
                    .map(|bytes| digest_bytes(bytes))
                    .unwrap_or_default(),
                after
                    .get(key)
                    .map(|bytes| digest_bytes(bytes))
                    .unwrap_or_default()
            ));
        }
    }
    panic!("tree at {} changed: {diffs:#?}", root.display());
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn query(path: &Path, sql: &str) -> Vec<serde_json::Value> {
    let db = store::open(path).await.expect("open");
    let mut response = db.query(sql).await.expect("query");
    response.take(0).expect("rows")
}

fn strings(rows: &[serde_json::Value], field: &str) -> Vec<String> {
    rows.iter()
        .filter_map(|row| row.get(field).and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

fn backups(memory: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(memory)
        .expect("memory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("graph.v1-backup-"))
        })
        .collect();
    found.sort();
    found
}

async fn assert_swapped(memory: &Path) {
    assert!(!memory.join("graph.v2-build").exists());
    let graph = memory.join("graph");
    let stamp = strings(
        &query(&graph, "SELECT pipeline FROM meta:pipeline").await,
        "pipeline",
    );
    assert_eq!(stamp, vec!["v2".to_string()], "stamp {stamp:?}");

    let names = strings(&query(&graph, "SELECT name FROM entity").await, "name");
    for required in ["shelf", "Rust", "Python", "Go"] {
        assert!(names.iter().any(|name| name == required), "{names:?}");
    }
    let edges = query(
        &graph,
        "SELECT evidence FROM relates_to WHERE rel_type = 'uses'",
    )
    .await;
    assert_eq!(edges.len(), 3, "{edges:?}");
    for edge in &edges {
        let evidence = edge
            .get("evidence")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("evidence {edge}"));
        assert_eq!(evidence.len(), 1, "{edge}");
    }
    let episodes = query(&graph, "SELECT log_number, extracted FROM episode").await;
    assert_eq!(episodes.len(), 3, "{episodes:?}");
    assert!(
        episodes
            .iter()
            .all(|row| row.get("extracted") == Some(&serde_json::Value::Bool(true))),
        "{episodes:?}"
    );

    let found = backups(memory);
    assert_eq!(found.len(), 1, "{found:?}");
    let backup = &found[0];
    assert!(backup.join("KEEP").is_file(), "{}", backup.display());
    {
        let gm = GraphMemory::open_embedded(backup)
            .await
            .expect("v1 backup opens");
        let pending = gm.unextracted_log_numbers().await.expect("v1 scan");
        assert!(pending.is_empty(), "{pending:?}");
    }
    let backup_stamp = strings(
        &query(backup, "SELECT pipeline FROM meta:pipeline").await,
        "pipeline",
    );
    assert!(backup_stamp.is_empty(), "{backup_stamp:?}");

    let cfg = recall_echo::config::load(memory);
    assert_eq!(cfg.llm.model, "kept-model");
    assert_eq!(cfg.llm.num_ctx, 4096);
    assert!(!cfg.capture.enabled);
    assert_eq!(
        cfg.graph
            .as_ref()
            .and_then(|graph| graph.pipeline.as_deref()),
        Some("v2")
    );
    let weight = cfg.graph.as_ref().expect("graph").provenance.weight_user;
    assert!((weight - 0.5).abs() < f64::EPSILON, "{weight}");
}

#[tokio::test(flavor = "current_thread")]
async fn rebuild_swaps_and_keeps_backup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    seed(memory, false).await;
    let stop = IdleStop::new();
    rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect("rebuild");
    assert!(stop.called());
    assert_swapped(memory).await;
}

#[tokio::test(flavor = "current_thread")]
async fn failure_midway_keeps_graph() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before = seed(memory, false).await;
    let provider = Scripted::failing(PYTHON);
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&provider),
    )
    .await
    .expect_err("second archive fails");
    let message = err.to_string();
    assert!(message.contains("log 002"), "{message}");
    assert!(message.contains("was not modified"), "{message}");
    assert!(message.contains("graph.v2-build"), "{message}");
    assert_unchanged(&memory.join("graph"), &before);
    assert!(memory.join("graph/KEEP").is_file());
    assert!(memory.join("graph.v2-build/surreal").is_dir());
    assert!(provider.saw(RUST));
    assert!(provider.saw(PYTHON));
    assert!(!provider.saw(GO));
    let cfg = recall_echo::config::load(memory);
    assert!(cfg
        .graph
        .as_ref()
        .and_then(|graph| graph.pipeline.as_deref())
        .is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn existing_build_dir_resumes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before = seed(memory, false).await;
    let first = Scripted::failing(PYTHON);
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&first),
    )
    .await
    .expect_err("stop on the second archive");
    assert!(err.to_string().contains("log 002"), "{err}");
    assert_unchanged(&memory.join("graph"), &before);

    let second = Scripted::failing(RUST);
    rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&second),
    )
    .await
    .expect("resume");
    assert!(
        !second.saw(RUST),
        "resumed rebuild re-sent an extracted archive"
    );
    assert!(second.saw(PYTHON));
    assert!(second.saw(GO));
    assert_swapped(memory).await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_mode_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before_graph = seed(memory, true).await;
    let before = snapshot(memory);
    let stop = IdleStop::new();
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("server mode");
    let message = err.to_string();
    assert!(message.contains("server"), "{message}");
    assert!(message.contains("nothing was changed"), "{message}");
    assert!(!stop.called());
    assert_unchanged(memory, &before);
    assert_unchanged(&memory.join("graph"), &before_graph);
    assert!(!memory.join("graph.v2-build").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn dry_run_writes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    seed(memory, false).await;
    let before = snapshot(memory);
    let stop = IdleStop::new();
    rebuild(
        memory,
        options(PipelineVersion::V2, true),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect("dry run");
    assert!(!stop.called());
    assert_eq!(snapshot(memory), before);
    assert!(!memory.join("graph.v2-build").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn pipeline_v1_is_refused_before_anything_is_touched() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before_graph = seed(memory, false).await;
    let before = snapshot(memory);
    let stop = IdleStop::new();
    let err = rebuild(
        memory,
        options(PipelineVersion::V1, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("v1");
    assert!(err.to_string().contains("pipeline v2"), "{err}");
    assert!(!stop.called());
    assert_unchanged(memory, &before);
    assert_unchanged(&memory.join("graph"), &before_graph);
}

#[tokio::test(flavor = "current_thread")]
async fn a_daemon_that_cannot_be_stopped_leaves_the_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before_graph = seed(memory, false).await;
    let before = snapshot(memory);
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &FailStop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("daemon");
    let message = err.to_string();
    assert!(message.contains("cannot stop"), "{message}");
    assert_unchanged(memory, &before);
    assert_unchanged(&memory.join("graph"), &before_graph);
    assert!(!memory.join("graph.v2-build").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn an_unstamped_build_directory_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before_graph = seed(memory, false).await;
    let build = memory.join("graph.v2-build");
    std::fs::create_dir(&build).expect("empty build dir");
    let stop = IdleStop::new();
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("unstamped dir");
    let message = err.to_string();
    assert!(message.contains("rm -r"), "{message}");
    assert!(message.contains(&build.display().to_string()), "{message}");
    assert!(message.contains("stamped v2"), "{message}");
    assert!(!stop.called());
    assert_unchanged(&memory.join("graph"), &before_graph);
    assert!(!build.join("surreal").exists());

    {
        let _store = GraphMemory::open_embedded(&build)
            .await
            .expect("unstamped store");
    }
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("unstamped store");
    let message = err.to_string();
    assert!(message.contains("rm -r"), "{message}");
    assert!(!stop.called());
    assert_unchanged(&memory.join("graph"), &before_graph);
}

#[tokio::test(flavor = "current_thread")]
async fn a_concurrent_exclusive_blocks_and_a_mid_build_log_is_in_the_new_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before = seed(memory, false).await;
    let stop = IdleStop::new();
    let probe = LockProbe {
        inner: Scripted::answering(),
        memory: memory.to_path_buf(),
        blocked: Mutex::new(None),
    };
    let mut opts = options(PipelineVersion::V2, false);
    opts.delay_ms = 0;
    rebuild(memory, opts, &stop, Some(&probe))
        .await
        .expect("rebuild");
    assert!(stop.called());
    let outcome = probe
        .blocked
        .lock()
        .expect("blocked")
        .clone()
        .expect("probe ran");
    assert_eq!(outcome, "blocked", "{outcome}");

    let backup = backups(memory);
    assert_eq!(backup.len(), 1, "{backup:?}");
    assert_eq!(
        snapshot(&backup[0]),
        before,
        "backup changed during the build"
    );
    assert!(probe.inner.saw(ZIG), "mid-build log was not extracted");
    let episodes = query(
        &memory.join("graph"),
        "SELECT log_number, extracted FROM episode",
    )
    .await;
    assert!(
        episodes
            .iter()
            .any(|row| log_number(row) == Some(4) && extracted(row)),
        "{episodes:?}"
    );
}

struct LockProbe {
    inner: Scripted,
    memory: PathBuf,
    blocked: Mutex<Option<String>>,
}

#[async_trait]
impl LlmProvider for LockProbe {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError> {
        if !system_prompt.starts_with("You link NEW MENTIONS")
            && self.blocked.lock().expect("blocked").is_none()
        {
            std::fs::write(
                self.memory.join("conversations/conversation-004.md"),
                markdown(4, ZIG),
            )
            .expect("mid-build log");
            let memory = self.memory.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                let outcome = runtime.block_on(async {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        recall_echo::serve_client::exclusive(&memory, |_graph| async {
                            Ok::<(), RecallError>(())
                        }),
                    )
                    .await
                    {
                        Err(_) => "blocked".to_string(),
                        Ok(Ok(())) => "acquired".to_string(),
                        Ok(Err(err)) => format!("finished: {err}"),
                    }
                });
                let _ = tx.send(outcome);
            });
            let outcome = tokio::task::spawn_blocking(move || {
                rx.recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap_or_else(|err| format!("probe did not finish: {err}"))
            })
            .await
            .expect("probe join");
            *self.blocked.lock().expect("blocked") = Some(outcome);
        }
        self.inner
            .complete(system_prompt, user_message, max_tokens)
            .await
    }
}

fn log_number(row: &serde_json::Value) -> Option<u64> {
    row.get("log_number")
        .and_then(|value| value.as_u64().or_else(|| value.as_i64().map(|n| n as u64)))
}

fn extracted(row: &serde_json::Value) -> bool {
    row.get("extracted") == Some(&serde_json::Value::Bool(true))
}

#[tokio::test(flavor = "current_thread")]
async fn a_half_finished_swap_is_refused_with_the_commands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    seed(memory, false).await;
    rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&Scripted::answering()),
    )
    .await
    .expect("rebuild");
    let backup = backups(memory).remove(0);
    let graph = memory.join("graph");
    let build = memory.join("graph.v2-build");
    std::fs::rename(&graph, &build).expect("move graph back to the build name");
    assert!(!graph.exists());

    let stop = IdleStop::new();
    for dry_run in [true, false] {
        let err = rebuild(
            memory,
            options(PipelineVersion::V2, dry_run),
            &stop,
            Some(&Scripted::answering()),
        )
        .await
        .expect_err("half swap");
        let message = err.to_string();
        assert!(
            message.contains(&format!("mv '{}' '{}'", build.display(), graph.display())),
            "{message}"
        );
        assert!(
            message.contains(&format!("mv '{}' '{}'", backup.display(), graph.display())),
            "{message}"
        );
        assert!(message.contains("pipeline = \"v2\""), "{message}");
        assert!(message.contains("pipeline = \"v1\""), "{message}");
        assert!(
            message.contains("nothing was finished automatically"),
            "{message}"
        );
        assert!(!stop.called(), "dry_run={dry_run}");
        assert!(!graph.exists());
        assert!(build.is_dir());
        assert!(backup.is_dir());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_zero_context_and_server_mode_refuse_before_the_daemon_stops() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before_graph = seed(memory, false).await;
    let socket = memory.join("run/graph.sock");
    std::fs::write(
        memory.join(".recall-echo.toml"),
        format!(
            "[llm]\nnum_ctx = 0\nmodel = \"kept-model\"\n\n\
             [capture]\nenabled = false\n\n\
             [graph]\nmode = \"server\"\n\n\
             [graph.provenance]\nweight_user = 0.5\n\n\
             [serve]\nsocket_path = \"{}\"\n",
            socket.display()
        ),
    )
    .expect("bad config");
    let before = snapshot(memory);
    let stop = IdleStop::new();
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("invalid config");
    let message = err.to_string();
    assert!(message.contains("server"), "{message}");
    assert!(message.contains("nothing was changed"), "{message}");
    assert!(message.contains("num_ctx"), "{message}");
    assert!(message.contains("refusing to rewrite"), "{message}");
    assert!(!stop.called());
    assert_unchanged(memory, &before);
    assert_unchanged(&memory.join("graph"), &before_graph);
    assert!(!memory.join("graph.v2-build").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn a_short_episode_count_is_filled_on_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    write_config(memory, false);
    {
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .expect("v1 store");
        drop(gm);
    }
    std::fs::write(memory.join("graph/KEEP"), b"keep").expect("sentinel");
    let conversations = memory.join("conversations");
    std::fs::create_dir_all(&conversations).expect("conversations");
    let paragraph = "Use Rust for the shelf service. ".repeat(80);
    let text = format!(
        "---\nlog: 1\ndate: \"2026-03-01T00:00:00Z\"\nsession_id: \"s1\"\n---\n\n### User\n\n{paragraph}\n---\n\n### User\n\n{paragraph}\n"
    );
    std::fs::write(conversations.join("conversation-001.md"), &text).expect("archive");
    let chunks = recall_echo::graph::extract::chunk_conversation(&text, 500);
    assert!(chunks.len() >= 2, "{}", chunks.len());
    let build = memory.join("graph.v2-build");
    {
        let gm = GraphMemory::open_embedded(&build).await.expect("build");
        gm.add_episode(recall_echo::graph::types::NewEpisode {
            session_id: "s1".into(),
            abstract_text: "partial".into(),
            overview: None,
            content: Some("partial".into()),
            log_number: Some(1),
        })
        .await
        .expect("one episode");
    }
    {
        let db = store::open(&build).await.expect("stamp");
        db.query("UPSERT meta:pipeline SET pipeline = $pipeline")
            .bind(("pipeline", "v2"))
            .await
            .expect("upsert")
            .check()
            .expect("stamp check");
    }
    rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&Scripted::answering()),
    )
    .await
    .expect("resume fills the short log");
    let episodes = query(
        &memory.join("graph"),
        "SELECT log_number, extracted FROM episode",
    )
    .await;
    let mine: Vec<_> = episodes
        .iter()
        .filter(|row| log_number(row) == Some(1))
        .collect();
    assert_eq!(mine.len(), chunks.len(), "{episodes:?}");
    assert!(mine.iter().all(|row| extracted(row)), "{mine:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_zero_chunk_log_is_not_extracted_again_on_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    write_config(memory, false);
    {
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .expect("v1 store");
        drop(gm);
    }
    let conversations = memory.join("conversations");
    std::fs::create_dir_all(&conversations).expect("conversations");
    std::fs::write(conversations.join("conversation-001.md"), markdown(1, RUST)).expect("log 1");
    std::fs::write(conversations.join("conversation-002.md"), "\n").expect("empty log");
    std::fs::write(
        conversations.join("conversation-003.md"),
        markdown(3, PYTHON),
    )
    .expect("log 3");
    let turns = memory.join("turns");
    std::fs::create_dir_all(&turns).expect("turns");
    std::fs::write(turns.join("conversation-001.json"), sidecar(1, RUST)).expect("sidecar 1");
    std::fs::write(turns.join("conversation-002.json"), sidecar(2, ZIG)).expect("sidecar 2");

    let first = Scripted::failing(PYTHON);
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&first),
    )
    .await
    .expect_err("later log fails");
    assert!(err.to_string().contains("log 003"), "{err}");
    assert!(first.saw(RUST));
    assert!(first.saw(PYTHON));
    assert!(!first.saw(ZIG), "zero-chunk log was sent to the provider");

    let second = Scripted::failing(PYTHON);
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &IdleStop::new(),
        Some(&second),
    )
    .await
    .expect_err("resume still fails the later log");
    assert!(err.to_string().contains("log 003"), "{err}");
    assert!(!second.saw(RUST), "extracted log was sent again");
    assert!(second.saw(PYTHON));
    assert!(!second.saw(ZIG), "zero-chunk log was sent again on resume");
}

#[tokio::test(flavor = "current_thread")]
async fn an_unrelated_surreal_directory_is_not_opened() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = dir.path();
    let before_graph = seed(memory, false).await;
    let surreal = memory.join("graph.v2-build/surreal");
    std::fs::create_dir_all(&surreal).expect("surreal");
    std::fs::write(surreal.join("note.txt"), b"unrelated").expect("note");
    let stop = IdleStop::new();
    let err = rebuild(
        memory,
        options(PipelineVersion::V2, false),
        &stop,
        Some(&Scripted::answering()),
    )
    .await
    .expect_err("not a store");
    let message = err.to_string();
    assert!(message.contains("rm -r"), "{message}");
    assert!(!stop.called());
    let mut names: Vec<_> = std::fs::read_dir(&surreal)
        .expect("surreal")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    names.sort();
    assert_eq!(names, vec![std::ffi::OsString::from("note.txt")]);
    assert_unchanged(&memory.join("graph"), &before_graph);
}
