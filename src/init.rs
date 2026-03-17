//! Initialize the recall-echo memory system.
//!
//! Creates the directory structure and template files needed for
//! three-layer memory, and prompts for LLM provider configuration.

use std::fs;
use std::io::{self, BufRead, Write as _};
use std::path::Path;

use crate::config::{self, Config, LlmSection, Provider};

// ANSI color helpers
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
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

/// Prompt for LLM provider selection during init.
/// Returns None if stdin is not a terminal (non-interactive).
fn prompt_provider(reader: &mut dyn BufRead) -> Option<Provider> {
    // Check if stdin is a terminal
    if !atty_check() {
        return None;
    }

    eprintln!("\n{BOLD}LLM provider for entity extraction:{RESET}");
    eprintln!("  {BOLD}1{RESET}) anthropic   {DIM}— Claude API (default){RESET}");
    eprintln!("  {BOLD}2{RESET}) ollama      {DIM}— Local models via Ollama{RESET}");
    eprintln!(
        "  {BOLD}3{RESET}) claude-code {DIM}— In-session only, no standalone extraction{RESET}"
    );
    eprintln!(
        "  {BOLD}4{RESET}) skip        {DIM}— Configure later with `recall-echo config`{RESET}"
    );
    eprint!("\n  Choice [1]: ");
    io::stderr().flush().ok();

    let mut input = String::new();
    if reader.read_line(&mut input).is_err() {
        return None;
    }

    match input.trim() {
        "" | "1" | "anthropic" => Some(Provider::Anthropic),
        "2" | "ollama" => Some(Provider::Openai),
        "3" | "claude-code" => Some(Provider::ClaudeCode),
        "4" | "skip" => None,
        _ => {
            eprintln!("  {YELLOW}~{RESET} Unknown choice, defaulting to anthropic");
            Some(Provider::Anthropic)
        }
    }
}

/// Check if stderr is a terminal (for interactive prompts).
fn atty_check() -> bool {
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: std::os::raw::c_int) -> std::os::raw::c_int;
        }
        unsafe { isatty(2) != 0 }
    }
    #[cfg(not(unix))]
    {
        false
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
/// ├── .recall-echo.toml
/// └── conversations/
/// ```
pub fn run(entity_root: &Path) -> Result<(), String> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    run_with_reader(entity_root, &mut reader)
}

/// Testable init with injectable reader.
pub fn run_with_reader(entity_root: &Path, reader: &mut dyn BufRead) -> Result<(), String> {
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

    // Configure LLM provider if no config exists yet
    if !config::exists(&memory_dir) {
        if let Some(provider) = prompt_provider(reader) {
            let cfg = Config {
                llm: LlmSection {
                    provider: provider.clone(),
                    model: String::new(),
                    api_base: String::new(),
                },
                ..Config::default()
            };
            match config::save(&memory_dir, &cfg) {
                Ok(()) => {
                    let display_name = match &provider {
                        Provider::Anthropic => "anthropic",
                        Provider::Openai => "ollama (openai-compat)",
                        Provider::ClaudeCode => "claude-code",
                    };
                    print_status(
                        Status::Created,
                        &format!("Created .recall-echo.toml (provider: {display_name})"),
                    );
                }
                Err(e) => print_status(Status::Error, &format!("Failed to write config: {e}")),
            }
        } else {
            print_status(
                Status::Exists,
                "Skipped LLM config — run `recall-echo config set provider <name>` later",
            );
        }
    } else {
        print_status(
            Status::Exists,
            ".recall-echo.toml already exists — preserved",
        );
    }

    // Summary
    eprintln!(
        "\n{BOLD}Setup complete.{RESET} Memory system is ready.\n\n\
         \x20 Layer 1 (MEMORY.md)     — Curated facts, always in context\n\
         \x20 Layer 2 (EPHEMERAL.md)  — Rolling window of recent sessions (FIFO, max 5)\n\
         \x20 Layer 3 (Archive)       — Full conversations in memory/conversations/\n\n\
         \x20 Run `recall-echo status` to check memory health.\n\
         \x20 Run `recall-echo config show` to view configuration.\n"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn init_creates_directories_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mut reader = Cursor::new(b"4\n" as &[u8]); // skip provider prompt

        run_with_reader(&root, &mut reader).unwrap();

        assert!(root.join("memory/MEMORY.md").exists());
        assert!(root.join("memory/EPHEMERAL.md").exists());
        assert!(root.join("memory/ARCHIVE.md").exists());
        assert!(root.join("memory/conversations").exists());
    }

    #[test]
    fn init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mut reader = Cursor::new(b"4\n" as &[u8]);

        run_with_reader(&root, &mut reader).unwrap();
        fs::write(root.join("memory/MEMORY.md"), "custom content").unwrap();

        let mut reader2 = Cursor::new(b"4\n" as &[u8]);
        run_with_reader(&root, &mut reader2).unwrap();
        let content = fs::read_to_string(root.join("memory/MEMORY.md")).unwrap();
        assert_eq!(content, "custom content");
    }

    #[test]
    fn init_fails_if_root_missing() {
        let mut reader = Cursor::new(b"" as &[u8]);
        let result = run_with_reader(Path::new("/nonexistent/path"), &mut reader);
        assert!(result.is_err());
    }

    #[test]
    fn archive_template_has_header() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reader = Cursor::new(b"4\n" as &[u8]);
        run_with_reader(tmp.path(), &mut reader).unwrap();
        let content = fs::read_to_string(tmp.path().join("memory/ARCHIVE.md")).unwrap();
        assert!(content.contains("# Conversation Archive"));
        assert!(content.contains("| # | Date"));
    }
}
