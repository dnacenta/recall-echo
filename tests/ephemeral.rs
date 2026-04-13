//! Integration tests for EPHEMERAL.md FIFO window management.

use recall_echo::ephemeral::{self, EphemeralEntry};
use tempfile::TempDir;

fn make_entry(id: u32) -> EphemeralEntry {
    EphemeralEntry {
        session_id: format!("session-{id:03}"),
        date: format!("2026-04-{id:02}T10:00:00Z"),
        duration: "30m".to_string(),
        message_count: 10 + id,
        archive_file: format!("conversation-{id:03}.md"),
        summary: format!("Session {id} discussed topic {id}."),
    }
}

#[test]
fn append_to_empty_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("EPHEMERAL.md");

    ephemeral::append_entry(&path, &make_entry(1)).unwrap();

    let count = ephemeral::count_entries(&path).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn append_multiple_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("EPHEMERAL.md");

    for i in 1..=3 {
        ephemeral::append_entry(&path, &make_entry(i)).unwrap();
    }

    let count = ephemeral::count_entries(&path).unwrap();
    assert_eq!(count, 3);
}

#[test]
fn trim_evicts_oldest() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("EPHEMERAL.md");

    // Add 6 entries
    for i in 1..=6 {
        ephemeral::append_entry(&path, &make_entry(i)).unwrap();
    }

    assert_eq!(ephemeral::count_entries(&path).unwrap(), 6);

    // Trim to 5 (default max)
    ephemeral::trim_to_limit(&path, 5).unwrap();

    let count = ephemeral::count_entries(&path).unwrap();
    assert_eq!(count, 5, "should have trimmed to 5");

    // Verify oldest (session-001) was evicted
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains("session-001"),
        "oldest entry should be evicted"
    );
    assert!(
        content.contains("session-006"),
        "newest entry should remain"
    );
}

#[test]
fn trim_noop_when_under_limit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("EPHEMERAL.md");

    for i in 1..=3 {
        ephemeral::append_entry(&path, &make_entry(i)).unwrap();
    }

    ephemeral::trim_to_limit(&path, 5).unwrap();

    assert_eq!(ephemeral::count_entries(&path).unwrap(), 3);
}

#[test]
fn trim_nonexistent_file_is_ok() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("EPHEMERAL.md");

    // Should not error on missing file
    ephemeral::trim_to_limit(&path, 5).unwrap();
}

#[test]
fn count_nonexistent_file_is_zero() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("EPHEMERAL.md");

    assert_eq!(ephemeral::count_entries(&path).unwrap(), 0);
}

#[test]
fn parse_entries_empty_content() {
    let entries = ephemeral::parse_entries("");
    assert!(entries.is_empty());
}

#[test]
fn parse_entries_whitespace_only() {
    let entries = ephemeral::parse_entries("   \n\n  ");
    assert!(entries.is_empty());
}

#[test]
fn entry_render_contains_key_fields() {
    let entry = make_entry(42);
    let rendered = entry.render();

    assert!(rendered.contains("session-042"));
    assert!(rendered.contains("conversation-042.md"));
    assert!(rendered.contains("Session 42 discussed"));
}
