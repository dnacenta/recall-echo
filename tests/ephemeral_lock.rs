// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `status` must not mention the hidden EPHEMERAL lock file.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn status_ignores_the_ephemeral_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("pulse");
    let memory = root.join("memory");
    fs::create_dir_all(memory.join("conversations")).unwrap();
    fs::write(memory.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(memory.join("EPHEMERAL.md"), "## Session a\n").unwrap();
    fs::write(memory.join("ARCHIVE.md"), "# Conversation Archive\n").unwrap();

    let without = status_text(&root);
    fs::write(memory.join(".EPHEMERAL.lock"), "").unwrap();
    fs::write(memory.join(".EPHEMERAL.md.1.1.tmp"), "stale\n").unwrap();
    let with = status_text(&root);
    assert_eq!(without, with);
    assert!(!without.contains(".EPHEMERAL"));
}

fn status_text(root: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_recall-echo"))
        .arg("status")
        .arg(root)
        .env("HOME", root)
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "status failed: {stderr}{}",
        String::from_utf8_lossy(&output.stdout)
    );
    stderr
}
