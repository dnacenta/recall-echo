use std::fs;
use std::path::Path;

use crate::paths;

const PROTOCOL_TEMPLATE: &str = include_str!("../templates/recall-echo.md");

const MEMORY_TEMPLATE: &str = "# Memory\n\n\
<!-- recall-echo: Curated memory. Distilled facts, preferences, patterns. -->\n\
<!-- Keep under 200 lines. Only write confirmed, stable information. -->\n";

const ARCHIVE_TEMPLATE: &str = "# Conversation Archive\n\n\
| # | Date | Session | Topics | Messages | Duration |\n\
|---|------|---------|--------|----------|----------|\n";

const ARCHIVE_SESSION_COMMAND: &str = "recall-echo archive-session";
const CHECKPOINT_COMMAND: &str = "recall-echo checkpoint --trigger precompact";
const CONSUME_COMMAND: &str = "recall-echo consume";

// Legacy commands to detect and migrate
const LEGACY_PROMOTE_COMMAND: &str = "recall-echo promote";

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

fn write_protocol(path: &Path) {
    if path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing == PROTOCOL_TEMPLATE {
            print_status(Status::Exists, "Memory protocol already up to date");
            return;
        }
        eprintln!("  {YELLOW}~{RESET} Updating memory protocol to latest version");
    }
    match fs::write(path, PROTOCOL_TEMPLATE) {
        Ok(()) => print_status(
            Status::Created,
            "Installed memory protocol (rules/recall-echo.md)",
        ),
        Err(e) => print_status(
            Status::Error,
            &format!("Failed to write memory protocol: {e}"),
        ),
    }
}

/// Check if a hook event already contains a command substring.
fn hook_has_command(settings: &serde_json::Value, event: &str, needle: &str) -> bool {
    if let Some(hooks) = settings.get("hooks") {
        if let Some(event_hooks) = hooks.get(event) {
            if let Some(arr) = event_hooks.as_array() {
                for entry in arr {
                    if let Some(inner_hooks) = entry.get("hooks") {
                        if let Some(inner_arr) = inner_hooks.as_array() {
                            for hook in inner_arr {
                                if let Some(cmd) = hook.get("command") {
                                    if let Some(s) = cmd.as_str() {
                                        if s.contains(needle) {
                                            return true;
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
    false
}

/// Add a hook entry to a given event, creating the hooks/event structure if needed.
fn add_hook_entry(settings: &mut serde_json::Value, event: &str, command: &str) {
    let hook_entry = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": command
        }]
    });

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let event_arr = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| serde_json::json!([]));
    event_arr.as_array_mut().unwrap().push(hook_entry);
}

/// Remove hook entries containing a specific command needle from an event.
fn remove_hook_entries(settings: &mut serde_json::Value, event: &str, needle: &str) {
    if let Some(hooks) = settings.get_mut("hooks") {
        if let Some(event_hooks) = hooks.get_mut(event) {
            if let Some(arr) = event_hooks.as_array_mut() {
                arr.retain(|entry| {
                    if let Some(inner_hooks) = entry.get("hooks") {
                        if let Some(inner_arr) = inner_hooks.as_array() {
                            for hook in inner_arr {
                                if let Some(cmd) = hook.get("command") {
                                    if let Some(s) = cmd.as_str() {
                                        if s.contains(needle) {
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
}

fn merge_hooks(settings_path: &Path) {
    let mut settings: serde_json::Value = if settings_path.exists() {
        match fs::read_to_string(settings_path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => {
                    print_status(
                        Status::Error,
                        "Could not parse settings.json — add hooks manually",
                    );
                    return;
                }
            },
            Err(_) => {
                print_status(
                    Status::Error,
                    "Could not read settings.json — add hooks manually",
                );
                return;
            }
        }
    } else {
        serde_json::json!({})
    };

    let mut changed = false;

    // --- SessionEnd hook: archive-session ---
    let has_archive = hook_has_command(&settings, "SessionEnd", ARCHIVE_SESSION_COMMAND);
    let has_legacy_promote = hook_has_command(&settings, "SessionEnd", LEGACY_PROMOTE_COMMAND);

    if has_archive {
        print_status(Status::Exists, "SessionEnd hook already up to date");
    } else if has_legacy_promote {
        // Migrate: remove promote, add archive-session
        remove_hook_entries(&mut settings, "SessionEnd", LEGACY_PROMOTE_COMMAND);
        add_hook_entry(&mut settings, "SessionEnd", ARCHIVE_SESSION_COMMAND);
        print_status(
            Status::Created,
            "Migrated SessionEnd hook: promote → archive-session",
        );
        changed = true;
    } else {
        add_hook_entry(&mut settings, "SessionEnd", ARCHIVE_SESSION_COMMAND);
        print_status(
            Status::Created,
            "Added SessionEnd hook (conversation archiving)",
        );
        changed = true;
    }

    // --- PreCompact hook: checkpoint ---
    let has_checkpoint = hook_has_command(&settings, "PreCompact", CHECKPOINT_COMMAND);
    if has_checkpoint {
        print_status(Status::Exists, "PreCompact hook already up to date");
    } else {
        add_hook_entry(&mut settings, "PreCompact", CHECKPOINT_COMMAND);
        print_status(Status::Created, "Added PreCompact hook");
        changed = true;
    }

    // --- PreToolUse hook: consume ---
    let has_consume = hook_has_command(&settings, "PreToolUse", CONSUME_COMMAND);
    if has_consume {
        print_status(Status::Exists, "PreToolUse hook already up to date");
    } else {
        add_hook_entry(&mut settings, "PreToolUse", CONSUME_COMMAND);
        print_status(
            Status::Created,
            "Added PreToolUse hook (ephemeral consumption)",
        );
        changed = true;
    }

    // Write once if anything changed
    if changed {
        match serde_json::to_string_pretty(&settings) {
            Ok(json) => match fs::write(settings_path, format!("{json}\n")) {
                Ok(()) => {}
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
}

pub fn run() -> Result<(), String> {
    run_with_base(&paths::claude_dir()?)
}

pub fn run_with_base(base: &Path) -> Result<(), String> {
    // Pre-flight check
    if !base.exists() {
        return Err(
            "~/.claude directory not found. Is Claude Code installed?\n  \
             Install Claude Code first, then run this again."
                .to_string(),
        );
    }

    eprintln!("\n{BOLD}recall-echo{RESET} — initializing memory system\n");

    // Create directories
    let rules_dir = base.join("rules");
    let memory_dir = base.join("memory");
    let conversations_dir = base.join("conversations");
    ensure_dir(&rules_dir);
    ensure_dir(&memory_dir);
    ensure_dir(&conversations_dir);

    // Write protocol rules file
    write_protocol(&rules_dir.join("recall-echo.md"));

    // Write MEMORY.md (never overwrite)
    write_if_not_exists(&memory_dir.join("MEMORY.md"), MEMORY_TEMPLATE, "MEMORY.md");

    // Write EPHEMERAL.md (never overwrite)
    write_if_not_exists(&base.join("EPHEMERAL.md"), "", "EPHEMERAL.md");

    // Write ARCHIVE.md (never overwrite)
    write_if_not_exists(&base.join("ARCHIVE.md"), ARCHIVE_TEMPLATE, "ARCHIVE.md");

    // Merge hooks (SessionEnd + PreCompact + PreToolUse)
    merge_hooks(&base.join("settings.json"));

    // Summary
    eprintln!(
        "\n{BOLD}Setup complete.{RESET} Your memory system is ready.\n\n\
         \x20 Layer 1 (MEMORY.md)     — Curated facts, always in context\n\
         \x20 Layer 2 (EPHEMERAL.md)  — Rolling window of recent sessions\n\
         \x20 Layer 3 (Archive)       — Full conversations in ~/.claude/conversations/\n\n\
         \x20 Hooks installed:\n\
         \x20   PreToolUse  → recall-echo consume  (reads EPHEMERAL.md into context)\n\
         \x20   PreCompact  → recall-echo checkpoint (saves before context compression)\n\
         \x20   SessionEnd  → recall-echo archive-session (archives conversation)\n\n\
         \x20 Start a new Claude Code session and your conversations will be remembered.\n"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_directories_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(&base).unwrap();

        run_with_base(&base).unwrap();

        assert!(base.join("rules/recall-echo.md").exists());
        assert!(base.join("memory/MEMORY.md").exists());
        assert!(base.join("EPHEMERAL.md").exists());
        assert!(base.join("ARCHIVE.md").exists());
        assert!(base.join("conversations").exists());
        assert!(base.join("settings.json").exists());
    }

    #[test]
    fn init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(&base).unwrap();

        run_with_base(&base).unwrap();
        fs::write(base.join("memory/MEMORY.md"), "custom content").unwrap();

        run_with_base(&base).unwrap();
        let content = fs::read_to_string(base.join("memory/MEMORY.md")).unwrap();
        assert_eq!(content, "custom content");
    }

    #[test]
    fn init_merges_hooks_into_existing_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(&base).unwrap();

        let existing = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{"type": "command", "command": "some-other-tool"}]
                }]
            }
        });
        fs::write(
            base.join("settings.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        run_with_base(&base).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(base.join("settings.json")).unwrap()).unwrap();

        assert!(hook_has_command(&settings, "PreToolUse", "some-other-tool"));
        assert!(hook_has_command(
            &settings,
            "SessionEnd",
            ARCHIVE_SESSION_COMMAND
        ));
        assert!(hook_has_command(
            &settings,
            "PreCompact",
            CHECKPOINT_COMMAND
        ));
        assert!(hook_has_command(&settings, "PreToolUse", CONSUME_COMMAND));
    }

    #[test]
    fn init_does_not_duplicate_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(&base).unwrap();

        run_with_base(&base).unwrap();
        run_with_base(&base).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(base.join("settings.json")).unwrap()).unwrap();

        let session_end = settings["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(session_end.len(), 1);
    }

    #[test]
    fn init_migrates_legacy_promote_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(&base).unwrap();

        let legacy = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "recall-echo promote"}]
                }]
            }
        });
        fs::write(
            base.join("settings.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        run_with_base(&base).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(base.join("settings.json")).unwrap()).unwrap();

        assert!(hook_has_command(
            &settings,
            "SessionEnd",
            ARCHIVE_SESSION_COMMAND
        ));
        assert!(!hook_has_command(
            &settings,
            "SessionEnd",
            LEGACY_PROMOTE_COMMAND
        ));
    }

    #[test]
    fn fails_if_base_dir_missing() {
        let result = run_with_base(std::path::Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }
}
