// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memory system health check.
//!
//! Quick status overview of the four-layer memory system.
//! For the full ASCII art dashboard, use the `dashboard` module.

use std::fs;
use std::path::Path;

use crate::config;
use crate::ephemeral;
use crate::error::RecallError;
use crate::paths;
use crate::theme::{BAD, BOLD, DIM, GOOD, RESET, WARN};

pub fn run() -> Result<(), RecallError> {
    let (root, source) = paths::pulse_root_described()?;
    eprintln!("{DIM}root {} — {source}{RESET}", root.display());
    run_with_base(&root)
}

/// The checks `init` applies before importing this store's MEMORY.md into
/// every Claude Code session, as a status issue when they fail.
fn trust_issue(pulse_root: &Path) -> Option<String> {
    crate::rules_file::vet_store(pulse_root).err()
}

pub fn run_with_base(pulse_root: &Path) -> Result<(), RecallError> {
    let memory = pulse_root.join("memory");
    if !memory.exists() {
        return Err(RecallError::NotInitialized(
            "memory/ directory not found. Run `recall-echo init` first.".into(),
        ));
    }

    let mut issues: Vec<String> = Vec::new();

    // Header
    let overall = if memory.join("conversations").exists()
        && memory.join("EPHEMERAL.md").exists()
        && memory.join("MEMORY.md").exists()
    {
        format!("{GOOD}healthy{RESET}")
    } else {
        issues.push("Run `recall-echo init` to complete setup".to_string());
        format!("{WARN}incomplete{RESET}")
    };

    eprintln!("\n{BOLD}recall-echo{RESET} — {overall}\n");

    // MEMORY.md
    let memory_path = memory.join("MEMORY.md");
    // Read once, and only a regular file: a FIFO would block the read forever.
    let memory_md = fs::metadata(&memory_path)
        .is_ok_and(|meta| meta.is_file())
        .then(|| fs::read_to_string(&memory_path).unwrap_or_default());
    if let Some(content) = memory_md {
        let lines = content.lines().count();
        let pct = (lines as f32 / 200.0 * 100.0) as u32;
        let bar = progress_bar(pct, 4);
        let color = if pct > 90 {
            BAD
        } else if pct > 70 {
            WARN
        } else {
            GOOD
        };
        eprintln!("  MEMORY.md       {color}{lines}/200 lines ({pct}%){RESET}  {bar}");
        if pct > 70 {
            issues.push(format!("MEMORY.md approaching limit ({pct}%)"));
        }
        eprintln!(
            "                  {DIM}{} — loaded into every Claude Code session{RESET}",
            crate::distill::size_summary(&content)
        );
        if content.len() > crate::distill::DISTILL_AFTER_BYTES {
            issues.push(format!(
                "MEMORY.md is {} KB, over the {} KB budget — run distill",
                content.len() / 1024,
                crate::distill::DISTILL_AFTER_BYTES / 1024
            ));
        }
        issues.extend(crate::rules_file::hidden_import_warning(&content));
    } else if fs::symlink_metadata(&memory_path).is_ok() {
        eprintln!("  MEMORY.md       {BAD}not a regular file{RESET}");
        issues.push("MEMORY.md is not a regular file".to_string());
    } else {
        eprintln!("  MEMORY.md       {DIM}not found{RESET}");
        issues.push("MEMORY.md not found".to_string());
    }
    issues.extend(trust_issue(pulse_root));

    // EPHEMERAL.md
    let cfg = config::load(&memory);
    let max_entries = cfg.ephemeral.max_entries;
    let ephemeral_path = memory.join("EPHEMERAL.md");
    if ephemeral_path.exists() {
        let count = ephemeral::count_entries(&ephemeral_path).unwrap_or(0);
        eprintln!("  EPHEMERAL       {count}/{max_entries} sessions");
    } else {
        eprintln!("  EPHEMERAL       {DIM}not found{RESET}");
    }

    // Archives
    let conversations_dir = memory.join("conversations");
    if conversations_dir.exists() {
        let (count, total_bytes) = count_conversations(&conversations_dir);
        let size_str = format_bytes(total_bytes);
        eprintln!("  Archives        {count} conversations ({size_str})");

        if count > 0 {
            let (oldest, newest) = find_date_range(&conversations_dir);
            if let Some(newest) = newest {
                eprintln!("  Last archived   {newest}");
            }
            if let Some(oldest) = oldest {
                eprintln!("  Oldest archive  {oldest}");
            }
        }
    } else {
        eprintln!("  Archives        {DIM}not initialized{RESET}");
    }

    // Graph — counts stay with `graph status` (this command does not take the
    // store). The pipeline line reads the stamp only when the directory is
    // already a SurrealKV store, or from a daemon that already holds it.
    let graph_dir = memory.join("graph");
    if graph_dir.exists() {
        eprintln!(
            "  Graph           {DIM}present — run `recall-echo graph status` for counts{RESET}"
        );
        let pipeline = pipeline_report(&memory);
        eprintln!("  Pipeline        {}", pipeline.summary);
        if let Some(warning) = pipeline.warning {
            issues.push(warning);
        }
    }

    // Issues
    eprintln!();
    if issues.is_empty() {
        eprintln!("  {GOOD}No issues detected.{RESET}");
    } else {
        for issue in &issues {
            eprintln!("  {WARN}!{RESET} {issue}");
        }
    }
    eprintln!();

    Ok(())
}

struct PipelineReport {
    summary: String,
    warning: Option<String>,
}

/// Configured pipeline beside the store stamp. A missing config counts as v1,
/// which is what extraction runs. An unstamped store is not a mismatch: v1
/// has never written the stamp.
fn pipeline_report(memory: &Path) -> PipelineReport {
    let configured = config::load(memory)
        .graph
        .and_then(|section| section.pipeline);
    let configured_label = configured.as_deref().unwrap_or("unset");
    let stamp = match read_stamp_blocking(memory) {
        Ok(stamp) => stamp,
        Err(err) => {
            return PipelineReport {
                summary: format!("configured {configured_label}, store unread ({err})"),
                warning: None,
            };
        }
    };
    let store_label = stamp.as_deref().unwrap_or("unstamped");
    let comparable = configured.as_deref().unwrap_or("v1");
    let warning = stamp
        .as_deref()
        .filter(|stamp| *stamp != comparable)
        .map(|stamp| {
            format!("pipeline config is {configured_label} but the store is stamped {stamp}")
        });
    PipelineReport {
        summary: format!("configured {configured_label}, store {store_label}"),
        warning,
    }
}

fn read_stamp_blocking(memory: &Path) -> Result<Option<String>, RecallError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(crate::graph::v2::read_embedded_stamp(memory))
}

fn count_conversations(dir: &Path) -> (usize, u64) {
    let mut count = 0;
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("conversation-") && name.ends_with(".md") {
                count += 1;
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (count, total)
}

fn find_date_range(dir: &Path) -> (Option<String>, Option<String>) {
    let mut dates: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("conversation-") && name.ends_with(".md") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    for line in content.lines().take(10) {
                        if let Some(date) = line.strip_prefix("date: ") {
                            let d = date.trim().trim_matches('"');
                            if let Some(day) = d.split('T').next() {
                                dates.push(day.to_string());
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    dates.sort();
    let oldest = dates.first().cloned();
    let newest = dates.last().cloned();
    (oldest, newest)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn progress_bar(pct: u32, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

#[cfg(test)]
mod tests {
    /// A FIFO in MEMORY.md's place is reported, never read — reading it
    /// would block `status` forever.
    #[test]
    fn status_does_not_read_a_fifo_memory_md() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        std::fs::create_dir_all(memory.join("conversations")).unwrap();
        let fifo = std::ffi::CString::new(memory.join("MEMORY.md").to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // Returns at all: before, this blocked on the FIFO.
        run_with_base(tmp.path()).unwrap();
    }

    #[test]
    fn status_reports_a_store_others_can_write() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        for dir in [tmp.path(), memory.as_path()] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(trust_issue(tmp.path()), None);

        std::fs::set_permissions(&memory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let issue = trust_issue(tmp.path()).expect("an issue");
        assert!(issue.contains("writable by every user"), "{issue}");
        assert!(issue.contains("chmod g-w,o-w"), "{issue}");
    }

    use super::*;

    /// Init writes hooks and a persisted root pointer outside the store, so
    /// it runs here against a sandbox: the suite never touches the real
    /// configuration (#59).
    #[test]
    fn status_on_initialized_env() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("entity");
        fs::create_dir_all(&root).unwrap();
        let roots = crate::paths::ConfigRoots::sandboxed(tmp.path()).unwrap();
        let mut reader = std::io::Cursor::new(b"skip\n" as &[u8]);
        crate::init::run_with(&root, &mut reader, &roots).unwrap();
        assert!(run_with_base(&root).is_ok());
    }

    #[test]
    fn status_on_missing_dir() {
        assert!(run_with_base(Path::new("/nonexistent")).is_err());
    }

    #[test]
    fn format_bytes_ranges() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn progress_bar_display() {
        assert_eq!(progress_bar(0, 4), "░░░░");
        assert_eq!(progress_bar(50, 4), "██░░");
        assert_eq!(progress_bar(100, 4), "████");
    }

    #[test]
    fn count_conversations_basic() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("conversation-001.md"), "hello").unwrap();
        fs::write(tmp.path().join("conversation-002.md"), "world").unwrap();
        fs::write(tmp.path().join("notes.md"), "ignore").unwrap();
        let (count, _) = count_conversations(tmp.path());
        assert_eq!(count, 2);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn status_warns_when_the_store_stamp_and_the_config_differ() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        fs::write(memory.join("MEMORY.md"), "# m\n").unwrap();
        fs::write(memory.join("EPHEMERAL.md"), "").unwrap();
        fs::write(
            memory.join(".recall-echo.toml"),
            "[graph]\npipeline = \"v1\"\n",
        )
        .unwrap();
        {
            let gm = crate::graph::GraphMemory::open_embedded(&memory.join("graph"))
                .await
                .unwrap();
            crate::graph::v2::ensure_v2_store(&gm).await.unwrap();
        }
        // `pipeline_report` builds its own runtime. That cannot run on this
        // one, and the stamp write has to drop its connection here first so
        // the report's open is not the lock this runtime still holds.
        let memory_for_report = memory.clone();
        let report = tokio::task::spawn_blocking(move || pipeline_report(&memory_for_report))
            .await
            .unwrap();
        assert!(
            report.summary.contains("configured v1"),
            "{}",
            report.summary
        );
        assert!(report.summary.contains("store v2"), "{}", report.summary);
        let warning = report.warning.expect("mismatch");
        assert!(warning.contains("config is v1"), "{warning}");
        assert!(warning.contains("stamped v2"), "{warning}");
    }

    #[cfg(feature = "embedded")]
    #[test]
    fn status_does_not_create_a_store_to_read_the_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        let graph = memory.join("graph");
        fs::create_dir_all(graph.join("surreal")).unwrap();
        let before: Vec<_> = walk_files(&graph);
        let report = pipeline_report(&memory);
        assert!(
            report.summary.contains("store unstamped"),
            "{}",
            report.summary
        );
        assert!(report.warning.is_none(), "{:?}", report.warning);
        assert_eq!(walk_files(&graph), before);
    }

    #[cfg(feature = "embedded")]
    fn walk_files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut entries: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                if path.is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        walk(root, root, &mut out);
        out
    }
}
