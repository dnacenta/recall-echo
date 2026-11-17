// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A `.recall-echo.toml` that does not parse is an error naming the file.
//!
//! These run the binary. Each command fails before a socket is resolved, so
//! none of them start a daemon.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const BROKEN: &str = "this is not toml [[[\n";

struct Sandbox {
    home: TempDir,
    pulse: PathBuf,
    memory: PathBuf,
    config: PathBuf,
}

fn sandbox(with_graph: bool) -> Sandbox {
    let home = TempDir::new().expect("temp home");
    let pulse = home.path().join("pulse");
    let memory = pulse.join("memory");
    fs::create_dir_all(memory.join("conversations")).expect("conversations");
    if with_graph {
        fs::create_dir_all(memory.join("graph")).expect("graph dir");
    }
    let config = memory.join(".recall-echo.toml");
    fs::write(&config, BROKEN).expect("config");
    fs::create_dir_all(home.path().join("xdg")).expect("xdg");
    fs::create_dir_all(home.path().join("run")).expect("runtime");
    Sandbox {
        home,
        pulse,
        memory,
        config,
    }
}

fn command(sandbox: &Sandbox) -> Command {
    let mut cmd = Command::new("timeout");
    cmd.arg("20")
        .arg(env!("CARGO_BIN_EXE_recall-echo"))
        .env("HOME", sandbox.home.path())
        .env("XDG_CONFIG_HOME", sandbox.home.path().join("xdg"))
        .env("XDG_RUNTIME_DIR", sandbox.home.path().join("run"))
        .env_remove("RECALL_ECHO_HOME")
        .current_dir(sandbox.home.path());
    cmd
}

fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    command(sandbox)
        .args(args)
        .output()
        .expect("run recall-echo")
}

fn assert_names_config(output: &Output, config: &Path) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains(&config.display().to_string()), "{stderr}");
}

#[test]
fn status_names_a_malformed_config() {
    let sandbox = sandbox(false);
    let output = run(
        &sandbox,
        &["status", sandbox.pulse.to_str().expect("utf-8")],
    );
    assert_names_config(&output, &sandbox.config);
}

#[test]
fn config_show_names_a_malformed_config() {
    let sandbox = sandbox(false);
    let pulse = sandbox.pulse.to_str().expect("utf-8");
    let output = run(&sandbox, &["config", "--pulse-root", pulse, "show"]);
    assert_names_config(&output, &sandbox.config);
}

#[test]
fn config_set_names_a_malformed_config_and_writes_nothing() {
    let sandbox = sandbox(false);
    let before = fs::read(&sandbox.config).expect("read");
    let pulse = sandbox.pulse.to_str().expect("utf-8");
    let output = run(
        &sandbox,
        &[
            "config",
            "--pulse-root",
            pulse,
            "set",
            "ephemeral.max_entries",
            "5",
        ],
    );
    assert_names_config(&output, &sandbox.config);
    assert_eq!(fs::read(&sandbox.config).expect("read"), before);
}

#[test]
fn graph_status_names_a_malformed_config() {
    let sandbox = sandbox(true);
    let pulse = sandbox.pulse.to_str().expect("utf-8");
    let output = run(&sandbox, &["graph", "--pulse-root", pulse, "status"]);
    assert_names_config(&output, &sandbox.config);
}

#[test]
fn graph_extract_dry_run_names_a_malformed_config() {
    let sandbox = sandbox(true);
    let pulse = sandbox.pulse.to_str().expect("utf-8");
    let output = run(
        &sandbox,
        &["graph", "--pulse-root", pulse, "extract", "--dry-run"],
    );
    assert_names_config(&output, &sandbox.config);
}

#[test]
fn ingest_names_a_malformed_config() {
    let sandbox = sandbox(false);
    let memory = sandbox.memory.to_str().expect("utf-8");
    let output = run(&sandbox, &["ingest", "--from", "grok", "--dir", memory]);
    assert_names_config(&output, &sandbox.config);
}
