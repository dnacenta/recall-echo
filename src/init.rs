use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

const PROTOCOL_TEMPLATE: &str = include_str!("../templates/recall-echo.md");

const MEMORY_TEMPLATE: &str = "# Memory\n\n\
<!-- recall-echo: Curated memory. Distilled facts, preferences, patterns. -->\n\
<!-- Keep under 200 lines. Only write confirmed, stable information. -->\n";

const ARCHIVE_TEMPLATE: &str = "# Archive Index\n\n\
<!-- recall-echo: Lightweight index of archive logs. -->\n\
<!-- Format: | log number | date | key topics | -->\n";

const PRECOMPACT_COMMAND: &str = "echo 'RECALL-ECHO: Context compaction imminent. Save a memory checkpoint to ~/.claude/memories/ before context is lost. Check the highest archive-log-XXX.md number and create the next one.'";

// ANSI color helpers
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

enum Status {
    Created,
    Exists,
    Error,
}

fn print_status(status: Status, msg: &str) {
    match status {
        Status::Created => println!("  {GREEN}✓{RESET} {msg}"),
        Status::Exists => println!("  {YELLOW}~{RESET} {msg}"),
        Status::Error => println!("  {RED}✗{RESET} {msg}"),
    }
}

fn claude_dir() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    Ok(home.join(".claude"))
}

fn confirm(question: &str) -> bool {
    print!("  {question} [y/N] ");
    io::stdout().flush().ok();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn ensure_dir(path: &PathBuf) {
    if !path.exists() {
        fs::create_dir_all(path).ok();
    }
}

fn write_if_not_exists(path: &PathBuf, content: &str, label: &str) {
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

fn write_protocol(path: &PathBuf) {
    if path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing == PROTOCOL_TEMPLATE {
            print_status(Status::Exists, "Memory protocol already up to date");
            return;
        }
        if !confirm("Memory protocol already exists but differs from latest. Overwrite?") {
            print_status(Status::Exists, "Kept existing memory protocol");
            return;
        }
    }
    match fs::write(path, PROTOCOL_TEMPLATE) {
        Ok(()) => print_status(
            Status::Created,
            "Created memory protocol (~/.claude/rules/recall-echo.md)",
        ),
        Err(e) => print_status(
            Status::Error,
            &format!("Failed to write memory protocol: {e}"),
        ),
    }
}

fn merge_precompact_hook(settings_path: &PathBuf) {
    let mut settings: serde_json::Value = if settings_path.exists() {
        match fs::read_to_string(settings_path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => {
                    print_status(
                        Status::Error,
                        "Could not parse settings.json — add PreCompact hook manually",
                    );
                    return;
                }
            },
            Err(_) => {
                print_status(
                    Status::Error,
                    "Could not read settings.json — add PreCompact hook manually",
                );
                return;
            }
        }
    } else {
        serde_json::json!({})
    };

    // Check if RECALL-ECHO hook already exists
    if let Some(hooks) = settings.get("hooks") {
        if let Some(precompact) = hooks.get("PreCompact") {
            if let Some(arr) = precompact.as_array() {
                for entry in arr {
                    if let Some(inner_hooks) = entry.get("hooks") {
                        if let Some(inner_arr) = inner_hooks.as_array() {
                            for hook in inner_arr {
                                if let Some(cmd) = hook.get("command") {
                                    if let Some(s) = cmd.as_str() {
                                        if s.contains("RECALL-ECHO") {
                                            print_status(
                                                Status::Exists,
                                                "PreCompact hook already configured",
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Build the hook entry
    let hook_entry = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": PRECOMPACT_COMMAND
        }]
    });

    // Merge into settings
    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let precompact = hooks
        .as_object_mut()
        .unwrap()
        .entry("PreCompact")
        .or_insert_with(|| serde_json::json!([]));
    precompact.as_array_mut().unwrap().push(hook_entry);

    // Write back
    match serde_json::to_string_pretty(&settings) {
        Ok(json) => match fs::write(settings_path, format!("{json}\n")) {
            Ok(()) => print_status(Status::Created, "Added PreCompact hook to settings.json"),
            Err(e) => print_status(
                Status::Error,
                &format!("Failed to write settings.json: {e}"),
            ),
        },
        Err(e) => print_status(
            Status::Error,
            &format!("Failed to serialize settings.json: {e}"),
        ),
    }
}

pub fn run() -> Result<(), String> {
    let claude = claude_dir()?;

    // Pre-flight check
    if !claude.exists() {
        return Err(
            "~/.claude directory not found. Is Claude Code installed?\n  \
             Install Claude Code first, then run this again."
                .to_string(),
        );
    }

    println!("\n{BOLD}recall-echo{RESET} — initializing memory system\n");

    // Create directories
    let rules_dir = claude.join("rules");
    let memory_dir = claude.join("memory");
    let memories_dir = claude.join("memories");
    ensure_dir(&rules_dir);
    ensure_dir(&memory_dir);
    ensure_dir(&memories_dir);

    // Write protocol rules file
    write_protocol(&rules_dir.join("recall-echo.md"));

    // Write MEMORY.md (never overwrite)
    write_if_not_exists(&memory_dir.join("MEMORY.md"), MEMORY_TEMPLATE, "MEMORY.md");

    // Write EPHEMERAL.md (never overwrite)
    write_if_not_exists(&claude.join("EPHEMERAL.md"), "", "EPHEMERAL.md");

    // Write ARCHIVE.md (never overwrite)
    write_if_not_exists(&claude.join("ARCHIVE.md"), ARCHIVE_TEMPLATE, "ARCHIVE.md");

    // Merge PreCompact hook
    merge_precompact_hook(&claude.join("settings.json"));

    // Summary
    println!(
        "\n{BOLD}Setup complete.{RESET} Your memory system is ready.\n\n\
         \x20 Layer 1 (MEMORY.md)     — Curated facts, always in context\n\
         \x20 Layer 2 (EPHEMERAL.md)  — Last session summary, read then cleared\n\
         \x20 Layer 3 (Archive)       — Searchable history in ~/.claude/memories/\n\n\
         \x20 The memory protocol loads automatically via ~/.claude/rules/recall-echo.md\n\
         \x20 Start a new Claude Code session and your agent will have persistent memory.\n"
    );

    Ok(())
}
