// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `recall-echo config show` prints the whole configuration.
//!
//! Run against the real binary, because what is under test is what a user
//! debugging their setup sees. The sections that decide whether sessions are
//! imported at all — `[capture]`, `[extraction]`, `[serve]` — are all defaulted
//! on, which means a working install has an empty config file and the answer to
//! "why is it doing that?" is nowhere on screen unless defaults are printed too.

use std::process::Command;

use tempfile::TempDir;

/// `config show` writes to stderr; stdout stays free for piping.
fn config_show(pulse_root: &std::path::Path) -> String {
    config_show_with("--pulse-root", pulse_root)
}

fn config_show_with(flag: &str, pulse_root: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_recall-echo"))
        .args(["config", flag])
        .arg(pulse_root)
        .arg("show")
        .output()
        .expect("run config show");
    assert!(output.status.success(), "config show failed: {output:?}");
    String::from_utf8(output.stderr).expect("utf-8 output")
}

fn pulse_root(config: &str) -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir).expect("memory dir");
    if !config.is_empty() {
        std::fs::write(memory_dir.join(".recall-echo.toml"), config).expect("write config");
    }
    dir
}

#[test]
fn every_section_is_shown_even_at_its_defaults() {
    let dir = pulse_root("");
    let shown = config_show(dir.path());

    for section in [
        "[ephemeral]",
        "[llm]",
        "[capture]",
        "[extraction]",
        "[serve]",
    ] {
        assert!(
            shown.contains(section),
            "{section} is missing from:\n{shown}"
        );
    }
}

#[test]
fn capture_says_what_it_sweeps_and_how_long_it_waits() {
    let dir = pulse_root("");
    let shown = config_show(dir.path());

    assert!(shown.contains("enabled     = true"), "{shown}");
    assert!(
        shown.contains("every CLI with sessions on this machine"),
        "an auto-detecting default must say what it detects:\n{shown}"
    );
    assert!(shown.contains("settle_secs = 300"), "{shown}");
}

#[test]
fn configured_capture_sources_are_listed_by_name() {
    let dir = pulse_root("[capture]\nsources = [\"codex\", \"grok\"]\n");
    let shown = config_show(dir.path());
    assert!(shown.contains("sources     = codex, grok"), "{shown}");
}

#[test]
fn extraction_shows_what_the_daemon_will_do_on_its_own() {
    let dir = pulse_root("");
    let shown = config_show(dir.path());

    assert!(shown.contains("background_enabled = true"), "{shown}");
    assert!(shown.contains("idle_after_secs    = 120"), "{shown}");
    assert!(shown.contains("batch_size         = 3"), "{shown}");
}

#[test]
fn serve_shows_where_the_daemon_listens_and_how_long_it_lives() {
    let dir = pulse_root("");
    let shown = config_show(dir.path());

    assert!(
        shown.contains("derived from the memory directory"),
        "{shown}"
    );
    assert!(shown.contains("idle_timeout_secs = 3600"), "{shown}");
}

#[test]
fn a_daemon_that_never_shuts_down_says_so_rather_than_printing_zero() {
    let dir = pulse_root("[serve]\nsocket_path = \"/tmp/re.sock\"\nidle_timeout_secs = 0\n");
    let shown = config_show(dir.path());

    assert!(
        shown.contains("socket_path       = /tmp/re.sock"),
        "{shown}"
    );
    assert!(shown.contains("never idle-shuts-down"), "{shown}");
}

/// The pre-4.6.0 flag reads the same store as the new one.
#[test]
fn the_legacy_entity_root_flag_reads_the_same_store() {
    let dir = pulse_root("[capture]\nsources = [\"codex\"]\n");
    assert_eq!(
        config_show_with("--entity-root", dir.path()),
        config_show(dir.path())
    );
}
