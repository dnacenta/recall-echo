use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::paths;

const PROTOCOL_TEMPLATE: &str = include_str!("../templates/recall-echo.md");

const MEMORY_TEMPLATE: &str = "# Memory\n\n\
<!-- recall-echo: Curated memory. Distilled facts, preferences, patterns. -->\n\
<!-- Keep under 200 lines. Only write confirmed, stable information. -->\n";

const ARCHIVE_TEMPLATE: &str = "# Archive Index\n\n\
<!-- recall-echo: Lightweight index of archive logs. -->\n\
<!-- Format: | log number | date | key topics | -->\n";

const CHECKPOINT_COMMAND: &str = "recall-echo checkpoint --trigger precompact";

const LEGACY_MARKER: &str = "RECALL-ECHO";

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

    // Check existing hooks for recall-echo entries
    let mut has_checkpoint = false;
    let mut has_legacy = false;

    if let Some(hooks) = settings.get("hooks") {
        if let Some(precompact) = hooks.get("PreCompact") {
            if let Some(arr) = precompact.as_array() {
                for entry in arr {
                    if let Some(inner_hooks) = entry.get("hooks") {
                        if let Some(inner_arr) = inner_hooks.as_array() {
                            for hook in inner_arr {
                                if let Some(cmd) = hook.get("command") {
                                    if let Some(s) = cmd.as_str() {
                                        if s.contains(CHECKPOINT_COMMAND) {
                                            has_checkpoint = true;
                                        } else if s.contains(LEGACY_MARKER) {
                                            has_legacy = true;
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

    if has_checkpoint {
        print_status(Status::Exists, "PreCompact hook already up to date");
        return;
    }

    if has_legacy {
        // Migrate: remove legacy hook entries, add new checkpoint hook
        if let Some(hooks) = settings.get_mut("hooks") {
            if let Some(precompact) = hooks.get_mut("PreCompact") {
                if let Some(arr) = precompact.as_array_mut() {
                    // Remove entries containing the legacy RECALL-ECHO marker
                    arr.retain(|entry| {
                        if let Some(inner_hooks) = entry.get("hooks") {
                            if let Some(inner_arr) = inner_hooks.as_array() {
                                for hook in inner_arr {
                                    if let Some(cmd) = hook.get("command") {
                                        if let Some(s) = cmd.as_str() {
                                            if s.contains(LEGACY_MARKER) {
                                                return false;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        true
                    });
                }
            }
        }

        // Add new checkpoint hook
        let hook_entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": CHECKPOINT_COMMAND
            }]
        });

        settings["hooks"]["PreCompact"]
            .as_array_mut()
            .unwrap()
            .push(hook_entry);

        match serde_json::to_string_pretty(&settings) {
            Ok(json) => match fs::write(settings_path, format!("{json}\n")) {
                Ok(()) => print_status(
                    Status::Created,
                    "Migrated PreCompact hook: echo → checkpoint",
                ),
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
        return;
    }

    // No recall-echo hook at all — add fresh
    let hook_entry = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": CHECKPOINT_COMMAND
        }]
    });

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
    let claude = paths::claude_dir()?;

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
    let rules_dir = paths::rules_dir()?;
    let memory_dir = claude.join("memory");
    let memories_dir = paths::memories_dir()?;
    ensure_dir(&rules_dir);
    ensure_dir(&memory_dir);
    ensure_dir(&memories_dir);

    // Write protocol rules file
    write_protocol(&paths::protocol_file()?);

    // Write MEMORY.md (never overwrite)
    write_if_not_exists(&paths::memory_file()?, MEMORY_TEMPLATE, "MEMORY.md");

    // Write EPHEMERAL.md (never overwrite)
    write_if_not_exists(&paths::ephemeral_file()?, "", "EPHEMERAL.md");

    // Write ARCHIVE.md (never overwrite)
    write_if_not_exists(&paths::archive_index()?, ARCHIVE_TEMPLATE, "ARCHIVE.md");

    // Merge PreCompact hook
    merge_precompact_hook(&paths::settings_file()?);

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
