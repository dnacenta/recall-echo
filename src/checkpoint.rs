use std::fs;
use std::path::Path;
use std::process::Command;

use crate::archive;
use crate::frontmatter::Frontmatter;
use crate::paths;

pub fn utc_now() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

pub fn log_body(log_num: u32) -> String {
    format!(
        "\n# Archive Log {log_num:03}\n\n\
         ## Summary\n\n\
         <!-- Fill in: What was discussed, decided, and accomplished -->\n\n\
         ## Key Details\n\n\
         <!-- Fill in: Important specifics — code changes, configurations, decisions -->\n\n\
         ## Action Items\n\n\
         <!-- Fill in: What needs to happen next -->\n\n\
         ## Unresolved\n\n\
         <!-- Fill in: Open questions or threads to pick up later -->\n"
    )
}

fn run_with_paths(
    memories: &Path,
    archive_index: &Path,
    trigger: &str,
    context: &str,
) -> Result<(), String> {
    if !memories.exists() {
        return Err("~/.claude/memories/ not found. Run `recall-echo init` first.".to_string());
    }

    let next_num = archive::highest_log_number(memories) + 1;
    let date = utc_now();
    let date_short = date.split('T').next().unwrap_or(&date);

    let fm = Frontmatter {
        log: next_num,
        date: date.clone(),
        trigger: trigger.to_string(),
        context: context.to_string(),
        topics: vec![],
    };

    let content = format!("{}\n{}", fm.render(), log_body(next_num));
    let log_path = memories.join(format!("archive-log-{next_num:03}.md"));

    fs::write(&log_path, &content).map_err(|e| format!("Failed to create archive log: {e}"))?;

    archive::append_index(archive_index, next_num, date_short, trigger)?;

    let path_display = log_path.to_string_lossy();
    println!("RECALL-ECHO checkpoint: {path_display}");
    println!("Trigger: {trigger} | Log: {next_num:03} | Date: {date}");
    println!("Fill in the Summary, Key Details, Action Items, and Unresolved sections.");

    Ok(())
}

pub fn run(trigger: &str, context: &str) -> Result<(), String> {
    let memories = paths::memories_dir()?;
    let archive_index = paths::archive_index()?;
    run_with_paths(&memories, &archive_index, trigger, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let memories = tmp.path().join("memories");
        let archive = tmp.path().join("ARCHIVE.md");
        fs::create_dir_all(&memories).unwrap();
        fs::write(&archive, "# Archive Index\n\n").unwrap();
        (tmp, memories, archive)
    }

    #[test]
    fn creates_logs_and_sequences() {
        let (_tmp, memories, archive) = setup_test_dirs();

        // First checkpoint
        run_with_paths(&memories, &archive, "precompact", "test session").unwrap();

        let log = memories.join("archive-log-001.md");
        assert!(log.exists());

        let content = fs::read_to_string(&log).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("log: 1"));
        assert!(content.contains("trigger: precompact"));
        assert!(content.contains("context: \"test session\""));
        assert!(content.contains("# Archive Log 001"));
        assert!(content.contains("## Summary"));

        let index = fs::read_to_string(&archive).unwrap();
        assert!(index.contains("| 001 |"));
        assert!(index.contains("| precompact |"));

        // Second checkpoint — should get next number
        run_with_paths(&memories, &archive, "session-end", "").unwrap();

        assert!(memories.join("archive-log-002.md").exists());

        let content = fs::read_to_string(memories.join("archive-log-002.md")).unwrap();
        assert!(content.contains("log: 2"));
        assert!(content.contains("trigger: session-end"));
    }
}
