// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `graph ingest --pipeline v2` against a temp store.
//!
//! The provider is a local program that records each invocation. A repeat
//! ingest of an already-extracted log must not run it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use recall_echo::graph::{GraphMemory, IngestContext, PipelineVersion};
use recall_echo::serve_client;

#[tokio::test]
#[cfg_attr(
    not(embed_tests),
    ignore = "embeds: set RECALL_ECHO_MODELS_DIR to a seeded models dir"
)]
async fn a_clean_repeat_ingest_still_skips_without_calls() {
    let dir = tempfile::tempdir().expect("temp");
    let memory = dir.path().join("memory");
    let graph = memory.join("graph");
    fs::create_dir_all(&graph).expect("memory");

    let calls = dir.path().join("calls");
    let provider = dir.path().join("provider.sh");
    write_counting_provider(&provider, &calls);

    let socket = dir.path().join("g.sock");
    fs::write(
        memory.join(".recall-echo.toml"),
        format!(
            "[llm]\nprovider = \"cli\"\n\n[llm.cli]\ncommand = \"{}\"\n\n\
             [extraction]\nbackground_enabled = false\n\n\
             [capture]\nenabled = false\n\n\
             [serve]\nsocket_path = \"{}\"\nidle_timeout_secs = 30\n",
            provider.display(),
            socket.display()
        ),
    )
    .expect("config");

    let archive = memory.join("conversations/conversation-007.md");
    fs::create_dir_all(archive.parent().expect("conversations")).expect("conversations");
    let text = "### User\n\nA note about the shelf.\n";
    fs::write(&archive, text).expect("archive");

    {
        let gm = GraphMemory::open_embedded(&graph).await.expect("open");
        let report = gm
            .ingest_archive(text, &IngestContext::new("session-note", Some(7)), None)
            .await
            .expect("seed");
        assert!(report.episodes_created > 0);
        assert!(report.errors.is_empty());
        gm.mark_extracted(7).await.expect("mark extracted");
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The client starts the daemon from the binary this test built.
    std::env::set_var(
        serve_client::DAEMON_BIN_ENV,
        env!("CARGO_BIN_EXE_recall-echo"),
    );

    let ingested =
        recall_echo::graph_cli::ingest(&memory, &archive, None, Some(PipelineVersion::V2)).await;
    let stopped = serve_client::stop_daemon(&memory).await;
    ingested.expect("repeat ingest of an extracted log");
    stopped.expect("stop daemon");

    let called = fs::read_to_string(&calls).unwrap_or_default();
    assert!(
        called.is_empty(),
        "provider ran during a repeat ingest of an extracted log: {called}"
    );
}

fn write_counting_provider(path: &Path, calls: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf 'x\\n' >> '{}'\nexit 0\n",
        calls.display()
    );
    fs::write(path, script).expect("provider script");
    let mut permissions = fs::metadata(path).expect("provider metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("provider mode");
}
