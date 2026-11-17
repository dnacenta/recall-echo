// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::RecallError;

pub const DEFAULT_MAX_ENTRIES: usize = 5;
const ENTRY_SEPARATOR: &str = "\n---\n\n";

pub struct EphemeralEntry {
    pub session_id: String,
    pub date: String,
    pub duration: String,
    pub message_count: u32,
    pub archive_file: String,
    pub summary: String,
}

impl EphemeralEntry {
    #[must_use]
    pub fn render(&self) -> String {
        let display_date = self.date.replace('T', " ").replace('Z', " UTC");
        format!(
            "## Session {} — {}\n**Duration**: ~{} | **Messages**: {} | **Archive**: {}\n**Summary**: {}",
            self.session_id, display_date, self.duration, self.message_count,
            self.archive_file, self.summary
        )
    }
}

/// Append a session entry to EPHEMERAL.md
pub fn append_entry(ephemeral_path: &Path, entry: &EphemeralEntry) -> Result<(), RecallError> {
    let existing = if ephemeral_path.exists() {
        fs::read_to_string(ephemeral_path)?
    } else {
        String::new()
    };

    let new_content = if existing.trim().is_empty() {
        entry.render()
    } else {
        format!(
            "{}{}{}",
            existing.trim_end(),
            ENTRY_SEPARATOR,
            entry.render()
        )
    };

    fs::write(ephemeral_path, format!("{new_content}\n"))?;

    Ok(())
}

/// Parse EPHEMERAL.md content into individual entry strings.
#[must_use]
pub fn parse_entries(content: &str) -> Vec<&str> {
    if content.trim().is_empty() {
        return Vec::new();
    }

    content
        .split("\n---\n")
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Count current entries in EPHEMERAL.md
pub fn count_entries(ephemeral_path: &Path) -> Result<usize, RecallError> {
    if !ephemeral_path.exists() {
        return Ok(0);
    }
    let content = fs::read_to_string(ephemeral_path)?;
    Ok(parse_entries(&content).len())
}

/// Trim EPHEMERAL.md to max_entries, removing oldest entries (FIFO).
pub fn trim_to_limit(ephemeral_path: &Path, max_entries: usize) -> Result<(), RecallError> {
    if !ephemeral_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(ephemeral_path)?;

    let entries = parse_entries(&content);
    if entries.len() <= max_entries {
        return Ok(());
    }

    // Keep the last max_entries (most recent)
    let kept: Vec<&str> = entries[entries.len() - max_entries..].to_vec();
    let new_content = kept.join(ENTRY_SEPARATOR);

    fs::write(ephemeral_path, format!("{new_content}\n"))?;

    Ok(())
}

/// Append `entry` and trim to `max` in one locked, atomic replacement.
///
/// The lock is the sibling `.EPHEMERAL.lock`, because the publish is a
/// `rename` and a rename changes the inode `flock` would have been on.
pub fn record_entry(path: &Path, entry: &EphemeralEntry, max: usize) -> Result<(), RecallError> {
    record_entry_with(path, &entry.render(), max, crate::file_lock::lock_wait())
}

fn record_entry_with(
    path: &Path,
    rendered: &str,
    max: usize,
    wait: Duration,
) -> Result<(), RecallError> {
    let lock_path = lock_path(path);
    let lock = open_lock_file(&lock_path)?;
    crate::file_lock::lock_exclusive(&lock, &lock_path, wait)?;
    publish_ephemeral(path, rendered, max)
}

fn lock_path(ephemeral: &Path) -> PathBuf {
    match ephemeral.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(".EPHEMERAL.lock"),
        _ => PathBuf::from(".EPHEMERAL.lock"),
    }
}

fn open_lock_file(path: &Path) -> Result<File, RecallError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    ensure_lock_mode(&file)?;
    Ok(file)
}

fn ensure_lock_mode(file: &File) -> Result<(), RecallError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode == 0o600 {
        return Ok(());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn publish_ephemeral(path: &Path, rendered: &str, max: usize) -> Result<(), RecallError> {
    let target = resolve_target(path)?;
    sweep_stale_temps(&target_dir(&target))?;
    let existing = read_optional(&target)?;
    let next = next_ephemeral(existing.as_deref(), rendered, max);
    replace_file(&target, &next)
}

fn target_dir(target: &Path) -> PathBuf {
    match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn resolve_target(path: &Path) -> Result<PathBuf, RecallError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(RecallError::from)
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(err) => Err(err.into()),
    }
}

fn sweep_stale_temps(dir: &Path) -> Result<(), RecallError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        remove_temp(&entry?)?;
    }
    Ok(())
}

fn remove_temp(entry: &fs::DirEntry) -> Result<(), RecallError> {
    let name = entry.file_name();
    if stale_temp_name(&name.to_string_lossy()) {
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn stale_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".EPHEMERAL.md.") else {
        return false;
    };
    rest.ends_with(".tmp")
}

fn read_optional(path: &Path) -> Result<Option<String>, RecallError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Bytes `append_entry` then `trim_to_limit` would leave for this input.
fn next_ephemeral(existing: Option<&str>, rendered: &str, max: usize) -> String {
    let appended = append_rendered(existing.unwrap_or(""), rendered);
    trim_rendered(&appended, max)
}

fn append_rendered(existing: &str, rendered: &str) -> String {
    if existing.trim().is_empty() {
        format!("{rendered}\n")
    } else {
        format!("{}{ENTRY_SEPARATOR}{rendered}\n", existing.trim_end())
    }
}

fn trim_rendered(content: &str, max: usize) -> String {
    let entries = parse_entries(content);
    if entries.len() <= max {
        return content.to_string();
    }
    let start = entries.len() - max;
    format!("{}\n", entries[start..].join(ENTRY_SEPARATOR))
}

fn replace_file(target: &Path, contents: &str) -> Result<(), RecallError> {
    let temp = temp_path(target);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let mut guard = TempGuard::arm(temp);
    file.write_all(contents.as_bytes())?;
    // The rename is what readers see. sync_data first so a crash after the
    // rename cannot publish a file whose bytes never reached disk.
    file.sync_data()?;
    copy_mode(target, &guard.path)?;
    fs::rename(&guard.path, target)?;
    guard.disarm();
    Ok(())
}

fn temp_path(target: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let name = format!(".EPHEMERAL.md.{}.{n}.tmp", std::process::id());
    match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

fn copy_mode(target: &Path, temp: &Path) -> Result<(), RecallError> {
    let meta = match fs::metadata(target) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    fs::set_permissions(temp, meta.permissions())?;
    Ok(())
}

struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn arm(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str, num: u32) -> EphemeralEntry {
        EphemeralEntry {
            session_id: id.to_string(),
            date: "2026-03-05T14:30:00Z".to_string(),
            duration: "10m".to_string(),
            message_count: num,
            archive_file: format!("conversation-{num:03}.md"),
            summary: format!("Session {id} summary"),
        }
    }

    #[test]
    fn append_to_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");

        append_entry(&path, &make_entry("aaa", 1)).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Session aaa"));
        assert!(content.contains("conversation-001.md"));
    }

    #[test]
    fn append_to_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");

        append_entry(&path, &make_entry("aaa", 1)).unwrap();
        append_entry(&path, &make_entry("bbb", 2)).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Session aaa"));
        assert!(content.contains("## Session bbb"));
        assert!(content.contains("\n---\n"));
    }

    #[test]
    fn parse_entries_basic() {
        let content = "## Session aaa\nstuff\n---\n\n## Session bbb\nmore stuff";
        let entries = parse_entries(content);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("aaa"));
        assert!(entries[1].contains("bbb"));
    }

    #[test]
    fn parse_entries_empty() {
        assert_eq!(parse_entries("").len(), 0);
        assert_eq!(parse_entries("  \n  ").len(), 0);
    }

    #[test]
    fn count_entries_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");

        assert_eq!(count_entries(&path).unwrap(), 0);

        append_entry(&path, &make_entry("a", 1)).unwrap();
        assert_eq!(count_entries(&path).unwrap(), 1);

        append_entry(&path, &make_entry("b", 2)).unwrap();
        assert_eq!(count_entries(&path).unwrap(), 2);
    }

    #[test]
    fn trim_below_limit_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");

        append_entry(&path, &make_entry("a", 1)).unwrap();
        append_entry(&path, &make_entry("b", 2)).unwrap();

        let before = fs::read_to_string(&path).unwrap();
        trim_to_limit(&path, 5).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn trim_at_limit_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");

        for i in 0..5 {
            append_entry(&path, &make_entry(&format!("s{i}"), i + 1)).unwrap();
        }

        assert_eq!(count_entries(&path).unwrap(), 5);
        trim_to_limit(&path, 5).unwrap();
        assert_eq!(count_entries(&path).unwrap(), 5);
    }

    #[test]
    fn trim_over_limit_removes_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");

        for i in 0..7 {
            append_entry(&path, &make_entry(&format!("s{i}"), i + 1)).unwrap();
        }

        assert_eq!(count_entries(&path).unwrap(), 7);
        trim_to_limit(&path, 5).unwrap();
        assert_eq!(count_entries(&path).unwrap(), 5);

        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("Session s0"));
        assert!(!content.contains("Session s1"));
        assert!(content.contains("Session s2"));
        assert!(content.contains("Session s6"));
    }

    #[test]
    fn trim_nonexistent_file_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");
        assert!(trim_to_limit(&path, 5).is_ok());
    }

    #[test]
    fn next_ephemeral_matches_append_then_trim() {
        let normal = make_entry("new", 9);
        let mut dashed = make_entry("dash", 9);
        dashed.summary = "before\n---\nafter".to_string();
        check_entry_against_oracle(&normal);
        check_entry_against_oracle(&dashed);
    }

    fn check_entry_against_oracle(entry: &EphemeralEntry) {
        for start in oracle_starts() {
            for max in 0..=6 {
                let got = next_ephemeral(start.as_deref(), &entry.render(), max);
                let expected = append_then_trim(start.as_deref(), entry, max);
                assert_eq!(got, expected, "max {max} start {start:?}");
            }
        }
    }

    fn oracle_starts() -> Vec<Option<String>> {
        let mut rendered = Vec::new();
        for i in 1..=6 {
            rendered.push(make_entry(&format!("s{i}"), i).render());
        }
        let mut dashed = make_entry("cut", 7);
        dashed.summary = "keep\n---\nthis".to_string();
        rendered.push(dashed.render());
        let mut states = vec![None, Some(String::new()), Some("  \n".to_string())];
        for n in 1..=6 {
            let body = rendered[..n].join(ENTRY_SEPARATOR);
            states.push(Some(format!("{body}\n")));
            states.push(Some(body.clone()));
            states.push(Some(format!("{body}\n\n")));
        }
        states.push(Some(format!("{}\n", dashed.render())));
        states
    }

    fn append_then_trim(start: Option<&str>, entry: &EphemeralEntry, max: usize) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");
        if let Some(text) = start {
            fs::write(&path, text).unwrap();
        }
        append_entry(&path, entry).unwrap();
        trim_to_limit(&path, max).unwrap();
        fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn record_entry_preserves_mode_and_a_symlinked_target() {
        assert_absent_mode_matches_write();
        assert_existing_mode_is_kept();
        assert_symlink_target_is_replaced();
    }

    fn mode_bits(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn assert_absent_mode_matches_write() {
        let tmp = tempfile::tempdir().unwrap();
        let via_write = tmp.path().join("via-write");
        fs::write(&via_write, "x\n").unwrap();
        let path = tmp.path().join("EPHEMERAL.md");
        record_entry(&path, &make_entry("a", 1), 5).unwrap();
        assert_eq!(mode_bits(&path), mode_bits(&via_write));
        assert_eq!(mode_bits(&tmp.path().join(".EPHEMERAL.lock")), 0o600);
        assert_no_temps(tmp.path());
    }

    fn assert_existing_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");
        fs::write(&path, "keep-mode\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let lock = tmp.path().join(".EPHEMERAL.lock");
        fs::write(&lock, "marker").unwrap();
        record_entry(&path, &make_entry("a", 1), 5).unwrap();
        assert_eq!(mode_bits(&path), 0o640);
        assert_eq!(fs::read(&lock).unwrap(), b"marker");
        assert_eq!(mode_bits(&lock), 0o600);
        assert_no_temps(tmp.path());
    }

    fn assert_symlink_target_is_replaced() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let real = real_dir.join("EPHEMERAL.md");
        fs::write(&real, "").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        let link = tmp.path().join("EPHEMERAL.md");
        symlink(&real, &link).unwrap();
        record_entry(&link, &make_entry("a", 1), 5).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let body = fs::read_to_string(&real).unwrap();
        assert!(body.contains("## Session a"), "{body}");
        assert_eq!(mode_bits(&real), 0o600);
        assert!(tmp.path().join(".EPHEMERAL.lock").is_file());
        assert!(!real_dir.join(".EPHEMERAL.lock").exists());
        assert_no_temps(tmp.path());
        assert_no_temps(&real_dir);
    }

    #[test]
    fn a_stale_ephemeral_temp_is_swept_under_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");
        fs::write(&path, "").unwrap();
        let stale = tmp.path().join(".EPHEMERAL.md.99.1.tmp");
        fs::write(&stale, "STALE-ENTRY should not be read\n").unwrap();
        record_entry(&path, &make_entry("a", 1), 5).unwrap();
        assert!(!stale.exists());
        let body = fs::read_to_string(&path).unwrap();
        assert!(!body.contains("STALE-ENTRY"), "{body}");
        assert!(body.contains("## Session a"), "{body}");
        assert_no_temps(tmp.path());
    }

    #[test]
    fn a_held_ephemeral_lock_times_out_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EPHEMERAL.md");
        fs::write(&path, "keep\n").unwrap();
        let lock = tmp.path().join(".EPHEMERAL.lock");
        fs::write(&lock, "held").unwrap();
        let _held = crate::file_lock::HeldLock::on(&lock);
        let err = record_entry_with(&path, "rendered", 5, Duration::from_millis(150)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&lock.display().to_string()), "{message}");
        assert!(
            message.contains("locked by another recall-echo process"),
            "{message}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep\n");
        assert_eq!(fs::read(&lock).unwrap(), b"held");
        assert_no_temps(tmp.path());
    }

    fn assert_no_temps(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!stale_temp_name(&name), "{name}");
        }
    }
}
