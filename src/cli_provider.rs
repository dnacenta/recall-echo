// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Completions from whichever agent CLI the user already pays for.
//!
//! Every agent CLI worth using has the same headless shape: a binary, a way to
//! hand it a prompt, optional model and output-format flags, and an answer on
//! stdout. So there is one implementation here, and a vendor is a
//! [`CliPreset`] — a set of defaults for the same [`CliSpec`] fields a user can
//! set by hand in `[llm.cli]`. Supporting a CLI nobody has heard of yet is
//! config, not a release.
//!
//! ```text
//! preset defaults ──▶ [llm.cli] overrides ──▶ CliSpec ──▶ argv + stdin
//!                                                     └─▶ OutputMode ──▶ answer
//! ```
//!
//! What the vendors do *not* agree on is stdout: one JSON object, one JSON
//! object per line, or prose. That is an [`OutputMode`], not a code path.

use std::process::Stdio;
use std::time::Duration;

use crate::config::{
    CliPreset, CliSection, JsonPaths, LineMatchers, OutputMode, PromptDelivery, Provider,
};
use crate::error::RecallError;
use crate::graph::error::GraphError;
use crate::graph::llm::LlmProvider;

/// Bytes of a failing CLI's stderr carried into the error.
const STDERR_EXCERPT: usize = 300;
/// Per-call wall-clock limit when no preset or config sets one.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

// ── Resolved spec ────────────────────────────────────────────────────────

/// Everything needed to call one agent CLI once.
///
/// Built by [`CliSpec::resolve`] from a preset plus `[llm.cli]`; pure data, so
/// [`CliSpec::invocation`] is testable without spawning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSpec {
    /// Binary name or path.
    pub command: String,
    /// Environment variable that overrides `command` when config does not.
    pub command_env: Option<String>,
    /// Fixed arguments before every generated flag.
    pub args: Vec<String>,
    /// How the prompt reaches the process.
    pub prompt_delivery: PromptDelivery,
    /// Flag carrying the prompt under [`PromptDelivery::Flag`].
    pub prompt_flag: String,
    /// Flag selecting the model; empty omits it.
    pub model_flag: String,
    /// Model used when the config names none.
    pub default_model: String,
    /// Flag selecting the output format; empty omits it.
    pub output_format_flag: String,
    /// Value for `output_format_flag`; empty passes the flag alone.
    pub output_format_value: String,
    /// Flag carrying the system prompt; empty prepends it to the message.
    pub system_prompt_flag: String,
    /// Shape of the CLI's stdout.
    pub output_mode: OutputMode,
    /// Candidate paths to the answer inside JSON output; empty means stdout is
    /// the answer.
    pub result_json_paths: JsonPaths,
    /// Predicates selecting the answer's line under [`OutputMode::Ndjson`].
    pub ndjson_match: LineMatchers,
    /// Arguments after the generated flags.
    pub extra_args: Vec<String>,
    /// Per-call limit; `None` waits forever.
    pub timeout: Option<Duration>,
    /// Variables unset before spawning.
    pub env_remove: Vec<String>,
}

/// One resolved call: the exact argv, and what to write to stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `argv[0]` is the binary.
    pub argv: Vec<String>,
    /// `None` closes stdin.
    pub stdin: Option<String>,
}

impl CliSpec {
    /// Defaults for a known CLI.
    ///
    /// Verified on live calls against the installed binaries: `claude` 2.1.x,
    /// `grok` (JSON envelope confirmed) and `codex` 0.146.x (NDJSON stream
    /// confirmed). `gemini` 0.27.x is flags-only — its success field could not
    /// be checked without auth, so that preset tries several and falls back to
    /// stdout. See the provider table in the README.
    #[must_use]
    pub fn preset(preset: CliPreset) -> Self {
        match preset {
            // `claude -p --model M --output-format text --system-prompt S
            //  --no-session-persistence`, prompt on stdin. Unchanged since the
            // provider was claude-only; CLAUDECODE is cleared so a session can
            // spawn one.
            CliPreset::ClaudeCode => Self {
                command: "claude".into(),
                command_env: Some("CLAUDE_BIN".into()),
                args: vec!["-p".into()],
                prompt_delivery: PromptDelivery::Stdin,
                prompt_flag: String::new(),
                model_flag: "--model".into(),
                default_model: "sonnet".into(),
                output_format_flag: "--output-format".into(),
                output_format_value: "text".into(),
                system_prompt_flag: "--system-prompt".into(),
                output_mode: OutputMode::Raw,
                result_json_paths: JsonPaths::default(),
                ndjson_match: LineMatchers::default(),
                extra_args: vec!["--no-session-persistence".into()],
                timeout: default_timeout(),
                env_remove: vec!["CLAUDECODE".into()],
            },
            // `gemini -m M -o json -p <prompt>`. The envelope's success field
            // is not documented; `response` then `result` are tried and raw
            // stdout is the fallback, so a rename degrades to noisier output
            // rather than a broken provider.
            CliPreset::Gemini => Self {
                command: "gemini".into(),
                command_env: Some("GEMINI_BIN".into()),
                args: Vec::new(),
                prompt_delivery: PromptDelivery::Flag,
                prompt_flag: "-p".into(),
                model_flag: "-m".into(),
                default_model: String::new(),
                output_format_flag: "-o".into(),
                output_format_value: "json".into(),
                system_prompt_flag: String::new(),
                output_mode: OutputMode::SingleJson,
                result_json_paths: JsonPaths::new(["response".into(), "result".into()]),
                ndjson_match: LineMatchers::default(),
                extra_args: Vec::new(),
                timeout: default_timeout(),
                env_remove: Vec::new(),
            },
            // `grok -m M --output-format json -p <prompt>`. The envelope was
            // read off a live call: the answer is `text`, and `thought` holds
            // reasoning — which is why the path is pinned rather than "first
            // string field".
            CliPreset::Grok => Self {
                command: "grok".into(),
                command_env: Some("GROK_BIN".into()),
                args: Vec::new(),
                prompt_delivery: PromptDelivery::Flag,
                prompt_flag: "-p".into(),
                model_flag: "-m".into(),
                default_model: String::new(),
                output_format_flag: "--output-format".into(),
                output_format_value: "json".into(),
                system_prompt_flag: String::new(),
                output_mode: OutputMode::SingleJson,
                result_json_paths: JsonPaths::new(["text".into()]),
                ndjson_match: LineMatchers::default(),
                extra_args: Vec::new(),
                timeout: default_timeout(),
                env_remove: Vec::new(),
            },
            // `codex exec -m M --json --skip-git-repo-check`, prompt on stdin.
            // Three traps, all encoded here rather than left to the user:
            // `exec` is a subcommand and not a flag; `-p` is `--profile`, so
            // the prompt goes by stdin and never by `-p`; and without
            // `--skip-git-repo-check` codex refuses outside a trusted git
            // directory — which a memory directory usually is not. Its `--json`
            // is a stream of events, so the answer is the last
            // `item.completed` carrying an `agent_message`.
            CliPreset::Codex => Self {
                command: "codex".into(),
                command_env: Some("CODEX_BIN".into()),
                args: vec!["exec".into()],
                prompt_delivery: PromptDelivery::Stdin,
                prompt_flag: String::new(),
                model_flag: "-m".into(),
                default_model: String::new(),
                output_format_flag: "--json".into(),
                output_format_value: String::new(),
                system_prompt_flag: String::new(),
                output_mode: OutputMode::Ndjson,
                result_json_paths: JsonPaths::new(["item.text".into()]),
                ndjson_match: LineMatchers::new([
                    "type=item.completed".into(),
                    "item.type=agent_message".into(),
                ]),
                extra_args: vec!["--skip-git-repo-check".into()],
                timeout: default_timeout(),
                env_remove: Vec::new(),
            },
            // Nothing assumed: a binary, the prompt on stdin, prose out.
            CliPreset::Custom => Self {
                command: String::new(),
                command_env: Some("RECALL_CLI_BIN".into()),
                args: Vec::new(),
                prompt_delivery: PromptDelivery::Stdin,
                prompt_flag: String::new(),
                model_flag: String::new(),
                default_model: String::new(),
                output_format_flag: String::new(),
                output_format_value: String::new(),
                system_prompt_flag: String::new(),
                output_mode: OutputMode::Raw,
                result_json_paths: JsonPaths::default(),
                ndjson_match: LineMatchers::default(),
                extra_args: Vec::new(),
                timeout: default_timeout(),
                env_remove: Vec::new(),
            },
        }
    }

    /// Resolve the spec for a provider: its preset, then `[llm.cli]` on top.
    ///
    /// Fails for the HTTP providers, which have no CLI to spawn, and for a spec
    /// left unusable — no command, or flag delivery with no flag.
    pub fn resolve(provider: &Provider, section: &CliSection) -> Result<Self, RecallError> {
        let preset = section
            .preset
            .or_else(|| provider.default_cli_preset())
            .ok_or_else(|| {
                RecallError::Config(format!(
                    "provider {provider} is not a CLI provider — use create_provider()"
                ))
            })?;

        let mut spec = Self::preset(preset);
        spec.apply(section);
        spec.validate(provider)?;
        Ok(spec)
    }

    fn apply(&mut self, section: &CliSection) {
        if let Some(command) = &section.command {
            self.command = command.clone();
            self.command_env = None;
        }
        if let Some(args) = &section.args {
            self.args = args.clone();
        }
        if let Some(delivery) = section.prompt_delivery {
            self.prompt_delivery = delivery;
        }
        if let Some(flag) = &section.prompt_flag {
            self.prompt_flag = flag.clone();
        }
        if let Some(flag) = &section.model_flag {
            self.model_flag = flag.clone();
        }
        if let Some(flag) = &section.output_format_flag {
            self.output_format_flag = flag.clone();
        }
        if let Some(value) = &section.output_format_value {
            self.output_format_value = value.clone();
        }
        if let Some(flag) = &section.system_prompt_flag {
            self.system_prompt_flag = flag.clone();
        }
        if let Some(paths) = &section.result_json_path {
            self.result_json_paths = paths.clone();
            // Naming a path on a preset that prints prose can only mean the
            // output is JSON; asking for a second key to say so again would be
            // a papercut with no upside.
            if self.output_mode == OutputMode::Raw && !paths.is_empty() {
                self.output_mode = OutputMode::SingleJson;
            }
        }
        if let Some(matchers) = &section.ndjson_match {
            self.ndjson_match = matchers.clone();
        }
        if let Some(mode) = section.output_mode {
            self.output_mode = mode;
        }
        if let Some(args) = &section.extra_args {
            self.extra_args = args.clone();
        }
        if let Some(secs) = section.timeout_secs {
            self.timeout = (secs > 0).then(|| Duration::from_secs(secs));
        }
    }

    fn validate(&self, provider: &Provider) -> Result<(), RecallError> {
        if self.resolve_command().trim().is_empty() {
            return Err(RecallError::Config(format!(
                "provider {provider} has no command — set `[llm.cli] command = \"<binary>\"`"
            )));
        }
        if self.prompt_delivery == PromptDelivery::Flag && self.prompt_flag.is_empty() {
            return Err(RecallError::Config(format!(
                "provider {provider} delivers the prompt by flag but sets no \
                 `[llm.cli] prompt_flag`"
            )));
        }
        Ok(())
    }

    /// The binary to spawn: config first, then the preset's environment
    /// override, then the preset default.
    #[must_use]
    pub fn resolve_command(&self) -> String {
        self.command_env
            .as_ref()
            .and_then(|key| std::env::var(key).ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.command.clone())
    }

    /// The model this spec uses, given what the config asked for.
    #[must_use]
    pub fn resolve_model(&self, configured: &str) -> String {
        if configured.is_empty() {
            self.default_model.clone()
        } else {
            configured.to_string()
        }
    }

    /// Build the exact argv and stdin payload for one completion.
    #[must_use]
    pub fn invocation(&self, model: &str, system_prompt: &str, user_message: &str) -> Invocation {
        let mut argv = vec![self.resolve_command()];
        argv.extend(self.args.iter().cloned());

        if !self.model_flag.is_empty() && !model.is_empty() {
            argv.push(self.model_flag.clone());
            argv.push(model.to_string());
        }
        // A value-less output flag is a boolean switch (`codex --json`).
        if !self.output_format_flag.is_empty() {
            argv.push(self.output_format_flag.clone());
            if !self.output_format_value.is_empty() {
                argv.push(self.output_format_value.clone());
            }
        }

        let prompt = if self.system_prompt_flag.is_empty() {
            fold_system_prompt(system_prompt, user_message)
        } else {
            argv.push(self.system_prompt_flag.clone());
            argv.push(system_prompt.to_string());
            user_message.to_string()
        };

        argv.extend(self.extra_args.iter().cloned());

        let stdin = match self.prompt_delivery {
            PromptDelivery::Stdin => Some(prompt),
            PromptDelivery::Flag => {
                argv.push(self.prompt_flag.clone());
                argv.push(prompt);
                None
            }
            PromptDelivery::Arg => {
                argv.push(prompt);
                None
            }
        };

        Invocation { argv, stdin }
    }

    /// A human-readable, single-line argv for `config show`, with the prompts
    /// as placeholders.
    #[must_use]
    pub fn argv_preview(&self, model: &str) -> String {
        let invocation = self.invocation(model, "<system>", "<prompt>");
        let mut parts: Vec<String> = invocation.argv.iter().map(|arg| quote(arg)).collect();
        if invocation.stdin.is_some() {
            parts.push("< <prompt>".into());
        }
        parts.join(" ")
    }
}

/// Render one argument as a shell reader would expect to see it.
fn quote(arg: &str) -> String {
    if arg.chars().any(char::is_whitespace) {
        format!("\"{}\"", arg.replace('\n', "\\n"))
    } else {
        arg.to_string()
    }
}

/// CLIs with no system-prompt flag get one message, instructions first.
fn fold_system_prompt(system_prompt: &str, user_message: &str) -> String {
    if system_prompt.is_empty() {
        user_message.to_string()
    } else {
        format!("{system_prompt}\n\n{user_message}")
    }
}

fn default_timeout() -> Option<Duration> {
    Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

// ── Provider ─────────────────────────────────────────────────────────────

/// Completes by spawning an agent CLI described by a [`CliSpec`].
///
/// Costs nothing per token: the CLI authenticates with the subscription the
/// user already has.
pub struct CliProvider {
    spec: CliSpec,
    model: String,
}

impl CliProvider {
    #[must_use]
    pub fn new(spec: CliSpec, model: String) -> Self {
        Self { spec, model }
    }

    /// The spec this provider calls.
    #[must_use]
    pub fn spec(&self) -> &CliSpec {
        &self.spec
    }
}

#[async_trait::async_trait]
impl LlmProvider for CliProvider {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        _max_tokens: u32,
    ) -> Result<String, GraphError> {
        let invocation = self
            .spec
            .invocation(&self.model, system_prompt, user_message);
        let output = self.run(&invocation).await?;
        extract_answer(&output, &self.spec)
    }
}

impl CliProvider {
    /// Spawn, feed, wait, and hand back stdout — or the CLI's own complaint.
    async fn run(&self, invocation: &Invocation) -> Result<String, GraphError> {
        let (binary, args) = invocation
            .argv
            .split_first()
            .ok_or_else(|| GraphError::Llm("empty CLI invocation".into()))?;

        let mut command = tokio::process::Command::new(binary);
        command
            .args(args)
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A dropped future (a timeout, a cancelled task) must not leave an
            // agent running against the user's subscription.
            .kill_on_drop(true);
        for key in &self.spec.env_remove {
            command.env_remove(key);
        }

        let mut child = command
            .spawn()
            .map_err(|e| GraphError::Llm(format!("failed to spawn {binary}: {e}")))?;

        if let Some(payload) = &invocation.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(payload.as_bytes())
                    .await
                    .map_err(|e| GraphError::Llm(format!("write to {binary} stdin: {e}")))?;
                drop(stdin);
            }
        }

        let output = match self.spec.timeout {
            Some(limit) => tokio::time::timeout(limit, child.wait_with_output())
                .await
                .map_err(|_| {
                    GraphError::Llm(format!("{binary} timed out after {}s", limit.as_secs()))
                })?,
            None => child.wait_with_output().await,
        }
        .map_err(|e| GraphError::Llm(format!("{binary} process failed: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GraphError::Llm(format!(
                "{binary} exited {}: {}",
                output.status,
                truncate_str(stderr.trim(), STDERR_EXCERPT)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if stdout.trim().is_empty() {
            return Err(GraphError::Llm(format!("{binary} returned empty output")));
        }
        Ok(stdout)
    }
}

// ── Output handling ──────────────────────────────────────────────────────

/// Pull the answer out of a CLI's stdout, according to its [`OutputMode`].
///
/// Forgiving by design: not JSON, no such path, a non-string there — all fall
/// back to stdout rather than failing, because a CLI that renamed a field is
/// still returning a usable answer, and a hard failure here would stall
/// extraction on every archive. The one exception is an envelope that reports
/// its own failure while exiting zero: that is not an answer, and passing it on
/// as one would poison the graph.
fn extract_answer(stdout: &str, spec: &CliSpec) -> Result<String, GraphError> {
    let paths = spec.result_json_paths.paths();
    match spec.output_mode {
        OutputMode::Raw => Ok(stdout.to_string()),
        OutputMode::SingleJson => {
            if paths.is_empty() {
                return Ok(stdout.to_string());
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
                return Ok(stdout.to_string());
            };
            match first_match(&json, paths) {
                Some(text) => Ok(text),
                None => reported_error(&json).map_or_else(|| Ok(stdout.to_string()), Err),
            }
        }
        OutputMode::Ndjson => extract_from_ndjson(stdout, spec),
    }
}

/// Read a stream of events and keep the last one that matches.
///
/// Later events supersede earlier ones — an agent may speak more than once per
/// turn, and it is the final message that answers the prompt.
fn extract_from_ndjson(stdout: &str, spec: &CliSpec) -> Result<String, GraphError> {
    let predicates = spec.ndjson_match.predicates();
    let paths = spec.result_json_paths.paths();

    let mut answer = None;
    let mut error = None;

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if let Some(message) = reported_error(&event) {
            error = Some(message);
        }
        if !predicates
            .iter()
            .all(|(path, expected)| matches_scalar(&event, path, expected))
        {
            continue;
        }
        if let Some(text) = first_match(&event, paths) {
            answer = Some(text);
        }
    }

    match (answer, error) {
        (Some(text), _) => Ok(text),
        (None, Some(err)) => Err(err),
        (None, None) => Ok(stdout.to_string()),
    }
}

/// The first configured path that resolves to a string.
fn first_match(json: &serde_json::Value, paths: &[String]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| lookup(json, path).and_then(serde_json::Value::as_str))
        .map(str::to_string)
}

/// True when the scalar at `path` equals `expected`.
///
/// Strings compare directly — `type=item.completed` needs no quotes in the
/// config — and anything else is read as the JSON literal it is written as, so
/// `final=true` and `index=0` work too.
fn matches_scalar(json: &serde_json::Value, path: &str, expected: &str) -> bool {
    let Some(node) = lookup(json, path) else {
        return false;
    };
    if let Some(text) = node.as_str() {
        return text == expected;
    }
    serde_json::from_str::<serde_json::Value>(expected).is_ok_and(|wanted| *node == wanted)
}

/// A self-reported failure carried in a JSON document, if there is one.
fn reported_error(json: &serde_json::Value) -> Option<GraphError> {
    lookup(json, "error.message")
        .and_then(serde_json::Value::as_str)
        .map(|message| {
            GraphError::Llm(format!(
                "CLI reported an error: {}",
                truncate_str(message, STDERR_EXCERPT)
            ))
        })
}

/// Walk a dotted path; numeric segments index arrays.
fn lookup<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut node = json;
    for segment in path.split('.') {
        node = match node {
            serde_json::Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            other => other.get(segment)?,
        };
    }
    Some(node)
}

fn truncate_str(text: &str, max: usize) -> &str {
    let end = text.len().min(max);
    let mut i = end;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    &text[..i]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const SYSTEM: &str = "You extract entities.";
    const USER: &str = "Dani uses NeoVim.";

    fn spec_for(provider: Provider) -> CliSpec {
        CliSpec::resolve(&provider, &CliSection::default()).expect("preset resolves")
    }

    // ── argv construction ────────────────────────────────────────────────

    /// The claude-code argv is a compatibility surface: it is what every
    /// existing install already runs, so the generic path must reproduce it
    /// exactly.
    #[test]
    fn claude_code_argv_is_unchanged() {
        let spec = spec_for(Provider::ClaudeCode);
        let invocation = spec.invocation("sonnet", SYSTEM, USER);

        assert_eq!(
            invocation.argv,
            vec![
                "claude",
                "-p",
                "--model",
                "sonnet",
                "--output-format",
                "text",
                "--system-prompt",
                SYSTEM,
                "--no-session-persistence",
            ]
        );
        assert_eq!(invocation.stdin.as_deref(), Some(USER));
    }

    #[test]
    fn claude_code_defaults_to_sonnet() {
        let spec = spec_for(Provider::ClaudeCode);
        assert_eq!(spec.resolve_model(""), "sonnet");
        assert_eq!(spec.resolve_model("opus"), "opus");
    }

    #[test]
    fn gemini_argv_passes_the_prompt_by_flag() {
        let spec = spec_for(Provider::Gemini);
        let invocation = spec.invocation("gemini-2.5-pro", SYSTEM, USER);

        assert_eq!(
            invocation.argv,
            vec![
                "gemini",
                "-m",
                "gemini-2.5-pro",
                "-o",
                "json",
                "-p",
                &format!("{SYSTEM}\n\n{USER}"),
            ]
        );
        assert!(invocation.stdin.is_none());
        assert_eq!(spec.result_json_paths.paths(), ["response", "result"]);
    }

    #[test]
    fn grok_argv_matches_the_verified_flags() {
        let spec = spec_for(Provider::Grok);
        let invocation = spec.invocation("grok-4", SYSTEM, USER);

        assert_eq!(
            invocation.argv,
            vec![
                "grok",
                "-m",
                "grok-4",
                "--output-format",
                "json",
                "-p",
                &format!("{SYSTEM}\n\n{USER}"),
            ]
        );
        assert!(invocation.stdin.is_none());
        assert_eq!(spec.result_json_paths.paths(), ["text"]);
    }

    /// grok's envelope carries reasoning in `thought` beside the answer in
    /// `text`; extraction must not take whichever string it meets first.
    #[test]
    fn grok_extracts_text_and_not_the_reasoning() {
        let spec = spec_for(Provider::Grok);
        let stdout = r#"{"thought":"thinking about it","text":"OK","stopReason":"end_turn"}"#;
        assert_eq!(extract_answer(stdout, &spec).unwrap(), "OK");
    }

    /// codex breaks all three habits the other CLIs share: `exec` is a
    /// subcommand, `-p` means `--profile` there, and it refuses to run outside
    /// a trusted git directory without `--skip-git-repo-check`.
    #[test]
    fn codex_argv_uses_the_subcommand_stdin_and_the_repo_check_escape() {
        let spec = spec_for(Provider::Codex);
        let invocation = spec.invocation("gpt-5.1-codex", SYSTEM, USER);

        assert_eq!(
            invocation.argv,
            vec![
                "codex",
                "exec",
                "-m",
                "gpt-5.1-codex",
                "--json",
                "--skip-git-repo-check",
            ]
        );
        assert!(
            !invocation.argv.iter().any(|arg| arg == "-p"),
            "-p is --profile for codex, never the prompt"
        );
        assert_eq!(
            invocation.stdin.as_deref(),
            Some(format!("{SYSTEM}\n\n{USER}").as_str())
        );
        assert_eq!(spec.output_mode, OutputMode::Ndjson);
    }

    /// `--json` takes no value; the flag must still reach the argv.
    #[test]
    fn a_value_less_output_flag_is_passed_alone() {
        let spec = spec_for(Provider::Codex);
        let argv = spec.invocation("", SYSTEM, USER).argv;
        assert_eq!(
            argv,
            vec!["codex", "exec", "--json", "--skip-git-repo-check"]
        );
    }

    #[test]
    fn an_empty_model_omits_the_model_flag() {
        let spec = spec_for(Provider::Gemini);
        let invocation = spec.invocation("", SYSTEM, USER);
        assert_eq!(invocation.argv[1], "-o");
    }

    #[test]
    fn a_custom_provider_needs_a_command() {
        let err = CliSpec::resolve(&Provider::Cli, &CliSection::default())
            .expect_err("custom preset has no default binary");
        assert!(err.to_string().contains("command"), "{err}");
    }

    #[test]
    fn a_custom_provider_is_built_entirely_from_config() {
        let section = CliSection {
            command: Some("mycli".into()),
            args: Some(vec!["chat".into()]),
            prompt_delivery: Some(PromptDelivery::Arg),
            model_flag: Some("--model".into()),
            output_format_flag: Some("--format".into()),
            output_format_value: Some("json".into()),
            result_json_path: Some(JsonPaths::parse("data.text")),
            extra_args: Some(vec!["--quiet".into()]),
            timeout_secs: Some(0),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::Cli, &section).expect("resolves");

        assert_eq!(
            spec.invocation("m1", SYSTEM, USER).argv,
            vec![
                "mycli",
                "chat",
                "--model",
                "m1",
                "--format",
                "json",
                "--quiet",
                &format!("{SYSTEM}\n\n{USER}"),
            ]
        );
        assert!(spec.timeout.is_none(), "0 seconds means no limit");
        assert_eq!(spec.result_json_paths.paths(), ["data.text"]);
    }

    #[test]
    fn overrides_are_applied_on_top_of_a_preset() {
        let section = CliSection {
            command: Some("/opt/bin/gemini".into()),
            model_flag: Some("--model".into()),
            result_json_path: Some(JsonPaths::parse("output")),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::Gemini, &section).expect("resolves");
        let invocation = spec.invocation("flash", SYSTEM, USER);

        assert_eq!(invocation.argv[0], "/opt/bin/gemini");
        assert_eq!(invocation.argv[1], "--model");
        assert_eq!(spec.result_json_paths.paths(), ["output"]);
    }

    #[test]
    fn a_preset_can_be_chosen_independently_of_the_provider() {
        let section = CliSection {
            preset: Some(CliPreset::Gemini),
            command: Some("gemini-next".into()),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::Cli, &section).expect("resolves");
        assert_eq!(spec.prompt_delivery, PromptDelivery::Flag);
        assert_eq!(spec.invocation("", "", USER).argv.last().unwrap(), USER);
    }

    #[test]
    fn flag_delivery_without_a_flag_is_rejected() {
        let section = CliSection {
            command: Some("mycli".into()),
            prompt_delivery: Some(PromptDelivery::Flag),
            ..CliSection::default()
        };
        let err = CliSpec::resolve(&Provider::Cli, &section).expect_err("no prompt flag");
        assert!(err.to_string().contains("prompt_flag"), "{err}");
    }

    #[test]
    fn http_providers_have_no_cli_spec() {
        assert!(CliSpec::resolve(&Provider::Anthropic, &CliSection::default()).is_err());
        assert!(CliSpec::resolve(&Provider::Openai, &CliSection::default()).is_err());
    }

    #[test]
    fn a_missing_system_prompt_still_uses_the_flag_when_there_is_one() {
        let spec = spec_for(Provider::ClaudeCode);
        let invocation = spec.invocation("sonnet", "", USER);
        assert!(invocation.argv.contains(&"--system-prompt".to_string()));
    }

    #[test]
    fn argv_preview_shows_the_stdin_redirect() {
        let preview = spec_for(Provider::ClaudeCode).argv_preview("sonnet");
        assert!(preview.starts_with("claude -p --model sonnet"), "{preview}");
        assert!(preview.ends_with("< <prompt>"), "{preview}");
    }

    /// A preview is one line, whatever the prompt looks like.
    #[test]
    fn argv_preview_keeps_a_folded_prompt_on_one_line() {
        let preview = spec_for(Provider::Grok).argv_preview("grok-4");
        assert!(!preview.contains('\n'), "{preview}");
        assert!(
            preview.ends_with(r#"-p "<system>\n\n<prompt>""#),
            "{preview}"
        );
    }

    // ── JSON extraction ──────────────────────────────────────────────────

    /// A single-JSON spec reading the given paths.
    fn json_spec(paths: &[&str]) -> CliSpec {
        CliSpec {
            output_mode: OutputMode::SingleJson,
            result_json_paths: JsonPaths::new(paths.iter().map(|p| (*p).to_string())),
            ..spec_for(Provider::Gemini)
        }
    }

    #[test]
    fn json_path_present_returns_the_field() {
        let spec = json_spec(&["response", "result"]);
        let answer = extract_answer(r#"{"response":"hello","session_id":"a"}"#, &spec).unwrap();
        assert_eq!(answer, "hello");
    }

    #[test]
    fn json_paths_are_tried_in_order() {
        let spec = json_spec(&["response", "result"]);
        let answer = extract_answer(r#"{"result":"second choice"}"#, &spec).unwrap();
        assert_eq!(answer, "second choice");
    }

    #[test]
    fn nested_and_indexed_paths_resolve() {
        let spec = json_spec(&["messages.0.text"]);
        let answer = extract_answer(r#"{"messages":[{"text":"deep"}]}"#, &spec).unwrap();
        assert_eq!(answer, "deep");
    }

    #[test]
    fn a_missing_json_path_falls_back_to_raw_stdout() {
        let spec = json_spec(&["response"]);
        let stdout = r#"{"text":"renamed field"}"#;
        assert_eq!(extract_answer(stdout, &spec).unwrap(), stdout);
    }

    #[test]
    fn malformed_json_falls_back_to_raw_stdout() {
        let spec = json_spec(&["response"]);
        let stdout = "{not json at all";
        assert_eq!(extract_answer(stdout, &spec).unwrap(), stdout);
    }

    #[test]
    fn a_non_string_at_the_path_falls_back_to_raw_stdout() {
        let spec = json_spec(&["response"]);
        let stdout = r#"{"response":{"text":"nested"}}"#;
        assert_eq!(extract_answer(stdout, &spec).unwrap(), stdout);
    }

    #[test]
    fn an_error_envelope_is_an_error_not_an_answer() {
        let spec = json_spec(&["response"]);
        let err = extract_answer(r#"{"error":{"message":"quota exceeded"}}"#, &spec)
            .expect_err("error envelope");
        assert!(err.to_string().contains("quota exceeded"), "{err}");
    }

    #[test]
    fn raw_mode_means_stdout_is_the_answer() {
        let spec = spec_for(Provider::ClaudeCode);
        assert_eq!(spec.output_mode, OutputMode::Raw);
        assert_eq!(extract_answer("plain prose", &spec).unwrap(), "plain prose");
    }

    // ── NDJSON extraction ────────────────────────────────────────────────

    /// Verbatim from a live `codex exec --json` run.
    const CODEX_STREAM: &str = concat!(
        r#"{"type":"thread.started","thread_id":"01999"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":13658,"output_tokens":5}}"#,
        "\n",
    );

    #[test]
    fn ndjson_takes_the_matching_event_and_ignores_the_rest() {
        let spec = spec_for(Provider::Codex);
        assert_eq!(extract_answer(CODEX_STREAM, &spec).unwrap(), "OK");
    }

    /// An agent may speak more than once; the final message is the answer.
    #[test]
    fn ndjson_keeps_the_last_matching_event() {
        let spec = spec_for(Provider::Codex);
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"ignore me"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"final"}}"#,
        );
        assert_eq!(extract_answer(stream, &spec).unwrap(), "final");
    }

    #[test]
    fn ndjson_skips_lines_that_are_not_json() {
        let spec = spec_for(Provider::Codex);
        let stream = format!("a banner line\n{CODEX_STREAM}");
        assert_eq!(extract_answer(&stream, &spec).unwrap(), "OK");
    }

    #[test]
    fn ndjson_with_no_matching_event_falls_back_to_raw_stdout() {
        let spec = spec_for(Provider::Codex);
        let stream = r#"{"type":"turn.completed","usage":{"output_tokens":5}}"#;
        assert_eq!(extract_answer(stream, &spec).unwrap(), stream);
    }

    #[test]
    fn ndjson_surfaces_an_error_event_when_nothing_matched() {
        let spec = spec_for(Provider::Codex);
        let stream = r#"{"type":"error","error":{"message":"model overloaded"}}"#;
        let err = extract_answer(stream, &spec).expect_err("error event");
        assert!(err.to_string().contains("model overloaded"), "{err}");
    }

    /// Predicates address any dotted path, so the mode serves the next
    /// streaming CLI without touching this file.
    #[test]
    fn ndjson_matchers_are_configurable() {
        let section = CliSection {
            command: Some("mycli".into()),
            output_mode: Some(OutputMode::Ndjson),
            ndjson_match: Some(LineMatchers::parse("kind=message, final=true")),
            result_json_path: Some(JsonPaths::parse("content")),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::Cli, &section).expect("resolves");
        let stream = concat!(
            r#"{"kind":"message","final":false,"content":"partial"}"#,
            "\n",
            r#"{"kind":"message","final":true,"content":"done"}"#,
        );
        assert_eq!(extract_answer(stream, &spec).unwrap(), "done");
    }

    /// Naming a result path where the preset expected prose is an unambiguous
    /// statement that the output is JSON.
    #[test]
    fn a_result_path_on_a_prose_preset_implies_single_json() {
        let section = CliSection {
            result_json_path: Some(JsonPaths::parse("result")),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::ClaudeCode, &section).expect("resolves");
        assert_eq!(spec.output_mode, OutputMode::SingleJson);
        assert_eq!(
            extract_answer(r#"{"result":"from json"}"#, &spec).unwrap(),
            "from json"
        );
    }

    #[test]
    fn an_explicit_output_mode_wins_over_the_inference() {
        let section = CliSection {
            result_json_path: Some(JsonPaths::parse("result")),
            output_mode: Some(OutputMode::Raw),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::ClaudeCode, &section).expect("resolves");
        assert_eq!(spec.output_mode, OutputMode::Raw);
        assert_eq!(
            extract_answer(r#"{"result":"from json"}"#, &spec).unwrap(),
            r#"{"result":"from json"}"#
        );
    }

    // ── Spawning, against generated mock CLIs ────────────────────────────

    struct MockCli {
        _dir: tempfile::TempDir,
        path: PathBuf,
        argv_log: PathBuf,
        stdin_log: PathBuf,
    }

    impl MockCli {
        /// A shell script that records how it was called, then behaves as told.
        fn new(body: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("mock-cli");
            let argv_log = dir.path().join("argv.txt");
            let stdin_log = dir.path().join("stdin.txt");
            // NUL-separated: arguments carry newlines when a system prompt is
            // folded into the message.
            let script = format!(
                "#!/bin/sh\n\
                 : > '{argv}'\n\
                 for a in \"$@\"; do printf '%s\\0' \"$a\" >> '{argv}'; done\n\
                 cat > '{stdin}'\n\
                 {body}\n",
                argv = argv_log.display(),
                stdin = stdin_log.display(),
            );
            fs::write(&path, script).expect("write mock");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            Self {
                _dir: dir,
                path,
                argv_log,
                stdin_log,
            }
        }

        fn provider(&self, section: CliSection, model: &str) -> CliProvider {
            let section = CliSection {
                command: Some(self.path.display().to_string()),
                ..section
            };
            let spec = CliSpec::resolve(&Provider::Cli, &section).expect("resolves");
            let model = spec.resolve_model(model);
            CliProvider::new(spec, model)
        }

        fn recorded_argv(&self) -> Vec<String> {
            fs::read_to_string(&self.argv_log)
                .unwrap_or_default()
                .split('\0')
                .filter(|arg| !arg.is_empty())
                .map(str::to_string)
                .collect()
        }

        fn recorded_stdin(&self) -> String {
            fs::read_to_string(&self.stdin_log).unwrap_or_default()
        }
    }

    #[tokio::test]
    async fn stdin_delivery_feeds_the_prompt_to_the_process() {
        let mock = MockCli::new("printf 'answer from stdin'");
        let provider = mock.provider(
            CliSection {
                preset: Some(CliPreset::ClaudeCode),
                ..CliSection::default()
            },
            "sonnet",
        );

        let answer = provider.complete(SYSTEM, USER, 1024).await.unwrap();

        assert_eq!(answer, "answer from stdin");
        assert_eq!(
            mock.recorded_argv(),
            vec![
                "-p",
                "--model",
                "sonnet",
                "--output-format",
                "text",
                "--system-prompt",
                SYSTEM,
                "--no-session-persistence",
            ]
        );
        assert_eq!(mock.recorded_stdin(), USER);
    }

    #[tokio::test]
    async fn flag_delivery_puts_the_prompt_in_argv_and_leaves_stdin_closed() {
        let mock = MockCli::new(r#"printf '{"response":"answer from flag"}'"#);
        let provider = mock.provider(
            CliSection {
                preset: Some(CliPreset::Gemini),
                ..CliSection::default()
            },
            "",
        );

        let answer = provider.complete(SYSTEM, USER, 1024).await.unwrap();

        assert_eq!(answer, "answer from flag");
        assert_eq!(
            mock.recorded_argv(),
            vec!["-o", "json", "-p", &format!("{SYSTEM}\n\n{USER}")]
        );
        assert!(mock.recorded_stdin().is_empty());
    }

    #[tokio::test]
    async fn a_streaming_cli_is_read_through_the_ndjson_mode() {
        let mock = MockCli::new(
            "printf '%s\\n' '{\"type\":\"turn.started\"}' \
             '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"OK\"}}' \
             '{\"type\":\"turn.completed\"}'",
        );
        let provider = mock.provider(
            CliSection {
                preset: Some(CliPreset::Codex),
                ..CliSection::default()
            },
            "",
        );

        let answer = provider.complete(SYSTEM, USER, 1024).await.unwrap();

        assert_eq!(answer, "OK");
        assert_eq!(
            mock.recorded_argv(),
            vec!["exec", "--json", "--skip-git-repo-check"]
        );
        assert_eq!(mock.recorded_stdin(), format!("{SYSTEM}\n\n{USER}"));
    }

    #[tokio::test]
    async fn a_non_zero_exit_surfaces_the_cli_stderr() {
        let mock = MockCli::new("printf 'not authenticated: run gemini auth' >&2\nexit 3");
        let provider = mock.provider(
            CliSection {
                preset: Some(CliPreset::Gemini),
                ..CliSection::default()
            },
            "",
        );

        let err = provider
            .complete(SYSTEM, USER, 1024)
            .await
            .expect_err("non-zero exit");
        let message = err.to_string();
        assert!(message.contains("not authenticated"), "{message}");
        assert!(message.contains("exited"), "{message}");
    }

    #[tokio::test]
    async fn empty_output_is_an_error() {
        let mock = MockCli::new("printf ''");
        let provider = mock.provider(CliSection::default(), "");

        let err = provider
            .complete(SYSTEM, USER, 1024)
            .await
            .expect_err("no output");
        assert!(err.to_string().contains("empty output"), "{err}");
    }

    #[tokio::test]
    async fn a_hung_cli_hits_the_timeout() {
        let mock = MockCli::new("sleep 30\nprintf 'too late'");
        let provider = mock.provider(
            CliSection {
                timeout_secs: Some(1),
                ..CliSection::default()
            },
            "",
        );

        let err = provider
            .complete(SYSTEM, USER, 1024)
            .await
            .expect_err("timeout");
        assert!(err.to_string().contains("timed out after 1s"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_as_a_spawn_failure() {
        let section = CliSection {
            command: Some("/nonexistent/agent-cli".into()),
            ..CliSection::default()
        };
        let spec = CliSpec::resolve(&Provider::Cli, &section).expect("resolves");
        let provider = CliProvider::new(spec, String::new());

        let err = provider
            .complete(SYSTEM, USER, 1024)
            .await
            .expect_err("missing binary");
        assert!(err.to_string().contains("failed to spawn"), "{err}");
    }
}
