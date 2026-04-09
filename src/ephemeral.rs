use std::fs;
use std::path::Path;

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
}
