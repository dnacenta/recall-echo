use std::fs;
use std::path::Path;
use std::process::Command;

use crate::archive;
use crate::frontmatter::Frontmatter;
use crate::paths;

fn utc_now() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

fn run_with_paths(
    ephemeral: &Path,
    memories: &Path,
    archive_index: &Path,
    context: &str,
) -> Result<(), String> {
    if !memories.exists() {
        return Err("~/.claude/memories/ not found. Run `recall-echo init` first.".to_string());
    }

    // Read EPHEMERAL.md
    if !ephemeral.exists() {
        println!("RECALL-ECHO promote: nothing to promote (EPHEMERAL.md not found)");
        return Ok(());
    }

    let content =
        fs::read_to_string(ephemeral).map_err(|e| format!("Failed to read EPHEMERAL.md: {e}"))?;

    if content.trim().is_empty() {
        println!("RECALL-ECHO promote: nothing to promote (EPHEMERAL.md is empty)");
        return Ok(());
    }

    let next_num = archive::highest_log_number(memories) + 1;
    let date = utc_now();
    let date_short = date.split('T').next().unwrap_or(&date);

    // Use provided context or extract first line as context
    let ctx = if context.is_empty() {
        content
            .lines()
            .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .or_else(|| content.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_string()
    } else {
        context.to_string()
    };

    let fm = Frontmatter {
        log: next_num,
        date: date.clone(),
        trigger: "session-end".to_string(),
        context: ctx,
        topics: vec![],
    };

    let log_content = format!(
        "{}\n\n# Archive Log {next_num:03}\n\n{}",
        fm.render(),
        content
    );
    let log_path = memories.join(format!("archive-log-{next_num:03}.md"));

    fs::write(&log_path, &log_content).map_err(|e| format!("Failed to create archive log: {e}"))?;

    archive::append_index(archive_index, next_num, date_short, "session-end")?;

    // Clear EPHEMERAL.md
    fs::write(ephemeral, "").map_err(|e| format!("Failed to clear EPHEMERAL.md: {e}"))?;

    let path_display = log_path.to_string_lossy();
    println!("RECALL-ECHO promote: {path_display}");
    println!("Promoted EPHEMERAL.md → Log {next_num:03} | Date: {date_short}");

    Ok(())
}

pub fn run(context: &str) -> Result<(), String> {
    let ephemeral = paths::ephemeral_file()?;
    let memories = paths::memories_dir()?;
    let archive_index = paths::archive_index()?;
    run_with_paths(&ephemeral, &memories, &archive_index, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dirs() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let memories = tmp.path().join("memories");
        let archive = tmp.path().join("ARCHIVE.md");
        let ephemeral = tmp.path().join("EPHEMERAL.md");
        fs::create_dir_all(&memories).unwrap();
        fs::write(&archive, "# Archive Index\n\n").unwrap();
        (tmp, memories, archive, ephemeral)
    }

    #[test]
    fn promotes_ephemeral_to_archive() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        let session_summary = "## Summary\nWorked on recall-echo promote command.\n\n## Action Items\n- Write tests\n";
        fs::write(&ephemeral, session_summary).unwrap();

        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        // Archive log created
        let log = memories.join("archive-log-001.md");
        assert!(log.exists());

        let content = fs::read_to_string(&log).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("log: 1"));
        assert!(content.contains("trigger: session-end"));
        assert!(content.contains("# Archive Log 001"));
        assert!(content.contains("Worked on recall-echo promote command."));
        assert!(content.contains("Write tests"));

        // ARCHIVE.md updated
        let index = fs::read_to_string(&archive).unwrap();
        assert!(index.contains("| 001 |"));
        assert!(index.contains("| session-end |"));

        // EPHEMERAL.md cleared
        let eph = fs::read_to_string(&ephemeral).unwrap();
        assert!(eph.is_empty());
    }

    #[test]
    fn skips_empty_ephemeral() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        fs::write(&ephemeral, "").unwrap();
        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        // No archive log created
        assert_eq!(archive::highest_log_number(&memories), 0);
    }

    #[test]
    fn skips_whitespace_only_ephemeral() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        fs::write(&ephemeral, "   \n\n  \n").unwrap();
        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        assert_eq!(archive::highest_log_number(&memories), 0);
    }

    #[test]
    fn skips_missing_ephemeral() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        // Don't create EPHEMERAL.md
        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        assert_eq!(archive::highest_log_number(&memories), 0);
    }

    #[test]
    fn sequences_after_existing_logs() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        // Pre-existing log
        fs::write(memories.join("archive-log-003.md"), "existing").unwrap();

        fs::write(&ephemeral, "# Session\nDid some work.").unwrap();
        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        assert!(memories.join("archive-log-004.md").exists());
        let content = fs::read_to_string(memories.join("archive-log-004.md")).unwrap();
        assert!(content.contains("log: 4"));
    }

    #[test]
    fn uses_explicit_context() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        fs::write(&ephemeral, "Some session content.").unwrap();
        run_with_paths(&ephemeral, &memories, &archive, "custom context").unwrap();

        let log = memories.join("archive-log-001.md");
        let content = fs::read_to_string(&log).unwrap();
        assert!(content.contains("context: \"custom context\""));
    }

    #[test]
    fn extracts_context_from_content() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        fs::write(&ephemeral, "# Last Session\nImplemented promote command.\n").unwrap();
        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        let log = memories.join("archive-log-001.md");
        let content = fs::read_to_string(&log).unwrap();
        // Prefers first non-heading line as context
        assert!(content.contains("context: \"Implemented promote command.\""));
    }

    #[test]
    fn double_promote_is_noop() {
        let (_tmp, memories, archive, ephemeral) = setup_test_dirs();

        fs::write(&ephemeral, "Session content.").unwrap();

        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();
        assert!(memories.join("archive-log-001.md").exists());

        // Second promote — EPHEMERAL is now empty
        run_with_paths(&ephemeral, &memories, &archive, "").unwrap();

        // No second log created
        assert!(!memories.join("archive-log-002.md").exists());
    }
}
