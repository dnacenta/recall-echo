// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Initialize the recall-echo memory system.
//!
//! Creates the directory structure and template files needed for four-layer
//! memory (graph, curated, short-term, long-term), picks an extraction
//! provider, installs Claude Code's hooks and the rules file that imports
//! MEMORY.md, registers the MCP server with every agent CLI on the machine,
//! and downloads the embedding model.
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
//! And one yes/no, only when Claude Code's built-in auto-memory is on: turn it
//! off, so MEMORY.md here is the one curated store? Enter means yes, end of
//! input means no. It is skipped for a pulse root, a user who runs pulses and
//! a build-directory binary, and never asked when auto-memory is already off.
//!
//! Nothing prompts unless stdin and stderr are both terminals
//! ([`paths::ConfigRoots::interactive`]); a scripted or piped install takes
//! the same defaults without blocking, and leaves auto-memory as it is with a
//! notice.
//!
//! # What it does without asking
//!
//! Hooks, MCP registration and the model download are consequences of what is
//! installed, not preferences, so they happen. Each is idempotent, each reports
//! itself, and none of them can fail the command:
//!
//! - hooks are matched by command name, so re-running never duplicates one;
//! - the rules file carries a marker line, so only a file `init` wrote is ever
//!   rewritten (see `rules_file`);
//! - MCP servers live in a map keyed by name in every client, so re-registering
//!   the same name is a no-op (see [`crate::agent_cli`]);
//! - the model is a content-addressed cache, so a second warm is a no-op.
//!
//! # The build-directory guard
//!
//! A binary under `target/debug` or `target/release` is a test harness or a
//! working copy, not something a user's hooks and MCP configs should be pinned
//! to for the life of the install. Everything that writes *outside* the pulse
//! root — hooks, the rules file, the auto-memory switch, MCP registration — is
//! skipped there, which is also what keeps `cargo test` from repointing the
//! developer's live tooling at a test binary or downloading 127 MB per test.

use std::fs;
use std::io::{self, BufRead, Write as _};
use std::path::Path;

use crate::agent_cli::{self, AgentCli, McpReport, McpStatus};
use crate::claude_settings;
use crate::config::{self, Config, LlmSection, Provider};
use crate::error::RecallError;
use crate::paths;
use crate::rules_file;
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
/// the pulse layout, which hooks and reads will now prefer — a populated
/// legacy directory would otherwise be left behind silently: invisible to
/// search and the graph, with numbering restarting at 001 in the new place.
fn notice_legacy_conversations(pulse_root: &Path, new_dir: &Path) {
    let legacy = pulse_root.join("conversations");
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
            pulse_root.display()
        );
    }
}

/// Create `path` (and any missing parents) owner-only.
///
/// Explicit, not the umask's: under the 0002 umask Ubuntu and Fedora give
/// login shells, a default directory is group-writable — and the rules file
/// refuses to import from a store anyone but its owner could write.
fn ensure_dir(path: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    if !path.exists() {
        if let Err(e) = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
        {
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
        // Owner-only from birth, whatever the umask (see `ensure_dir`).
        use std::os::unix::fs::OpenOptionsExt;
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(content.as_bytes()));
        match written {
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
fn select_provider(
    reader: &mut dyn BufRead,
    detected: &[AgentCli],
    interactive: bool,
) -> Option<Provider> {
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
            prompt_any_provider(reader, interactive)
        }
        several => prompt_installed_cli(reader, several, interactive),
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
fn prompt_installed_cli(
    reader: &mut dyn BufRead,
    detected: &[AgentCli],
    interactive: bool,
) -> Option<Provider> {
    let default = default_cli(detected);
    if !interactive {
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
        return prompt_any_provider(reader, interactive);
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
fn prompt_any_provider(reader: &mut dyn BufRead, interactive: bool) -> Option<Provider> {
    if !interactive {
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
    interactive: bool,
) -> Option<Provider> {
    if config::exists(memory_dir) {
        print_status(
            Status::Exists,
            ".recall-echo.toml already exists — preserved",
        );
        return Some(config::load(memory_dir).llm.provider);
    }

    let Some(provider) = select_provider(reader, detected, interactive) else {
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

// ── Claude Code: hooks, rules file, auto-memory ──────────────────────────

/// Configure Claude Code for this store: the three hooks, the rules file
/// that imports MEMORY.md, and the auto-memory question.
///
/// All three go through [`claude_settings::claude_code_target`], so they skip
/// for the same reasons with the same words. settings.json is read once, both
/// the hooks and the auto-memory answer are applied to that one value, and it
/// is written once at the end ([`claude_settings::SettingsEdit`]). The file is
/// `<roots.claude_dir()>/settings.json` — `~/.claude/settings.json` in
/// production, a sandbox under test — regardless of where `pulse_root` is,
/// and the binary the hooks invoke comes from `roots` too: nothing here is
/// decided by the path this process happens to be running from.
fn configure_claude_code(
    roots: &paths::ConfigRoots,
    pulse_root: &Path,
    reader: &mut dyn BufRead,
    interactive: bool,
    env_disable: Option<&str>,
) {
    let target = match claude_settings::claude_code_target(roots, pulse_root) {
        Ok(target) => target,
        // Nothing to configure and nothing worth saying: a machine without
        // Claude Code is using another agent CLI on purpose.
        Err(claude_settings::Skip::NoClaudeCode) => return,
        Err(skip) => {
            print_status(
                Status::Exists,
                &format!("Skipped Claude Code hooks, rules file and auto-memory — {skip}"),
            );
            warn_stale_import(roots.claude_dir(), pulse_root);
            return;
        }
    };

    let settings_path = target.settings_path();
    let mut edit = match claude_settings::open_settings(&settings_path) {
        claude_settings::OpenSettings::Edit(edit) => Some(edit),
        claude_settings::OpenSettings::Symlink => {
            advise_symlinked_settings(&settings_path, roots, pulse_root);
            None
        }
        claude_settings::OpenSettings::Refused(why) => {
            print_status(
                Status::Error,
                &format!("{why} — hooks and auto-memory left as is"),
            );
            None
        }
    };

    let hooks_changed = edit
        .as_mut()
        .is_some_and(|edit| stage_hooks(&mut edit.value, roots, pulse_root));
    let mut auto_memory_off = false;
    match target.memory_steps() {
        Err(skip) => {
            print_status(
                Status::Exists,
                &format!("Skipped rules file and auto-memory — {skip}"),
            );
            warn_stale_import(Some(target.claude_dir()), pulse_root);
        }
        Ok(()) => {
            configure_rules(&target, pulse_root);
            if let Some(edit) = edit.as_mut() {
                auto_memory_off = stage_auto_memory(
                    &mut edit.value,
                    &settings_path,
                    reader,
                    interactive,
                    env_disable,
                ) == AutoMemoryStep::TurnedOff;
            }
        }
    }

    if let Some(edit) = edit {
        commit_settings(edit, hooks_changed, auto_memory_off, roots, pulse_root);
    }
}

/// Apply the three hooks to the settings value. Returns whether it changed.
fn stage_hooks(
    settings: &mut serde_json::Value,
    roots: &paths::ConfigRoots,
    pulse_root: &Path,
) -> bool {
    match claude_settings::apply_hooks(settings, pulse_root, roots.recall_bin()) {
        Ok(change) => {
            for note in &change.notes {
                print_status(Status::Exists, note);
            }
            if !change.changed {
                print_status(Status::Exists, "Hooks already configured in settings.json");
            }
            change.changed
        }
        Err(why) => {
            print_status(Status::Error, &why);
            false
        }
    }
}

/// Write the one edit — when there is anything to write — and report it.
fn commit_settings(
    edit: claude_settings::SettingsEdit,
    hooks_changed: bool,
    auto_memory_off: bool,
    roots: &paths::ConfigRoots,
    pulse_root: &Path,
) {
    if !hooks_changed && !auto_memory_off {
        return;
    }
    let reapply = |settings: &mut serde_json::Value| {
        if hooks_changed {
            claude_settings::apply_hooks(settings, pulse_root, roots.recall_bin())?;
        }
        if auto_memory_off {
            claude_settings::disable_auto_memory(settings);
        }
        Ok(())
    };
    match edit.commit(reapply) {
        Ok(committed) => {
            if committed == claude_settings::Committed::WrittenOverChange {
                print_status(
                    Status::Exists,
                    "settings.json changed while init was running — kept its new content \
                     and applied this run's changes to it",
                );
            }
            if hooks_changed {
                print_status(
                    Status::Created,
                    "Configured SessionStart + SessionEnd + PreCompact hooks in settings.json",
                );
            }
            if auto_memory_off {
                print_status(
                    Status::Created,
                    "Turned off Claude Code's auto-memory — MEMORY.md here is the one curated \
                     store",
                );
            }
        }
        Err(why) => print_status(Status::Error, &why),
    }
}

/// settings.json is a symlink — a dotfiles setup — so it is never replaced.
/// Say what it needs instead.
fn advise_symlinked_settings(settings_path: &Path, roots: &paths::ConfigRoots, pulse_root: &Path) {
    let mut wanted = serde_json::json!({});
    let hooks = claude_settings::apply_hooks(&mut wanted, pulse_root, roots.recall_bin())
        .ok()
        .and_then(|_| serde_json::to_string_pretty(&wanted["hooks"]).ok());
    print_status(
        Status::Exists,
        &format!(
            "{} is a symlink — not replaced. Add the recall-echo hooks to the file it points \
             at yourself, and \"autoMemoryEnabled\": false to keep one curated store:",
            settings_path.display()
        ),
    );
    if let Some(hooks) = hooks {
        for line in format!("\"hooks\": {hooks}").lines() {
            eprintln!("      {line}");
        }
    }
}

/// After a skipped rules step: say so if a managed rules file is loading some
/// other store's MEMORY.md into every session.
fn warn_stale_import(claude_dir: Option<&Path>, pulse_root: &Path) {
    if let Some(warning) = claude_dir.and_then(|dir| rules_file::stale_import(dir, pulse_root)) {
        print_status(Status::Exists, &warning);
    }
}

/// Install the rules file that imports MEMORY.md into every Claude Code
/// session (#75) — the only channel that carries Layer 1 whole.
fn configure_rules(target: &claude_settings::ClaudeTarget, pulse_root: &Path) {
    let report = rules_file::install(target.claude_dir(), pulse_root);
    for warning in &report.warnings {
        print_status(Status::Exists, warning);
    }
    match report.outcome {
        rules_file::Outcome::Written(path) => print_status(
            Status::Created,
            &format!(
                "Wrote {} — MEMORY.md now loads in every Claude Code session",
                path.display()
            ),
        ),
        rules_file::Outcome::Unchanged(path) => print_status(
            Status::Exists,
            &format!("{} already up to date", path.display()),
        ),
        rules_file::Outcome::UnmanagedImports(path) => print_status(
            Status::Exists,
            &format!(
                "{} is not managed by recall-echo but already imports MEMORY.md — left as is",
                path.display()
            ),
        ),
        rules_file::Outcome::Unmanaged { path, import_line } => {
            print_status(
                Status::Exists,
                &format!(
                    "{} is not managed by recall-echo — left as is. For MEMORY.md to \
                     load in every session it needs this line:",
                    path.display()
                ),
            );
            eprintln!("      {import_line}");
        }
        rules_file::Outcome::Refused(why) | rules_file::Outcome::Failed(why) => {
            print_status(Status::Error, &format!("Rules file not written: {why}"));
        }
    }
}

/// What the auto-memory step did.
#[derive(Debug, PartialEq, Eq)]
enum AutoMemoryStep {
    /// Already off — nothing said, nothing asked.
    AlreadyOff,
    /// Not interactive: the notice line was printed instead of a question.
    Noticed,
    /// Asked; the user kept it on.
    Declined,
    /// Asked; turned off in the settings value.
    TurnedOff,
}

/// Offer to turn off Claude Code's built-in auto-memory (#75), in the
/// settings value.
///
/// With it on, the user has a second curated store that recall-echo neither
/// reads nor feeds — and it is the one the model is told to use. The choice
/// is the user's: asked once, only when auto-memory is on and the session is
/// `interactive`; a non-interactive one gets a notice line instead.
fn stage_auto_memory(
    settings: &mut serde_json::Value,
    settings_path: &Path,
    reader: &mut dyn BufRead,
    interactive: bool,
    env_disable: Option<&str>,
) -> AutoMemoryStep {
    if !claude_settings::auto_memory_on(settings, env_disable) {
        return AutoMemoryStep::AlreadyOff;
    }
    let what = "Claude Code's built-in auto-memory is on: a second curated store that \
                recall-echo does not read";
    let not_moved = "Notes already under ~/.claude/projects/*/memory/ are not moved.";
    if !interactive {
        print_status(
            Status::Exists,
            &format!(
                "{what}. Set \"autoMemoryEnabled\": false in {} to keep one store. {not_moved}",
                settings_path.display()
            ),
        );
        return AutoMemoryStep::Noticed;
    }

    eprintln!("\n{BOLD}{what}.{RESET}");
    eprintln!("  {DIM}{not_moved}{RESET}");
    if !ask_yes_default(reader, "Turn it off?") {
        print_status(Status::Exists, "Left Claude Code's auto-memory on");
        return AutoMemoryStep::Declined;
    }
    claude_settings::disable_auto_memory(settings);
    AutoMemoryStep::TurnedOff
}

/// Ask a question that defaults to yes. Enter, `y` or `yes` accept; anything
/// else, or end of input, declines — so a closed stdin never answers yes.
fn ask_yes_default(reader: &mut dyn BufRead, question: &str) -> bool {
    eprint!("  {question} [Y/n] ");
    let mut answer = String::new();
    match reader.read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "" | "y" | "yes"
        ),
    }
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
    pulse_root: &Path,
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

    let root = fs::canonicalize(pulse_root).unwrap_or_else(|_| pulse_root.to_path_buf());
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

// ── Entry point ──────────────────────────────────────────────────────────

/// Initialize memory structure at the given pulse root.
///
/// Creates:
/// ```text
/// {pulse_root}/memory/
/// ├── MEMORY.md
/// ├── EPHEMERAL.md
/// ├── ARCHIVE.md
/// ├── .recall-echo.toml
/// ├── graph/
/// └── conversations/
/// ```
pub fn run(pulse_root: &Path) -> Result<(), RecallError> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    run_with_reader(pulse_root, &mut reader)
}

/// Init with an injectable reader, against the real user configuration.
pub fn run_with_reader(pulse_root: &Path, reader: &mut dyn BufRead) -> Result<(), RecallError> {
    run_with(pulse_root, reader, &paths::ConfigRoots::from_env())
}

/// Init with both the reader and the global destinations injected.
///
/// `roots` decides where the hook file, the persisted pulse root and any
/// agent-CLI config land. Pass [`paths::ConfigRoots::sandboxed`] and nothing
/// outside that directory can be written, whatever this binary's path is (#59).
pub fn run_with(
    pulse_root: &Path,
    reader: &mut dyn BufRead,
    roots: &paths::ConfigRoots,
) -> Result<(), RecallError> {
    if !pulse_root.exists() {
        return Err(RecallError::NotInitialized(format!(
            "Directory not found: {}\n  Create the directory first, or run from a valid path.",
            pulse_root.display()
        )));
    }

    eprintln!("\n{BOLD}recall-echo{RESET} — initializing memory system\n");

    let memory_dir = pulse_root.join("memory");
    let conversations_dir = memory_dir.join("conversations");
    ensure_dir(&memory_dir);
    ensure_dir(&conversations_dir);
    notice_legacy_conversations(pulse_root, &conversations_dir);

    // Pin this root for flagless hook invocations (#46): capture must land in
    // the store the MCP server serves, not wherever the session's cwd is.
    match roots.persist_pulse_root(pulse_root) {
        Ok(paths::PersistOutcome::Written(file)) => print_status(
            Status::Created,
            &format!("Pulse root persisted to {}", file.display()),
        ),
        Ok(paths::PersistOutcome::Skipped(why)) => print_status(
            Status::Exists,
            &format!("Skipped persisting the pulse root — {why}"),
        ),
        Err(e) => print_status(Status::Error, &format!("Could not persist pulse root: {e}")),
    }

    // Write MEMORY.md (never overwrite)
    write_if_not_exists(&memory_dir.join("MEMORY.md"), MEMORY_TEMPLATE, "MEMORY.md");
    if let Ok(content) = fs::read_to_string(memory_dir.join("MEMORY.md")) {
        print_status(
            Status::Exists,
            &format!(
                "MEMORY.md is {} lines, {} — loaded into every Claude Code session",
                content.lines().count(),
                crate::distill::size_summary(&content)
            ),
        );
        if let Some(warning) = rules_file::hidden_import_warning(&content) {
            print_status(Status::Exists, &warning);
        }
    }

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
    let provider = configure_llm(reader, &memory_dir, &detected, roots.interactive());

    // Hooks are Claude Code's capture mechanism, not a consequence of the
    // extraction provider: a user who extracts with grok still wants their
    // Claude Code sessions archived. `configure_claude_code` no-ops when Claude
    // Code is not installed.
    configure_claude_code(
        roots,
        pulse_root,
        reader,
        roots.interactive(),
        roots.auto_memory_env(),
    );

    let mcp = match &runtime {
        Some(runtime) => register_mcp_clients(runtime, &detected, pulse_root, roots),
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

    /// A pulse root and a sandbox for everything `init` writes outside it.
    ///
    /// Every test here runs the real writers; none of them may reach the
    /// developer's `~/.claude`, `~/.claude.json` or persisted pulse root
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
            // Independent of the umask: the rules file refuses a root anyone
            // else can write (#75 audit H1).
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&entity, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let roots = paths::ConfigRoots::sandboxed(dir.path()).unwrap();
            Self { dir, roots }
        }

        fn pulse_root(&self) -> PathBuf {
            self.dir.path().join("entity")
        }

        fn init(&self, input: &str) -> Result<(), RecallError> {
            let mut reader = Cursor::new(input.as_bytes());
            run_with(&self.pulse_root(), &mut reader, &self.roots)
        }
    }

    #[test]
    fn init_creates_directories_and_files() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap(); // skip provider prompt

        let root = sandbox.pulse_root();
        assert!(root.join("memory/MEMORY.md").exists());
        assert!(root.join("memory/EPHEMERAL.md").exists());
        assert!(root.join("memory/ARCHIVE.md").exists());
        assert!(root.join("memory/conversations").exists());
    }

    /// `init` makes its store owner-only whatever the umask: memory/ and
    /// conversations/ 0700, MEMORY.md, EPHEMERAL.md and ARCHIVE.md 0600 —
    /// so the rules step never refuses the store `init` just created (H1).
    #[test]
    fn init_creates_the_store_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();
        let memory = sandbox.pulse_root().join("memory");
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(memory.clone()), 0o700);
        assert_eq!(mode(memory.join("conversations")), 0o700);
        for file in ["MEMORY.md", "EPHEMERAL.md", "ARCHIVE.md"] {
            assert_eq!(mode(memory.join(file)), 0o600, "{file}");
        }
        assert!(
            sandbox_rules(&sandbox).exists(),
            "the rules step accepted it"
        );
    }

    /// Everything `init` writes outside the pulse root lands in the sandbox:
    /// the persisted pointer exists there, and it names the pulse root.
    #[test]
    fn init_persists_the_pulse_root_inside_the_sandbox() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();

        let persisted = sandbox.roots.pulse_root_file().expect("a destination");
        let pinned = fs::read_to_string(persisted).expect("persisted inside the sandbox");
        assert_eq!(
            pinned.trim(),
            fs::canonicalize(sandbox.pulse_root())
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn init_is_idempotent() {
        let sandbox = Sandbox::new();
        let root = sandbox.pulse_root();
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
            false,
        );
        assert_eq!(chosen, Some(Provider::Grok));

        // Empty reader: a prompt here would take the default and lose grok.
        let again = configure_llm(
            &mut Cursor::new(b"" as &[u8]),
            &memory_dir,
            &[AgentCli::ClaudeCode, AgentCli::Codex],
            false,
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
            select_provider(&mut reader, &[AgentCli::Codex], false),
            Some(Provider::Codex)
        );
        assert_eq!(reader.position(), 0, "nothing should have been read");
    }

    /// Non-interactive (the tests, and any scripted install): pick the default
    /// rather than block on a prompt nobody can answer.
    #[test]
    fn several_installed_clis_default_without_blocking() {
        let mut reader = Cursor::new(b"" as &[u8]);
        let chosen = select_provider(&mut reader, &[AgentCli::Grok, AgentCli::Codex], false);
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
        assert_eq!(
            select_provider(&mut reader, &[], false),
            Some(Provider::Anthropic)
        );
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

    /// No Claude Code, no hooks — and nothing written anywhere looking for it.
    #[test]
    fn hooks_are_skipped_when_claude_code_is_absent() {
        let sandbox = Sandbox::new();
        let roots = sandbox.roots.clone().without_claude_code();
        assert_eq!(
            claude_settings::claude_code_target(&roots, &sandbox.pulse_root()).err(),
            Some(claude_settings::Skip::NoClaudeCode)
        );
        configure_claude_code(
            &roots,
            &sandbox.pulse_root(),
            &mut Cursor::new(Vec::new()),
            true,
            None,
        );
        assert!(!sandbox.dir.path().join(".claude/settings.json").exists());
    }

    fn sandbox_rules(sandbox: &Sandbox) -> PathBuf {
        sandbox
            .roots
            .claude_dir()
            .unwrap()
            .join("rules")
            .join(rules_file::FILE_NAME)
    }

    /// `init` writes the managed rules file into the sandbox, importing this
    /// root's MEMORY.md and nothing else (#75).
    #[test]
    fn init_writes_rules_file_in_sandbox() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();

        let rules = fs::read_to_string(sandbox_rules(&sandbox)).expect("rules file in the sandbox");
        assert_eq!(rules.lines().next(), Some(rules_file::MARKER));
        let root = fs::canonicalize(sandbox.pulse_root()).unwrap();
        let line = rules_file::import_line(&root).unwrap();
        let imports: Vec<&str> = rules.lines().filter(|l| l.starts_with('@')).collect();
        assert_eq!(imports, [line.as_str()]);
    }

    #[test]
    fn a_second_init_leaves_the_rules_file_as_it_was() {
        let sandbox = Sandbox::new();
        sandbox.init("skip\n").unwrap();
        let first = fs::read(sandbox_rules(&sandbox)).unwrap();

        sandbox.init("skip\n").unwrap();
        assert_eq!(fs::read(sandbox_rules(&sandbox)).unwrap(), first);
    }

    /// A pulse gets MEMORY.md from pulse-null's own prompt; a user-level
    /// import would load it twice in every pulse session.
    #[test]
    fn init_skips_rules_for_pulse_root() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.pulse_root().join("pulse-null.toml"), "[pulse]\n").unwrap();
        sandbox.init("skip\n").unwrap();
        assert!(!sandbox_rules(&sandbox).exists());
    }

    /// The user runs pulses: no user-level rules file and no auto-memory
    /// change, whatever root `init` is given (#75 audit B6).
    #[test]
    fn init_skips_the_memory_steps_when_the_home_holds_pulses() {
        let sandbox = Sandbox::new();
        let pulse = sandbox.dir.path().join("pulse-null").join("echo");
        fs::create_dir_all(&pulse).unwrap();
        fs::write(pulse.join("pulse-null.toml"), "[pulse]\n").unwrap();
        let settings_path = seeded_settings(&sandbox);

        let mut reader = Cursor::new(b"y\n".to_vec());
        configure_claude_code(
            &sandbox.roots,
            &sandbox.pulse_root(),
            &mut reader,
            true,
            None,
        );
        assert!(!sandbox_rules(&sandbox).exists());
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        assert!(settings.get("autoMemoryEnabled").is_none(), "{settings}");
        assert_eq!(reader.position(), 0, "nothing asked");
    }

    #[test]
    fn init_leaves_an_unmanaged_rules_file_untouched() {
        let sandbox = Sandbox::new();
        let rules = sandbox_rules(&sandbox);
        fs::create_dir_all(rules.parent().unwrap()).unwrap();
        fs::write(&rules, "# hand-written\n").unwrap();

        sandbox.init("skip\n").unwrap();
        assert_eq!(fs::read_to_string(&rules).unwrap(), "# hand-written\n");
    }

    #[test]
    fn rules_are_skipped_when_claude_code_is_absent() {
        let sandbox = Sandbox::new();
        let roots = sandbox.roots.clone().without_claude_code();
        configure_claude_code(
            &roots,
            &sandbox.pulse_root(),
            &mut Cursor::new(Vec::new()),
            true,
            None,
        );
        assert!(!sandbox.dir.path().join(".claude/rules").exists());
    }

    fn settings_file(sandbox: &Sandbox) -> PathBuf {
        sandbox.roots.claude_dir().unwrap().join("settings.json")
    }

    /// Write `json` as the sandbox user's settings.json, then let one
    /// non-interactive run put the hooks in place — so a later run changes
    /// only what the auto-memory answer changes.
    fn settled_settings(sandbox: &Sandbox, json: &str) -> PathBuf {
        let path = settings_file(sandbox);
        fs::write(&path, json).unwrap();
        configure_claude_code(
            &sandbox.roots,
            &sandbox.pulse_root(),
            &mut Cursor::new(Vec::new()),
            false,
            None,
        );
        path
    }

    /// The user's settings file in the sandbox, seeded with keys that must
    /// survive an edit, hooks already in place.
    fn seeded_settings(sandbox: &Sandbox) -> PathBuf {
        settled_settings(
            sandbox,
            r#"{"model": "opus", "permissions": {"allow": ["Bash(ls)"]}}"#,
        )
    }

    fn ask_auto_memory_with(
        sandbox: &Sandbox,
        answer: &str,
        interactive: bool,
        env_disable: Option<&str>,
    ) -> Cursor<Vec<u8>> {
        let mut reader = Cursor::new(answer.as_bytes().to_vec());
        configure_claude_code(
            &sandbox.roots,
            &sandbox.pulse_root(),
            &mut reader,
            interactive,
            env_disable,
        );
        reader
    }

    fn ask_auto_memory(sandbox: &Sandbox, answer: &str, interactive: bool) -> Cursor<Vec<u8>> {
        ask_auto_memory_with(sandbox, answer, interactive, None)
    }

    #[test]
    fn auto_memory_yes_writes_false_preserving_keys() {
        let sandbox = Sandbox::new();
        let path = seeded_settings(&sandbox);
        ask_auto_memory(&sandbox, "y\n", true);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(settings["autoMemoryEnabled"], serde_json::json!(false));
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls)");
    }

    #[test]
    fn auto_memory_enter_defaults_to_yes() {
        let sandbox = Sandbox::new();
        let path = seeded_settings(&sandbox);
        ask_auto_memory(&sandbox, "\n", true);
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("\"autoMemoryEnabled\": false"));
    }

    #[test]
    fn auto_memory_no_or_end_of_input_leaves_settings_byte_identical() {
        for answer in ["n\n", "no\n", ""] {
            let sandbox = Sandbox::new();
            let path = seeded_settings(&sandbox);
            let before = fs::read(&path).unwrap();
            ask_auto_memory(&sandbox, answer, true);
            assert_eq!(fs::read(&path).unwrap(), before, "answer {answer:?}");
        }
    }

    #[test]
    fn auto_memory_non_interactive_never_writes_or_reads() {
        let sandbox = Sandbox::new();
        let path = seeded_settings(&sandbox);
        let before = fs::read(&path).unwrap();
        let reader = ask_auto_memory(&sandbox, "y\n", false);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(reader.position(), 0, "nothing asked");
    }

    /// The non-interactive branch is the notice, not a silent no-op.
    #[test]
    fn a_non_interactive_run_takes_the_notice_branch() {
        let path = Path::new("/sandbox/.claude/settings.json");
        let step = |settings: serde_json::Value, interactive: bool, answer: &str| {
            let mut settings = settings;
            let mut reader = Cursor::new(answer.as_bytes().to_vec());
            stage_auto_memory(&mut settings, path, &mut reader, interactive, None)
        };
        assert_eq!(
            step(serde_json::json!({}), false, "y\n"),
            AutoMemoryStep::Noticed
        );
        assert_eq!(
            step(serde_json::json!({}), true, "n\n"),
            AutoMemoryStep::Declined
        );
        assert_eq!(
            step(serde_json::json!({}), true, "y\n"),
            AutoMemoryStep::TurnedOff
        );
        assert_eq!(
            step(serde_json::json!({"autoMemoryEnabled": false}), false, ""),
            AutoMemoryStep::AlreadyOff
        );
    }

    #[test]
    fn auto_memory_already_off_is_not_asked() {
        let sandbox = Sandbox::new();
        let path = settled_settings(&sandbox, r#"{"autoMemoryEnabled": false}"#);
        let before = fs::read(&path).unwrap();
        let reader = ask_auto_memory(&sandbox, "y\n", true);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(reader.position(), 0, "nothing asked");
    }

    #[test]
    fn auto_memory_off_by_environment_is_not_asked() {
        let sandbox = Sandbox::new();
        let path = seeded_settings(&sandbox);
        let before = fs::read(&path).unwrap();
        let reader = ask_auto_memory_with(&sandbox, "y\n", true, Some("1"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(reader.position(), 0, "nothing asked");
    }

    /// One read, one write: a run that changes both the hooks and the
    /// auto-memory switch writes settings.json once — `.bak` is the file as
    /// the user had it before the run, not an intermediate state.
    #[test]
    fn one_run_writes_settings_once_and_backs_up_the_original() {
        let sandbox = Sandbox::new();
        let path = settings_file(&sandbox);
        let original = r#"{"model": "opus"}"#;
        fs::write(&path, original).unwrap();

        ask_auto_memory(&sandbox, "y\n", true);
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(settings["hooks"]["SessionStart"].is_array(), "{settings}");
        assert_eq!(settings["autoMemoryEnabled"], serde_json::json!(false));
        assert_eq!(settings["model"], "opus");
        assert_eq!(
            fs::read_to_string(path.with_extension("json.bak")).unwrap(),
            original
        );
    }

    /// Rewrites settings.json the moment the answer is read, as an editor
    /// saving during the prompt would.
    struct EditsDuringPrompt {
        answer: Cursor<Vec<u8>>,
        path: PathBuf,
        edit: Option<&'static str>,
    }

    impl std::io::Read for EditsDuringPrompt {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.answer.read(buf)
        }
    }

    impl BufRead for EditsDuringPrompt {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if let Some(edit) = self.edit.take() {
                fs::write(&self.path, edit).unwrap();
            }
            self.answer.fill_buf()
        }
        fn consume(&mut self, amount: usize) {
            self.answer.consume(amount);
        }
    }

    fn answer_while_editing(sandbox: &Sandbox, edit: &'static str) -> PathBuf {
        let path = settings_file(sandbox);
        fs::write(&path, r#"{"model": "opus"}"#).unwrap();
        let mut reader = EditsDuringPrompt {
            answer: Cursor::new(b"y\n".to_vec()),
            path: path.clone(),
            edit: Some(edit),
        };
        configure_claude_code(
            &sandbox.roots,
            &sandbox.pulse_root(),
            &mut reader,
            true,
            None,
        );
        path
    }

    #[test]
    fn a_change_made_during_the_prompt_is_kept_and_this_runs_changes_reapplied() {
        let sandbox = Sandbox::new();
        let path = answer_while_editing(&sandbox, r#"{"theme": "dark"}"#);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark", "{settings}");
        assert!(
            settings.get("model").is_none(),
            "the stale read must not win: {settings}"
        );
        assert!(settings["hooks"]["SessionStart"].is_array(), "{settings}");
        assert_eq!(settings["autoMemoryEnabled"], serde_json::json!(false));
        assert_eq!(
            fs::read_to_string(path.with_extension("json.bak")).unwrap(),
            r#"{"theme": "dark"}"#
        );
    }

    #[test]
    fn a_file_broken_during_the_prompt_is_never_overwritten() {
        let sandbox = Sandbox::new();
        let path = answer_while_editing(&sandbox, "{broken");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn a_symlinked_settings_file_is_never_replaced() {
        let sandbox = Sandbox::new();
        let dotfiles = sandbox.dir.path().join("dotfiles-settings.json");
        fs::write(&dotfiles, r#"{"model": "opus"}"#).unwrap();
        let path = settings_file(&sandbox);
        std::os::unix::fs::symlink(&dotfiles, &path).unwrap();

        let reader = ask_auto_memory(&sandbox, "y\n", true);
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&dotfiles).unwrap(),
            r#"{"model": "opus"}"#
        );
        assert_eq!(
            reader.position(),
            0,
            "nothing asked: nothing could be written"
        );
    }

    /// AC10, build-directory arm: from a `target/debug` binary, even an
    /// interactive run answered yes writes neither the rules file nor any
    /// settings.json change (#75 audit B9).
    #[test]
    fn a_build_directory_binary_writes_no_rules_and_no_settings() {
        let sandbox = configured_sandbox();
        let roots = paths::ConfigRoots::sandboxed_in_build_dir(sandbox.dir.path())
            .unwrap()
            .with_interactive(true);
        assert_eq!(
            claude_settings::claude_code_target(&roots, &sandbox.pulse_root()).err(),
            Some(claude_settings::Skip::BuildDir)
        );
        let path = settings_file(&sandbox);
        fs::write(&path, r#"{"model": "opus"}"#).unwrap();

        init_with(&sandbox, &roots, "y\n");
        assert!(!sandbox_rules(&sandbox).exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"model": "opus"}"#);
        assert!(!path.with_extension("json.bak").exists());
    }

    /// A sandbox whose provider is already configured, so the provider menu
    /// never consumes a line: every answer in the reader is the auto-memory
    /// question's.
    fn configured_sandbox() -> Sandbox {
        let sandbox = Sandbox::new();
        let memory = sandbox.pulse_root().join("memory");
        fs::create_dir_all(&memory).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&memory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            memory.join(".recall-echo.toml"),
            "[llm]\nprovider = \"ollama\"\n",
        )
        .unwrap();
        sandbox
    }

    fn init_with(sandbox: &Sandbox, roots: &paths::ConfigRoots, input: &str) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        run_with(&sandbox.pulse_root(), &mut reader, roots).unwrap();
    }

    fn auto_memory_key(sandbox: &Sandbox) -> Option<serde_json::Value> {
        let path = sandbox.roots.claude_dir().unwrap().join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        settings.get("autoMemoryEnabled").cloned()
    }

    /// `run_with` asks only when its roots say the session is interactive —
    /// never because of whatever terminal the test runner happens to have.
    #[test]
    fn init_takes_interactivity_from_its_roots() {
        let quiet = configured_sandbox();
        init_with(&quiet, &quiet.roots, "y\n");
        assert_eq!(
            auto_memory_key(&quiet),
            None,
            "a sandbox never asks by default"
        );

        let asked = configured_sandbox();
        init_with(&asked, &asked.roots.clone().with_interactive(true), "y\n");
        assert_eq!(auto_memory_key(&asked), Some(serde_json::json!(false)));
    }

    #[test]
    fn init_takes_the_auto_memory_switch_from_its_roots() {
        let sandbox = configured_sandbox();
        let roots = sandbox
            .roots
            .clone()
            .with_interactive(true)
            .with_auto_memory_env(Some("1"));
        init_with(&sandbox, &roots, "y\n");
        assert_eq!(auto_memory_key(&sandbox), None, "already off: not asked");
    }

    #[test]
    fn auto_memory_invalid_settings_refused() {
        let sandbox = Sandbox::new();
        let path = sandbox.roots.claude_dir().unwrap().join("settings.json");
        fs::write(&path, "{not json").unwrap();
        let reader = ask_auto_memory(&sandbox, "y\n", true);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        assert_eq!(reader.position(), 0, "nothing asked");
    }

    #[test]
    fn auto_memory_skipped_for_pulse_root() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.pulse_root().join("pulse-null.toml"), "[pulse]\n").unwrap();
        let path = seeded_settings(&sandbox);
        let mut reader = Cursor::new(b"y\n".to_vec());
        configure_claude_code(
            &sandbox.roots,
            &sandbox.pulse_root(),
            &mut reader,
            true,
            None,
        );
        // Hooks still land; the memory steps do not run at all.
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(settings.get("autoMemoryEnabled").is_none(), "{settings}");
        assert!(settings.get("hooks").is_some(), "{settings}");
        assert_eq!(reader.position(), 0, "nothing asked");
        assert!(!sandbox_rules(&sandbox).exists());
    }

    /// The whole init flow, run against a sandbox, writes the hooks into the
    /// sandbox's `settings.json` — through the real `configure_claude_code`
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
        assert!(sandbox.pulse_root().join("memory/MEMORY.md").exists());
        assert!(sandbox
            .roots
            .pulse_root_file()
            .is_some_and(std::path::Path::exists));
        let hooks = fs::read_to_string(sandbox.roots.claude_dir().unwrap().join("settings.json"))
            .expect("hooks landed in the sandbox");
        for command in ["archive-session", "checkpoint", "consume"] {
            assert!(
                hooks.contains(&format!("{} {command}", sandbox.roots.recall_bin())),
                "{command} missing from the sandboxed settings.json: {hooks}"
            );
        }
        assert!(
            sandbox_rules(&sandbox).exists(),
            "the rules file landed in the sandbox"
        );

        before.assert_unchanged();
    }

    /// The files a real user's setup lives in, as digests.
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
                    Digest::take(real_rules_file(), Strictness::Exact),
                    Digest::take(real_claude_json(), Strictness::WhenUntouched),
                    Digest::take(real_pointer_file("pulse-root"), Strictness::Exact),
                    Digest::take(real_pointer_file("entity-root"), Strictness::Exact),
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

    /// `~/.claude/rules/recall-echo.md` — spelled out, not taken from
    /// `rules_file`, for the same reason as the settings path.
    fn real_rules_file() -> PathBuf {
        real_home()
            .join(".claude")
            .join("rules")
            .join("recall-echo.md")
    }

    /// The persisted pointer — `pulse-root`, or the pre-4.6.0 `entity-root`.
    fn real_pointer_file(name: &str) -> PathBuf {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => real_home().join(".config"),
        };
        base.join("recall-echo").join(name)
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
        let content = fs::read_to_string(sandbox.pulse_root().join("memory/ARCHIVE.md")).unwrap();
        assert!(content.contains("# Conversation Archive"));
        assert!(content.contains("| # | Date"));
    }
}

/// The suite's own fence: no test may call an `init` entry point that resolves
/// the real configuration for itself.
///
/// [`run_with_reader`] and [`run`] build [`paths::ConfigRoots::from_env`], and
/// that is the whole of #59 — a test calling either rewrites the developer's
/// hooks and global pulse-root pointer. Tests take `run_with` and a sandbox.
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
