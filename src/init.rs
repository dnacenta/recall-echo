//! Initialize the recall-echo memory system for a pulse-null entity.
//!
//! Creates the directory structure and template files needed for
//! three-layer memory. No hooks, no settings.json — pulse-null
//! calls recall-echo through the plugin adapter.

use std::fs;
use std::path::Path;

// ANSI color helpers
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

const MEMORY_TEMPLATE: &str = "# Memory\n\n\
<!-- recall-echo: Curated memory. Distilled facts, preferences, patterns. -->\n\
<!-- Keep under 200 lines. Only write confirmed, stable information. -->\n";

const ARCHIVE_TEMPLATE: &str = "# Conversation Archive\n\n\
| # | Date | Session | Topics | Messages | Duration |\n\
|---|------|---------|--------|----------|----------|\n";

enum Status {
    Created,
    Exists,
    Error,
}

fn print_status(status: Status, msg: &str) {
    match status {
        Status::Created => eprintln!("  {GREEN}✓{RESET} {msg}"),
        Status::Exists => eprintln!("  {YELLOW}~{RESET} {msg}"),
        Status::Error => eprintln!("  {RED}✗{RESET} {msg}"),
    }
}

fn ensure_dir(path: &Path) {
    if !path.exists() {
        if let Err(e) = fs::create_dir_all(path) {
            print_status(
                Status::Error,
                &format!("Failed to create {}: {e}", path.display()),
            );
        }
    }
}

fn write_if_not_exists(path: &Path, content: &str, label: &str) {
    if path.exists() {
        print_status(
            Status::Exists,
            &format!("{label} already exists — preserved"),
        );
    } else {
        match fs::write(path, content) {
            Ok(()) => print_status(Status::Created, &format!("Created {label}")),
            Err(e) => print_status(Status::Error, &format!("Failed to create {label}: {e}")),
        }
    }
}

/// Initialize memory structure at the given entity root.
///
/// Creates:
/// ```text
/// {entity_root}/memory/
/// ├── MEMORY.md
/// ├── EPHEMERAL.md
/// ├── ARCHIVE.md
/// └── conversations/
/// ```
pub fn run(entity_root: &Path) -> Result<(), String> {
    if !entity_root.exists() {
        return Err(format!(
            "Entity root not found: {}\n  Create the directory first or run `pulse-null init`.",
            entity_root.display()
        ));
    }

    eprintln!("\n{BOLD}recall-echo{RESET} — initializing memory system\n");

    let memory_dir = entity_root.join("memory");
    let conversations_dir = memory_dir.join("conversations");
    ensure_dir(&memory_dir);
    ensure_dir(&conversations_dir);

    // Write MEMORY.md (never overwrite)
    write_if_not_exists(&memory_dir.join("MEMORY.md"), MEMORY_TEMPLATE, "MEMORY.md");

    // Write EPHEMERAL.md (never overwrite)
    write_if_not_exists(&memory_dir.join("EPHEMERAL.md"), "", "EPHEMERAL.md");

    // Write ARCHIVE.md (never overwrite)
    write_if_not_exists(
        &memory_dir.join("ARCHIVE.md"),
        ARCHIVE_TEMPLATE,
        "ARCHIVE.md",
    );

    // Summary
    eprintln!(
        "\n{BOLD}Setup complete.{RESET} Memory system is ready.\n\n\
         \x20 Layer 1 (MEMORY.md)     — Curated facts, always in context\n\
         \x20 Layer 2 (EPHEMERAL.md)  — Rolling window of recent sessions (FIFO, max 5)\n\
         \x20 Layer 3 (Archive)       — Full conversations in memory/conversations/\n\n\
         \x20 pulse-null manages the lifecycle automatically.\n\
         \x20 Run `recall-echo status` to check memory health.\n"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_directories_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        run(&root).unwrap();

        assert!(root.join("memory/MEMORY.md").exists());
        assert!(root.join("memory/EPHEMERAL.md").exists());
        assert!(root.join("memory/ARCHIVE.md").exists());
        assert!(root.join("memory/conversations").exists());
    }

    #[test]
    fn init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        run(&root).unwrap();
        fs::write(root.join("memory/MEMORY.md"), "custom content").unwrap();

        run(&root).unwrap();
        let content = fs::read_to_string(root.join("memory/MEMORY.md")).unwrap();
        assert_eq!(content, "custom content");
    }

    #[test]
    fn init_fails_if_root_missing() {
        let result = run(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn archive_template_has_header() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("memory/ARCHIVE.md")).unwrap();
        assert!(content.contains("# Conversation Archive"));
        assert!(content.contains("| # | Date"));
    }
}
