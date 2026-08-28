// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Swap-engine tests for `recall-echo update`, against fake binaries
//! (shell scripts that answer `--version`) in tempdirs. No network.

#![cfg(all(unix, feature = "self-update"))]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use recall_echo::update::{preflight_writable, self_check, swap};

/// A fake recall-echo: a script that reports the given version.
fn fake_binary(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"recall-echo {version}\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn self_check_accepts_matching_version() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_binary(dir.path(), "recall-echo", "9.9.9");
    assert!(self_check(&bin, "9.9.9").is_ok());
    assert!(self_check(&bin, "1.0.0").is_err());
}

#[test]
fn swap_replaces_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let install = fake_binary(dir.path(), "recall-echo", "1.0.0");
    let new_bin = fake_binary(dir.path(), ".update.bin", "2.0.0");

    swap(&install, &new_bin, "1.0.0", "2.0.0").unwrap();

    // New binary answers at the install path, temp name is gone, and the
    // backup was cleaned up after verification.
    assert!(self_check(&install, "2.0.0").is_ok());
    assert!(!new_bin.exists());
    assert!(!dir.path().join("recall-echo.old.1.0.0").exists());
}

#[test]
fn failed_self_check_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let install = fake_binary(dir.path(), "recall-echo", "1.0.0");
    // The "new" binary lies about its version, so post-swap verification fails.
    let new_bin = fake_binary(dir.path(), ".update.bin", "0.0.1");

    let err = swap(&install, &new_bin, "1.0.0", "2.0.0").unwrap_err();
    assert!(err.to_string().contains("failed verification"));

    // Old binary is back in place and functional.
    assert!(self_check(&install, "1.0.0").is_ok());
    assert!(!dir.path().join("recall-echo.old.1.0.0").exists());
}

#[test]
fn unwritable_dir_fails_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let ro = dir.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = preflight_writable(&ro);
    // Root bypasses permission bits; only assert where the dir is really unwritable.
    if std::fs::write(ro.join("root-check"), b"").is_err() {
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not writable"));
    }
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(preflight_writable(&ro).is_ok());
}
