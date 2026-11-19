// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A forgotten fact is skipped on rebuild, resume, and extract after ingest.
//!
//! The provider is scripted. Stores are temporary. Nothing here calls a paid
//! provider or opens a network connection.

#![cfg(all(feature = "llm", feature = "embedded"))]

use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use recall_echo::error::RecallError;
use recall_echo::graph::error::GraphError;
use recall_echo::graph::llm::LlmProvider;
use recall_echo::graph::store;
use recall_echo::graph::GraphMemory;
use recall_echo::graph::PipelineVersion;
use recall_echo::graph_cli::{rebuild, ExtractOptions, RebuildStop};
use recall_echo::serve_client;

const KEPT: &str = "the port is 1000.";
const FORGOTTEN: &str = "the port is 9000. the port is 9001.";

struct IdleStop;

impl RebuildStop for IdleStop {
    fn stop<'a>(
        &'a self,
        _memory_dir: &'a Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, RecallError>> + Send + 'a>> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

struct Scripted {
    fail_on: Option<&'static str>,
}

#[async_trait]
impl LlmProvider for Scripted {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        _: u32,
    ) -> Result<String, GraphError> {
        if system_prompt.starts_with("You link NEW MENTIONS") {
            return Ok(r#"{"entities":[]}"#.to_string());
        }
        if system_prompt.starts_with("You compare stored") {
            return Ok(r#"{"pairs":[]}"#.to_string());
        }
        if self
            .fail_on
            .is_some_and(|needle| user_message.contains(needle))
        {
            return Err(GraphError::Llm("scripted failure".into()));
        }
        if user_message.contains("the port is 1000") {
            return Ok(one_fact("1000", "the port is 1000"));
        }
        if user_message.contains("the port is 9000") {
            return Ok(two_facts());
        }
        Ok(r#"{"facts":[],"bugs":[],"releases":[]}"#.to_string())
    }
}

fn one_fact(value: &str, quote: &str) -> String {
    format!(
        r#"{{"facts":[{{"subject":"shelf","subject_type":"project","predicate":"setting","role":"none","key":"port","object":"{value}","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"{quote}","confidence":0.9}}],"bugs":[],"releases":[]}}"#
    )
}

fn two_facts() -> String {
    r#"{"facts":[
        {"subject":"shelf","subject_type":"project","predicate":"setting","role":"none","key":"port","object":"9000","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"the port is 9000","confidence":0.9},
        {"subject":"shelf","subject_type":"project","predicate":"setting","role":"none","key":"port","object":"9001","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"the port is 9001","confidence":0.9}
    ],"bugs":[],"releases":[]}"#
        .to_string()
}

fn options() -> ExtractOptions {
    ExtractOptions {
        log: None,
        all: true,
        dry_run: false,
        model: None,
        provider: None,
        delay_ms: 0,
        max_tokens: 0,
        pipeline: Some(PipelineVersion::V2),
        rebuild: true,
        force: false,
    }
}

fn write_config(memory: &Path) {
    let run = memory.join("run");
    std::fs::create_dir_all(&run).expect("socket dir");
    let socket = run.join("graph.sock");
    std::fs::write(
        memory.join(".recall-echo.toml"),
        format!(
            "[capture]\nenabled = false\n\n[extraction]\nbackground_enabled = false\n\n\
             [graph]\nmode = \"embedded\"\nlasting_forget = true\n\
             ground_component_edges = false\n\n[serve]\nsocket_path = \"{}\"\n",
            socket.display()
        ),
    )
    .expect("config");
}

fn write_forget(memory: &Path) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(memory.join("forgotten.jsonl"))
        .expect("forget file");
    writeln!(
        file,
        r#"{{"v":1,"status":"active","kind":"fact","subject":"shelf","relation":"port","value":"9000","forgotten_at":"2026-11-17T12:00:00Z","provenance":"user"}}"#
    )
    .expect("record");
}

fn write_log(memory: &Path, log: u32, sentence: &str) {
    let conversations = memory.join("conversations");
    std::fs::create_dir_all(&conversations).expect("conversations");
    std::fs::write(
        conversations.join(format!("conversation-{log:03}.md")),
        format!(
            "---\nlog: {log}\ndate: \"2026-03-01T00:00:00Z\"\nsession_id: \"s{log}\"\n---\n\n### User\n\n{sentence}\n"
        ),
    )
    .expect("archive");
    let turns = memory.join("turns");
    std::fs::create_dir_all(&turns).expect("turns");
    std::fs::write(
        turns.join(format!("conversation-{log:03}.json")),
        format!(
            r#"{{"version":1,"session_id":"s{log}","date":"2026-03-01T00:00:00Z","cwd":null,"project":{{"name":"shelf","aliases":[]}},"turns":[{{"i":0,"role":"user","text":"{sentence}","ts":null}}],"tools":[]}}"#
        ),
    )
    .expect("sidecar");
}

async fn init_graph(memory: &Path) {
    GraphMemory::open_embedded(&memory.join("graph"))
        .await
        .expect("init");
}

async fn slot_values(graph: &Path) -> Vec<String> {
    let db = store::open(graph).await.expect("open");
    let mut response = db
        .query("SELECT attributes FROM entity")
        .await
        .expect("query");
    let rows: Vec<serde_json::Value> = response.take(0).expect("rows");
    let mut values = Vec::new();
    for row in rows {
        let Some(slots) = row
            .get("attributes")
            .and_then(|attrs| attrs.get("v2"))
            .and_then(|v2| v2.get("slots"))
            .and_then(|slots| slots.as_array())
        else {
            continue;
        };
        for slot in slots {
            if let Some(value) = slot.get("value").and_then(|value| value.as_str()) {
                values.push(value.to_string());
            }
        }
    }
    values.sort();
    values
}

#[tokio::test(flavor = "current_thread")]
async fn rebuild_counts_a_forgotten_skip() {
    let dir = tempfile::tempdir().expect("temp");
    let memory = dir.path();
    write_config(memory);
    write_forget(memory);
    write_log(memory, 1, FORGOTTEN);
    init_graph(memory).await;

    let skips = rebuild(
        memory,
        options(),
        &IdleStop,
        Some(&Scripted { fail_on: None }),
    )
    .await
    .expect("rebuild");
    assert_eq!(skips, 1);
    let values = slot_values(&memory.join("graph")).await;
    assert!(values.contains(&"9001".to_string()), "{values:?}");
    assert!(!values.iter().any(|value| value == "9000"), "{values:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn resume_counts_a_forgotten_skip() {
    let dir = tempfile::tempdir().expect("temp");
    let memory = dir.path();
    write_config(memory);
    write_forget(memory);
    write_log(memory, 1, KEPT);
    write_log(memory, 2, FORGOTTEN);
    init_graph(memory).await;

    let err = rebuild(
        memory,
        options(),
        &IdleStop,
        Some(&Scripted {
            fail_on: Some("9000"),
        }),
    )
    .await
    .expect_err("stop on the forgotten archive");
    assert!(err.to_string().contains("log 002"), "{err}");
    assert!(memory.join("graph.v2-build").is_dir());

    let skips = rebuild(
        memory,
        options(),
        &IdleStop,
        Some(&Scripted { fail_on: None }),
    )
    .await
    .expect("resume");
    assert_eq!(skips, 1);
    let values = slot_values(&memory.join("graph")).await;
    assert!(values.contains(&"1000".to_string()), "{values:?}");
    assert!(values.contains(&"9001".to_string()), "{values:?}");
    assert!(!values.iter().any(|value| value == "9000"), "{values:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn extract_after_graph_ingest_skips_the_fact_and_consolidation_leaves_it_out() {
    let dir = tempfile::tempdir().expect("temp");
    let memory = dir.path();
    write_config(memory);
    write_forget(memory);
    write_log(memory, 7, FORGOTTEN);
    init_graph(memory).await;
    std::env::set_var(
        serve_client::DAEMON_BIN_ENV,
        env!("CARGO_BIN_EXE_recall-echo"),
    );

    let archive = memory.join("conversations/conversation-007.md");
    recall_echo::graph_cli::ingest(memory, &archive, None, Some(PipelineVersion::V1))
        .await
        .expect("graph ingest");
    let text = std::fs::read_to_string(&archive).expect("archive");
    let report = recall_echo::graph_cli::extract_log_with(
        memory,
        7,
        &text,
        "s7",
        &Scripted { fail_on: None },
    )
    .await
    .expect("extract");
    let _ = serve_client::stop_daemon(memory).await;

    assert_eq!(report.forgotten_skips, 1, "{:?}", report.drops);
    let values = slot_values(&memory.join("graph")).await;
    assert!(values.contains(&"9001".to_string()), "{values:?}");
    assert!(
        !values.iter().any(|value| value == "9000"),
        "consolidation wrote the forgotten value back: {values:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_ingested_transcript_skips_a_forgotten_fact_and_counts_it() {
    let dir = tempfile::tempdir().expect("temp");
    let memory = dir.path().to_path_buf();
    write_config(&memory);
    write_forget(&memory);
    std::fs::create_dir_all(memory.join("conversations")).expect("conversations");
    init_graph(&memory).await;
    std::env::set_var(
        serve_client::DAEMON_BIN_ENV,
        env!("CARGO_BIN_EXE_recall-echo"),
    );

    let transcript = dir.path().join("session.jsonl");
    std::fs::write(
        &transcript,
        format!(
            r#"{{"type":"user","timestamp":"2026-03-01T00:00:00.000Z","message":{{"role":"user","content":"{FORGOTTEN}"}}}}"#
        ),
    )
    .expect("transcript");
    let archived = memory.clone();
    let source = transcript.clone();
    let log = tokio::task::spawn_blocking(move || {
        recall_echo::archive::archive_from_jsonl(
            &archived,
            "s-transcript",
            &source.display().to_string(),
        )
    })
    .await
    .expect("archive thread")
    .expect("ingest transcript");
    assert_ne!(log, 0, "the transcript had no user turn");

    let archive = memory.join(format!("conversations/conversation-{log:03}.md"));
    let text = std::fs::read_to_string(&archive).expect("archive");
    let report = recall_echo::graph_cli::extract_log_with(
        &memory,
        log,
        &text,
        "s-transcript",
        &Scripted { fail_on: None },
    )
    .await
    .expect("extract");
    let _ = serve_client::stop_daemon(&memory).await;

    assert_eq!(report.forgotten_skips, 1, "{:?}", report.drops);
    let gm = GraphMemory::open_embedded(&memory.join("graph"))
        .await
        .expect("reopen");
    assert!(
        gm.stats().await.expect("stats").episode_count >= 1,
        "the transcript was not ingested"
    );
    drop(gm);
    let values = slot_values(&memory.join("graph")).await;
    assert!(values.contains(&"9001".to_string()), "{values:?}");
    assert!(!values.iter().any(|value| value == "9000"), "{values:?}");
}
