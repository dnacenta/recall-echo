use std::fs;
use std::path::Path;

/// Scan the memories directory and return the highest archive-log-XXX number found.
/// Returns 0 if no logs exist.
pub fn highest_log_number(memories_dir: &Path) -> u32 {
    let entries = match fs::read_dir(memories_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut max = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(num_str) = name
            .strip_prefix("archive-log-")
            .and_then(|s| s.strip_suffix(".md"))
        {
            if let Ok(n) = num_str.parse::<u32>() {
                if n > max {
                    max = n;
                }
            }
        }
    }

    max
}

/// Append an entry to the ARCHIVE.md index file.
pub fn append_index(
    archive_path: &Path,
    log_num: u32,
    date: &str,
    trigger: &str,
) -> Result<(), String> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive_path)
        .map_err(|e| format!("Failed to open ARCHIVE.md: {e}"))?;

    writeln!(file, "| {log_num:03} | {date} | {trigger} |")
        .map_err(|e| format!("Failed to write to ARCHIVE.md: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn highest_from_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(highest_log_number(tmp.path()), 0);
    }

    #[test]
    fn highest_from_sequential_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("archive-log-001.md"), "").unwrap();
        fs::write(tmp.path().join("archive-log-002.md"), "").unwrap();
        fs::write(tmp.path().join("archive-log-003.md"), "").unwrap();
        assert_eq!(highest_log_number(tmp.path()), 3);
    }

    #[test]
    fn highest_with_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("archive-log-001.md"), "").unwrap();
        fs::write(tmp.path().join("archive-log-005.md"), "").unwrap();
        fs::write(tmp.path().join("archive-log-010.md"), "").unwrap();
        assert_eq!(highest_log_number(tmp.path()), 10);
    }

    #[test]
    fn highest_ignores_non_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("archive-log-003.md"), "").unwrap();
        fs::write(tmp.path().join("notes.md"), "").unwrap();
        fs::write(tmp.path().join("archive-log-bad.md"), "").unwrap();
        assert_eq!(highest_log_number(tmp.path()), 3);
    }

    #[test]
    fn append_index_creates_and_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("ARCHIVE.md");
        fs::write(&index, "# Archive Index\n\n").unwrap();

        append_index(&index, 1, "2026-02-24", "precompact").unwrap();
        append_index(&index, 2, "2026-02-24", "session-end").unwrap();

        let content = fs::read_to_string(&index).unwrap();
        assert!(content.contains("| 001 | 2026-02-24 | precompact |"));
        assert!(content.contains("| 002 | 2026-02-24 | session-end |"));
    }
}
