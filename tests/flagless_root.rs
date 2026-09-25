// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Flagless commands find the store `init` persisted (#63).
//!
//! Run against the real binary from a directory that is *not* the pulse
//! root, with `XDG_CONFIG_HOME` and `HOME` pointed at a temp dir so the
//! developer's real persisted root and `~/.claude` are never read or written.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    root: std::path::PathBuf,
    elsewhere: std::path::PathBuf,
    config: std::path::PathBuf,
    home: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("temp dir");
    let root = tmp.path().join("root");
    let memory = root.join("memory");
    std::fs::create_dir_all(memory.join("conversations")).expect("memory layout");
    std::fs::write(memory.join("MEMORY.md"), "# Memory\n").expect("MEMORY.md");
    std::fs::write(memory.join("EPHEMERAL.md"), "").expect("EPHEMERAL.md");
    std::fs::write(memory.join(".recall-echo.toml"), "").expect("config");
    let elsewhere = tmp.path().join("elsewhere");
    let config = tmp.path().join("config");
    let home = tmp.path().join("home");
    for dir in [&elsewhere, &config.join("recall-echo"), &home] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    Fixture {
        _tmp: tmp,
        root,
        elsewhere,
        config,
        home,
    }
}

impl Fixture {
    fn persist(&self, root: &Path) {
        self.persist_as("pulse-root", root);
    }

    /// Write the pointer under `name` — `entity-root` is the pre-4.6.0 file.
    fn persist_as(&self, name: &str, root: &Path) {
        std::fs::write(
            self.config.join("recall-echo").join(name),
            format!("{}\n", root.display()),
        )
        .expect("persist");
    }

    fn run(&self, args: &[&str], env_home: Option<&Path>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_recall-echo"));
        cmd.args(args)
            .current_dir(&self.elsewhere)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("HOME", &self.home)
            .env_remove("RECALL_ECHO_HOME");
        if let Some(home) = env_home {
            cmd.env("RECALL_ECHO_HOME", home);
        }
        cmd.output().expect("run recall-echo")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn bare_status_from_elsewhere_uses_the_persisted_root() {
    let fx = fixture();
    fx.persist(&fx.root);

    for args in [&[][..], &["status"][..]] {
        let out = fx.run(args, None);
        let err = stderr(&out);
        assert!(out.status.success(), "{args:?} failed: {err}");
        assert!(err.contains("healthy"), "{args:?}: {err}");
        assert!(
            err.contains("persisted by `recall-echo init`"),
            "{args:?} should say where the root came from: {err}"
        );
    }
}

#[test]
fn config_show_from_elsewhere_reads_the_persisted_store() {
    let fx = fixture();
    fx.persist(&fx.root);
    let out = fx.run(&["config", "show"], None);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    let canonical = std::fs::canonicalize(&fx.root).expect("canonical root");
    assert!(
        err.contains(&canonical.join("memory").display().to_string()),
        "config show should name the persisted store: {err}"
    );
}

#[test]
fn a_missing_persisted_root_is_named_and_ignored() {
    let fx = fixture();
    fx.persist(&fx.root.join("gone"));
    let out = fx.run(&["status"], None);
    let err = stderr(&out);
    assert!(!out.status.success());
    assert!(err.contains("ignoring persisted pulse root"), "{err}");
    assert!(err.contains("gone"), "{err}");
    assert!(err.contains("memory/ directory not found"), "{err}");
}

#[test]
fn a_world_writable_persisted_root_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    std::fs::set_permissions(&fx.root, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    fx.persist(&fx.root);
    let out = fx.run(&["status"], None);
    let err = stderr(&out);
    assert!(!out.status.success());
    assert!(err.contains("writable by other users"), "{err}");
}

#[test]
fn recall_echo_home_wins_over_a_stale_persisted_root() {
    let fx = fixture();
    fx.persist(&fx.root.join("gone"));
    let out = fx.run(&["status"], Some(&fx.root));
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("healthy"), "{err}");
    assert!(!err.contains("ignoring persisted"), "{err}");
}

/// An install `init`-ed before 4.6.0 has only `entity-root`: it keeps
/// resolving, and both the stderr note and the provenance line say so.
#[test]
fn a_legacy_entity_root_file_still_resolves_and_is_named() {
    let fx = fixture();
    fx.persist_as("entity-root", &fx.root);
    let out = fx.run(&["status"], None);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("healthy"), "{err}");
    assert!(err.contains("legacy pointer"), "{err}");
    assert!(err.contains("legacy entity-root file"), "{err}");
}

/// Once `init` has written `pulse-root`, a leftover `entity-root` is ignored.
#[test]
fn the_pulse_root_file_wins_over_a_leftover_entity_root_file() {
    let fx = fixture();
    fx.persist(&fx.root);
    fx.persist_as("entity-root", &fx.root.join("gone"));
    let out = fx.run(&["status"], None);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("persisted by `recall-echo init`"), "{err}");
    assert!(!err.contains("legacy"), "{err}");
    assert!(!err.contains("gone"), "{err}");
}
