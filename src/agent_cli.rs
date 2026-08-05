// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which agent CLIs this machine has, and how recall-echo wires itself into
//! them.
//!
//! Three questions are asked about the same four CLIs, and before this module
//! each was answered somewhere else, or not at all:
//!
//! - *which one extracts knowledge* — [`crate::config::Provider`], chosen by a
//!   six-item menu that only ever auto-detected Claude Code;
//! - *whose sessions get captured* — [`crate::transcript::detect_installed`],
//!   which answers for the three CLIs that have transcript adapters;
//! - *who can query memory over MCP* — nobody, because registering the server
//!   was a command in the README.
//!
//! [`AgentCli`] is the one place that knows a CLI end to end: its binary, its
//! provider preset, its transcript source if it has one, and the exact
//! `mcp add` incantation it wants. `init` asks here and stops guessing.
//!
//! # Detection
//!
//! A CLI counts as installed when its **binary resolves** — on `PATH`, or at
//! the absolute path an override names. Recorded transcripts are a different
//! question (a machine can hold Codex sessions long after Codex was
//! uninstalled) and the capture layer already answers it; both extraction and
//! MCP registration need a binary to run, so that is what is tested here.
//!
//! The binary name comes from [`CliSpec`], so `CLAUDE_BIN`, `GEMINI_BIN`,
//! `GROK_BIN` and `CODEX_BIN` steer detection exactly as they steer the calls
//! made later.
//!
//! # `mcp add` is four different commands
//!
//! Every one of these CLIs registers stdio MCP servers, and no two agree on how.
//! Each argv below was verified against the installed binary (`claude` 2.x,
//! `gemini` 0.27, `grok`, `codex` 0.146) by running it against a throwaway
//! `HOME` and reading back the config it wrote:
//!
//! ```text
//! claude mcp add recall-echo -s user -- <exe> mcp --entity-root <root>
//! gemini mcp add -s user recall-echo  <exe> mcp --entity-root <root>
//! grok   mcp add recall-echo -s user -- <exe> mcp --entity-root <root>
//! codex  mcp add recall-echo          -- <exe> mcp --entity-root <root>
//! ```
//!
//! Gemini takes the server name *after* its flags and rejects a `--` separator;
//! Codex has no scope flag at all and always writes `~/.codex/config.toml`.
//!
//! # Idempotency
//!
//! Every client stores servers in a map keyed by name, so re-registering the
//! same name with the same argv is a no-op by construction. What differs is
//! what they say about it: Claude refuses with a non-zero exit and "already
//! exists", Gemini says "already configured" and rewrites, Grok and Codex
//! rewrite silently. [`classify`] folds those four dialects into one
//! [`McpStatus`] so the user reads a result rather than four vendors' opinions.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::cli_provider::CliSpec;
use crate::config::{CliPreset, Provider};
use crate::transcript::Source;

/// Name recall-echo registers its MCP server under, in every client.
pub const MCP_SERVER_NAME: &str = "recall-echo";

/// Wall-clock limit for one `mcp add`. It is a local config write; anything
/// slower is a CLI waiting on something it should not be waiting on, and `init`
/// must not hang on it.
const MCP_ADD_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes of a failing client's output carried into the report.
const OUTPUT_EXCERPT: usize = 200;

/// Phrases a client uses to say the server was already there.
const ALREADY_PHRASES: [&str; 3] = ["already exists", "already configured", "already registered"];

// ── The CLIs ─────────────────────────────────────────────────────────────

/// An agent CLI recall-echo can extract with, capture from, or serve over MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentCli {
    ClaudeCode,
    Codex,
    Grok,
    Gemini,
}

impl AgentCli {
    /// Every known CLI, in preference order: the first installed one is the
    /// fallback default when nothing better identifies itself.
    pub const ALL: [AgentCli; 4] = [
        AgentCli::ClaudeCode,
        AgentCli::Codex,
        AgentCli::Grok,
        AgentCli::Gemini,
    ];

    /// The name used in config, prompts and reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AgentCli::ClaudeCode => "claude-code",
            AgentCli::Codex => "codex",
            AgentCli::Grok => "grok",
            AgentCli::Gemini => "gemini",
        }
    }

    /// The extraction provider that spawns this CLI.
    #[must_use]
    pub fn provider(self) -> Provider {
        match self {
            AgentCli::ClaudeCode => Provider::ClaudeCode,
            AgentCli::Codex => Provider::Codex,
            AgentCli::Grok => Provider::Grok,
            AgentCli::Gemini => Provider::Gemini,
        }
    }

    /// The CLI preset holding this CLI's binary name and flags.
    #[must_use]
    fn preset(self) -> CliPreset {
        match self {
            AgentCli::ClaudeCode => CliPreset::ClaudeCode,
            AgentCli::Codex => CliPreset::Codex,
            AgentCli::Grok => CliPreset::Grok,
            AgentCli::Gemini => CliPreset::Gemini,
        }
    }

    /// The transcript adapter that reads this CLI's sessions, if one exists.
    ///
    /// Gemini has none yet: it can extract and it can query memory over MCP,
    /// but its own sessions are not captured.
    #[must_use]
    pub fn capture_source(self) -> Option<Source> {
        match self {
            AgentCli::ClaudeCode => Some(Source::ClaudeCode),
            AgentCli::Codex => Some(Source::Codex),
            AgentCli::Grok => Some(Source::Grok),
            AgentCli::Gemini => None,
        }
    }

    /// The binary to look for and to run, honouring the preset's `*_BIN`
    /// environment override.
    #[must_use]
    pub fn command(self) -> String {
        CliSpec::preset(self.preset()).resolve_command()
    }

    /// Where this CLI's binary lives, if it is on this machine.
    #[must_use]
    pub fn binary_path(self) -> Option<PathBuf> {
        resolve_binary(&self.command())
    }

    /// True when this CLI's binary resolves.
    #[must_use]
    pub fn is_installed(self) -> bool {
        self.binary_path().is_some()
    }

    /// Environment variables this CLI sets in the processes it spawns.
    ///
    /// Only markers observed in a real child environment are listed, because a
    /// wrong guess here silently changes a menu default. Gemini exports no such
    /// marker, so a session under Gemini is simply not identified.
    #[must_use]
    fn session_markers(self) -> &'static [&'static str] {
        match self {
            AgentCli::ClaudeCode => &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"],
            AgentCli::Codex => &["CODEX_SANDBOX", "CODEX_SANDBOX_NETWORK_DISABLED"],
            AgentCli::Grok => &["GROK_SESSION_ID"],
            AgentCli::Gemini => &[],
        }
    }

    /// The full `mcp add` argv registering recall-echo's MCP server with this
    /// client. See the module docs for why all four differ.
    #[must_use]
    pub fn mcp_add_argv(self, exe: &str, entity_root: &Path) -> Vec<String> {
        let server = vec![
            exe.to_string(),
            "mcp".to_string(),
            "--entity-root".to_string(),
            entity_root.display().to_string(),
        ];
        let mut argv = vec![self.command(), "mcp".into(), "add".into()];
        match self {
            // Name, then flags, then `--`, then the server command.
            AgentCli::ClaudeCode | AgentCli::Grok => {
                argv.extend([
                    MCP_SERVER_NAME.into(),
                    "-s".into(),
                    "user".into(),
                    "--".into(),
                ]);
            }
            // Flags first: the name is a positional and `--` is not accepted.
            AgentCli::Gemini => {
                argv.extend(["-s".into(), "user".into(), MCP_SERVER_NAME.into()]);
            }
            // No scope flag; registration is always global.
            AgentCli::Codex => argv.extend([MCP_SERVER_NAME.into(), "--".into()]),
        }
        argv.extend(server);
        argv
    }
}

impl std::fmt::Display for AgentCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Every agent CLI whose binary resolves on this machine.
#[must_use]
pub fn installed() -> Vec<AgentCli> {
    AgentCli::ALL
        .into_iter()
        .filter(|cli| cli.is_installed())
        .collect()
}

/// The CLI this process is running under, when it says so.
///
/// Used only to pick a menu default, so a false negative costs a keystroke and
/// there is no cost at all to the CLIs that stay anonymous.
#[must_use]
pub fn current() -> Option<AgentCli> {
    AgentCli::ALL.into_iter().find(|cli| {
        cli.session_markers()
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    })
}

/// Which CLIs have actually recorded sessions here — the capture layer's own
/// answer, mapped back onto [`AgentCli`].
#[must_use]
pub fn capturing() -> Vec<Source> {
    crate::transcript::detect_installed()
        .iter()
        .map(|adapter| adapter.source())
        .collect()
}

// ── Binary resolution ────────────────────────────────────────────────────

/// Find `command` the way a shell would: as a path if it looks like one,
/// otherwise by walking `PATH`.
#[must_use]
pub fn resolve_binary(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    if command.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(command);
        return is_executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// ── MCP registration ─────────────────────────────────────────────────────

/// What happened when recall-echo's MCP server was offered to one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStatus {
    /// Newly written to that client's config.
    Registered,
    /// The client already had a server under this name.
    AlreadyRegistered,
    /// The client refused, or could not be run. Carries its complaint.
    Failed(String),
}

/// One client's outcome, with the command that produced it — so a failure can
/// be handed to the user verbatim instead of described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpReport {
    pub cli: AgentCli,
    pub status: McpStatus,
    pub command: String,
}

/// Register recall-echo's MCP server with one client.
///
/// Never fails the caller: a client that is missing, broken or unrecognisable
/// yields [`McpStatus::Failed`] carrying the command to run by hand.
pub async fn register_mcp(cli: AgentCli, exe: &str, entity_root: &Path) -> McpReport {
    let argv = cli.mcp_add_argv(exe, entity_root);
    let command = shell_line(&argv);
    let Some((binary, args)) = argv.split_first() else {
        return McpReport {
            cli,
            status: McpStatus::Failed("empty command".into()),
            command,
        };
    };

    let mut process = tokio::process::Command::new(binary);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let status = match tokio::time::timeout(MCP_ADD_TIMEOUT, process.output()).await {
        Err(_) => McpStatus::Failed(format!(
            "{binary} did not finish within {}s",
            MCP_ADD_TIMEOUT.as_secs()
        )),
        Ok(Err(e)) => McpStatus::Failed(format!("could not run {binary}: {e}")),
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            classify(output.status.success(), &text)
        }
    };

    McpReport {
        cli,
        status,
        command,
    }
}

/// Read one client's answer to `mcp add`.
///
/// The exit code alone is not enough: Claude reports an existing registration
/// as a failure, and Gemini reports it as a success it then overwrites.
#[must_use]
pub fn classify(success: bool, output: &str) -> McpStatus {
    let lower = output.to_lowercase();
    let already = ALREADY_PHRASES.iter().any(|phrase| lower.contains(phrase));
    match (success, already) {
        (_, true) => McpStatus::AlreadyRegistered,
        (true, false) => McpStatus::Registered,
        (false, false) => McpStatus::Failed(first_meaningful_line(output)),
    }
}

/// The first line with something in it, trimmed to an excerpt.
fn first_meaningful_line(output: &str) -> String {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    truncate(strip_ansi(line).trim(), OUTPUT_EXCERPT)
}

/// Drop CSI escape sequences, which several of these CLIs colour their errors
/// with even when stdout is not a terminal.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// An argv as a line a user can paste back into a shell.
#[must_use]
pub fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.chars().any(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(cli: AgentCli) -> Vec<String> {
        cli.mcp_add_argv("/usr/local/bin/recall-echo", Path::new("/home/d/entity"))
    }

    /// Verified against `claude mcp add --help` and a real registration:
    /// name first, `-s user` for a global scope, `--` before the server.
    #[test]
    fn claude_registration_puts_the_name_first_and_uses_a_separator() {
        assert_eq!(
            argv_of(AgentCli::ClaudeCode),
            vec![
                "claude",
                "mcp",
                "add",
                "recall-echo",
                "-s",
                "user",
                "--",
                "/usr/local/bin/recall-echo",
                "mcp",
                "--entity-root",
                "/home/d/entity",
            ]
        );
    }

    /// Gemini's name is a yargs positional: it follows the flags, and a `--`
    /// separator is not accepted.
    #[test]
    fn gemini_registration_takes_the_name_after_its_flags_and_no_separator() {
        let argv = argv_of(AgentCli::Gemini);
        assert_eq!(
            argv,
            vec![
                "gemini",
                "mcp",
                "add",
                "-s",
                "user",
                "recall-echo",
                "/usr/local/bin/recall-echo",
                "mcp",
                "--entity-root",
                "/home/d/entity",
            ]
        );
        assert!(!argv.iter().any(|arg| arg == "--"));
    }

    #[test]
    fn grok_registration_matches_claudes_shape() {
        let argv = argv_of(AgentCli::Grok);
        assert_eq!(argv[0], "grok");
        assert_eq!(argv[3..7], ["recall-echo", "-s", "user", "--"]);
    }

    /// Codex has no scope flag — registration is always `~/.codex/config.toml`.
    #[test]
    fn codex_registration_has_no_scope_flag() {
        let argv = argv_of(AgentCli::Codex);
        assert_eq!(
            argv,
            vec![
                "codex",
                "mcp",
                "add",
                "recall-echo",
                "--",
                "/usr/local/bin/recall-echo",
                "mcp",
                "--entity-root",
                "/home/d/entity",
            ]
        );
        assert!(!argv.iter().any(|arg| arg == "-s"));
    }

    /// Whatever the shape, every client is told the same three things.
    #[test]
    fn every_client_is_given_the_same_server_command() {
        for cli in AgentCli::ALL {
            let argv = argv_of(cli);
            let tail = &argv[argv.len() - 4..];
            assert_eq!(
                tail,
                [
                    "/usr/local/bin/recall-echo",
                    "mcp",
                    "--entity-root",
                    "/home/d/entity"
                ],
                "{cli}"
            );
            assert!(argv.contains(&MCP_SERVER_NAME.to_string()), "{cli}");
        }
    }

    /// Claude reports an existing server as a *failure*; treating the exit code
    /// as the answer would report a working setup as broken on every re-run.
    #[test]
    fn claude_already_exists_is_not_a_failure() {
        let status = classify(
            false,
            "MCP server recall-echo already exists in user config",
        );
        assert_eq!(status, McpStatus::AlreadyRegistered);
    }

    /// Gemini says it both ways in one breath, and exits zero.
    #[test]
    fn gemini_already_configured_is_recognised_despite_success() {
        let status = classify(
            true,
            "MCP server \"recall-echo\" is already configured within user settings.\n\
             MCP server \"recall-echo\" updated in user settings.",
        );
        assert_eq!(status, McpStatus::AlreadyRegistered);
    }

    #[test]
    fn a_silent_rewrite_reads_as_registered() {
        let status = classify(true, "Added stdio MCP server 'recall-echo' to user config");
        assert_eq!(status, McpStatus::Registered);
    }

    #[test]
    fn a_real_failure_carries_the_clients_first_line() {
        let status = classify(
            false,
            "\n\u{1b}[31mError: config is read-only\u{1b}[0m\ndetails",
        );
        assert_eq!(
            status,
            McpStatus::Failed("Error: config is read-only".into())
        );
    }

    #[test]
    fn a_failure_with_no_output_still_says_something() {
        assert_eq!(
            classify(false, "   \n\n"),
            McpStatus::Failed("no output".into())
        );
    }

    #[test]
    fn presets_and_sources_line_up_with_the_providers() {
        assert_eq!(AgentCli::Grok.provider(), Provider::Grok);
        assert_eq!(AgentCli::Codex.capture_source(), Some(Source::Codex));
        assert_eq!(
            AgentCli::Gemini.capture_source(),
            None,
            "gemini has no transcript adapter yet"
        );
        for cli in AgentCli::ALL {
            assert_eq!(cli.provider().default_cli_preset(), Some(cli.preset()));
        }
    }

    /// The binary name is the preset's, so `CLAUDE_BIN` and friends steer
    /// detection and the later calls identically.
    #[test]
    fn the_command_is_the_presets_command() {
        assert_eq!(AgentCli::ClaudeCode.command(), "claude");
        assert_eq!(AgentCli::Gemini.command(), "gemini");
        assert_eq!(AgentCli::Grok.command(), "grok");
        assert_eq!(AgentCli::Codex.command(), "codex");
    }

    #[test]
    fn an_explicit_path_is_resolved_without_consulting_path() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mycli");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            resolve_binary(&script.display().to_string()),
            Some(script.clone())
        );
        assert_eq!(
            resolve_binary(&dir.path().join("absent").display().to_string()),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_not_a_binary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        assert_eq!(resolve_binary(&file.display().to_string()), None);
    }

    #[test]
    fn an_empty_command_resolves_to_nothing() {
        assert_eq!(resolve_binary("   "), None);
    }

    #[test]
    fn a_shell_line_quotes_only_what_needs_it() {
        let argv = vec!["claude".into(), "mcp".into(), "/a path/bin".into()];
        assert_eq!(shell_line(&argv), "claude mcp \"/a path/bin\"");
    }
}
