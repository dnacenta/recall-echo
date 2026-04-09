//! Initialize the recall-echo memory system.
//!
//! Creates the directory structure and template files needed for
//! four-layer memory (graph, curated, short-term, long-term), hooks, and LLM provider config.

use std::fs;
use std::io::{self, BufRead, Write as _};
use std::path::Path;

use crate::config::{self, Config, LlmSection, Provider};
use crate::error::RecallError;
use crate::paths;

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
        // Non-interactive: default to claude-code if detected, else anthropic
        return if paths::detect_claude_code().is_some() {
            Some(Provider::ClaudeCode)
        } else {
            Some(Provider::Anthropic)
        };
    }

    let is_cc = paths::detect_claude_code().is_some();
    let default_label = if is_cc { "3" } else { "1" };

    eprintln!("\n{BOLD}LLM provider for entity extraction:{RESET}");
    eprintln!(
        "  {BOLD}1{RESET}) anthropic   {DIM}— Claude API{}",
        if !is_cc { " (default)" } else { "" }
    );
    eprintln!("  {BOLD}2{RESET}) ollama      {DIM}— Local models via Ollama{RESET}");
    eprintln!(
        "  {BOLD}3{RESET}) claude-code {DIM}— Uses `claude -p` subprocess{}",
        if is_cc { " (default)" } else { "" }
    );
    eprintln!(
        "  {BOLD}4{RESET}) skip        {DIM}— Configure later with `recall-echo config`{RESET}"
    );
    eprint!("\n  Choice [{default_label}]: ");
    io::stderr().flush().ok();

    let mut input = String::new();
    if reader.read_line(&mut input).is_err() {
        return None;
    }

    match input.trim() {
        "" => {
            if is_cc {
                Some(Provider::ClaudeCode)
            } else {
                Some(Provider::Anthropic)
            }
        }
        "1" | "anthropic" => Some(Provider::Anthropic),
        "2" | "ollama" => Some(Provider::Openai),
        "3" | "claude-code" => Some(Provider::ClaudeCode),
        "4" | "skip" => None,
        _ => {
            let default = if is_cc {
                Provider::ClaudeCode
            } else {
                Provider::Anthropic
            };
            eprintln!("  {YELLOW}~{RESET} Unknown choice, defaulting to {default}");
            Some(default)
        }
    }
}

/// Configure LLM provider. Returns true if the chosen provider is claude-code
/// (indicating this is likely a Claude Code user).
fn configure_llm(reader: &mut dyn BufRead, memory_dir: &Path) -> bool {
    if !config::exists(memory_dir) {
        if let Some(provider) = prompt_provider(reader) {
            let is_cc = provider == Provider::ClaudeCode;
            let cfg = Config {
                llm: LlmSection {
                    provider: provider.clone(),
                    model: String::new(),
                    api_base: String::new(),
                },
                ..Config::default()
            };
            match config::save(memory_dir, &cfg) {
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
            return is_cc;
        }
        print_status(
            Status::Exists,
            "Skipped LLM config — run `recall-echo config set provider <name>` later",
        );
    } else {
        print_status(
            Status::Exists,
            ".recall-echo.toml already exists — preserved",
        );
        // Check existing config
        let cfg = config::load(memory_dir);
        return cfg.llm.provider == Provider::ClaudeCode;
    }
    false
}

/// Initialize the graph store in memory/graph/.
fn init_graph(memory_dir: &Path) {
    let graph_dir = memory_dir.join("graph");
    if graph_dir.exists() {
        print_status(Status::Exists, "graph/ already exists — preserved");
        return;
    }

    match tokio::runtime::Runtime::new() {
        Ok(rt) => match rt.block_on(crate::graph::GraphMemory::open(&graph_dir)) {
            Ok(_) => print_status(Status::Created, "Created graph/ (SurrealDB + fastembed)"),
            Err(e) => print_status(Status::Error, &format!("Failed to init graph: {e}")),
        },
        Err(e) => print_status(Status::Error, &format!("Failed to start runtime: {e}")),
    }
}

/// Auto-configure Claude Code hooks (settings.json).
/// Returns true if hooks were configured.
/// Hooks always go in ~/.claude/settings.json regardless of where entity_root is.
fn configure_hooks(_entity_root: &Path) -> bool {
    let claude_dir = match paths::detect_claude_code() {
        Some(dir) => dir,
        None => return false,
    };

    let settings_path = claude_dir.join("settings.json");
    let recall_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "recall-echo".into());

    let archive_cmd = format!("{recall_bin} archive-session");
    let checkpoint_cmd = format!("{recall_bin} checkpoint --trigger precompact");

    // Load existing settings or start fresh
    let mut settings: serde_json::Value = if settings_path.exists() {
        fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hooks = settings.as_object_mut().and_then(|o| {
        o.entry("hooks")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
    });

    let hooks = match hooks {
        Some(h) => h,
        None => {
            print_status(Status::Error, "Could not parse settings.json hooks");
            return false;
        }
    };

    let mut changed = false;

    // Add SessionEnd hook if not already present
    if !hook_exists(hooks, "SessionEnd", &archive_cmd) {
        let arr = hooks
            .entry("SessionEnd")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut();
        if let Some(arr) = arr {
            arr.push(serde_json::json!({
                "hooks": [{"type": "command", "command": archive_cmd}]
            }));
            changed = true;
        }
    }

    // Add PreCompact hook if not already present
    if !hook_exists(hooks, "PreCompact", &checkpoint_cmd) {
        let arr = hooks
            .entry("PreCompact")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut();
        if let Some(arr) = arr {
            arr.push(serde_json::json!({
                "hooks": [{"type": "command", "command": checkpoint_cmd}]
            }));
            changed = true;
        }
    }

    if changed {
        match serde_json::to_string_pretty(&settings) {
            Ok(content) => match fs::write(&settings_path, content) {
                Ok(()) => {
                    print_status(
                        Status::Created,
                        "Configured SessionEnd + PreCompact hooks in settings.json",
                    );
                    return true;
                }
                Err(e) => print_status(
                    Status::Error,
                    &format!("Failed to write settings.json: {e}"),
                ),
            },
            Err(e) => print_status(Status::Error, &format!("Failed to serialize settings: {e}")),
        }
    } else {
        print_status(Status::Exists, "Hooks already configured in settings.json");
        return true;
    }

    false
}

/// Check if a hook command already exists in a hook event array.
fn hook_exists(
    hooks: &serde_json::Map<String, serde_json::Value>,
    event: &str,
    command: &str,
) -> bool {
    if let Some(arr) = hooks.get(event).and_then(|v| v.as_array()) {
        for group in arr {
            if let Some(inner) = group.get("hooks").and_then(|h| h.as_array()) {
                for hook in inner {
                    if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                        // Match on the base command name, not the full path
                        if cmd.contains("recall-echo archive-session")
                            && command.contains("archive-session")
                        {
                            return true;
                        }
                        if cmd.contains("recall-echo checkpoint") && command.contains("checkpoint")
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Check if stderr is a terminal (for interactive prompts).
fn atty_check() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
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
pub fn run(entity_root: &Path) -> Result<(), RecallError> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    run_with_reader(entity_root, &mut reader)
}

/// Testable init with injectable reader.
pub fn run_with_reader(entity_root: &Path, reader: &mut dyn BufRead) -> Result<(), RecallError> {
    if !entity_root.exists() {
        return Err(RecallError::NotInitialized(format!(
            "Directory not found: {}\n  Create the directory first, or run from a valid path.",
            entity_root.display()
        )));
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

    // Initialize graph store
    init_graph(&memory_dir);

    // Configure LLM provider if no config exists yet
    let is_claude_code = configure_llm(reader, &memory_dir);

    // Auto-configure Claude Code hooks if applicable
    let hooks_configured = if is_claude_code {
        configure_hooks(entity_root)
    } else {
        false
    };

    // Summary
    eprintln!("\n{BOLD}Setup complete.{RESET} Memory system is ready.\n");
    eprintln!("  Layer 1 (MEMORY.md)     — Curated facts, always in context");
    eprintln!("  Layer 2 (EPHEMERAL.md)  — Rolling window of recent sessions (FIFO, max 5)");
    eprintln!("  Layer 3 (Archive)       — Full conversations in memory/conversations/");
    eprintln!("  Layer 0 (Graph)         — Knowledge graph with semantic search");
    eprintln!();
    eprintln!("  Run `recall-echo status` to check memory health.");
    eprintln!("  Run `recall-echo config show` to view configuration.");
    if hooks_configured {
        eprintln!("  Hooks configured — archiving happens automatically.");
    }
    eprintln!();

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
