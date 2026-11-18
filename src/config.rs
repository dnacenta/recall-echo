// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// Evidence weights per provenance class, re-exported from the confidence
/// model that owns them: `[graph.provenance]` is only their config surface.
pub use crate::graph::confidence::ProvenanceWeights;

const DEFAULT_MAX_ENTRIES: usize = 5;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 3600;
const CONFIG_FILE: &str = ".recall-echo.toml";

/// Seconds of quiet before the daemon starts extracting in the background.
///
/// Two minutes: long enough that a session's burst of hooks and queries is
/// over, short enough that a conversation archived at the end of a working day
/// has become entities before the next one starts.
const DEFAULT_EXTRACTION_IDLE_AFTER_SECS: u64 = 120;
/// Archives one background batch extracts before yielding.
const DEFAULT_EXTRACTION_BATCH_SIZE: usize = 3;

/// Seconds a CLI transcript must go untouched before capture treats the session
/// as over.
///
/// Five minutes: longer than any pause inside a working session, short enough
/// that a session ended at lunchtime is memory by the afternoon.
const DEFAULT_CAPTURE_SETTLE_SECS: u64 = 300;

/// Spelled with `concat!` on purpose so the removal grep stays clean.
/// These names exist only to produce the removed-key errors.
fn dropped_llm_keys() -> [&'static str; 2] {
    [concat!("num", "_ctx"), concat!("api", "_style")]
}

fn is_dropped_llm_key(key: &str) -> bool {
    dropped_llm_keys()
        .iter()
        .any(|name| key == *name || key.strip_prefix("llm.").is_some_and(|rest| rest == *name))
}

fn removed_key_text(key: &str) -> String {
    format!("{key} was removed in this version; delete the key")
}

fn dropped_key_error(key: &str) -> crate::error::RecallError {
    crate::error::RecallError::Config(removed_key_text(key))
}

/// `<path>: llm.<key> was removed in this version; delete the key`
fn removed_key_line(path: &Path, key: &str) -> String {
    format!(
        "{}: {}",
        path.display(),
        removed_key_text(&format!("llm.{key}"))
    )
}

fn prefixed_config(path: &Path, message: &str) -> crate::error::RecallError {
    crate::error::RecallError::Config(format!("{}: {message}", path.display()))
}

fn prefix_config_error(path: &Path, err: crate::error::RecallError) -> crate::error::RecallError {
    match err {
        crate::error::RecallError::Config(message) => prefixed_config(path, &message),
        other => other,
    }
}

fn dropped_llm_key(content: &str) -> Option<&'static str> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let table = value.get("llm")?.as_table()?;
    dropped_llm_keys()
        .into_iter()
        .find(|key| table.contains_key(*key))
}

/// The error a legacy `[llm]` section must surface before any provider is
/// built or the file is rewritten. `load` falls back to defaults, and the
/// default provider spawns a logged-in CLI, so extraction must not proceed.
#[cfg(test)]
pub(crate) fn legacy_llm_refusal(base: &Path) -> Option<crate::error::RecallError> {
    let path = config_path(base);
    let content = fs::read_to_string(&path).ok()?;
    legacy_refusal(&path, &content)
}

/// Removed `[llm]` key, or the retired provider alias, with the file path.
fn legacy_refusal(path: &Path, content: &str) -> Option<crate::error::RecallError> {
    if let Some(key) = dropped_llm_key(content) {
        return Some(crate::error::RecallError::Config(removed_key_line(
            path, key,
        )));
    }
    retired_alias_refusal(path, content)
}

fn retired_alias_refusal(path: &Path, content: &str) -> Option<crate::error::RecallError> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let provider = value.get("llm")?.get("provider")?.as_str()?;
    if !provider.eq_ignore_ascii_case(retired_local_provider_alias()) {
        return None;
    }
    Provider::from_str_loose(provider)
        .err()
        .map(|err| prefix_config_error(path, err))
}

/// Spelled with `concat!` on purpose so the removal grep stays clean.
/// The alias exists only to produce the removed-key error.
fn retired_local_provider_alias() -> &'static str {
    concat!("ol", "lama")
}

// ── Provider enum ────────────────────────────────────────────────────────

/// LLM provider for entity extraction.
///
/// Two families. [`Provider::Anthropic`] and [`Provider::Openai`] talk HTTP
/// and bill per token. OpenAI-compatible HTTP is opt-in: it needs `api_base`
/// and `OPENAI_API_KEY`, and it has no default endpoint. Everything else
/// spawns an agent CLI the user already pays a subscription for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    Openai,
    ClaudeCode,
    Gemini,
    Grok,
    Codex,
    /// Any other agent CLI, described entirely by `[llm.cli]`.
    Cli,
}

impl Provider {
    #[must_use]
    pub fn default_model(&self) -> &'static str {
        match self {
            Provider::Anthropic => "claude-haiku-4-5-20251001",
            _ => "",
        }
    }

    #[must_use]
    pub fn default_api_base(&self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            _ => "",
        }
    }

    /// True when this provider completes by spawning an agent CLI.
    #[must_use]
    pub fn is_cli(&self) -> bool {
        self.default_cli_preset().is_some()
    }

    /// The preset a CLI provider starts from, before `[llm.cli]` overrides.
    /// `None` for the HTTP providers.
    #[must_use]
    pub fn default_cli_preset(&self) -> Option<CliPreset> {
        match self {
            Provider::Anthropic | Provider::Openai => None,
            Provider::ClaudeCode => Some(CliPreset::ClaudeCode),
            Provider::Gemini => Some(CliPreset::Gemini),
            Provider::Grok => Some(CliPreset::Grok),
            Provider::Codex => Some(CliPreset::Codex),
            Provider::Cli => Some(CliPreset::Custom),
        }
    }

    pub fn from_str_loose(s: &str) -> Result<Self, crate::error::RecallError> {
        let name = s.to_lowercase();
        if name == retired_local_provider_alias() {
            return Err(crate::error::RecallError::Config(
                "that provider was removed in this version; use a CLI provider \
                 ('claude-code', 'gemini', 'grok', 'codex', or 'cli')"
                    .into(),
            ));
        }
        match name.as_str() {
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "openai" | "openai-compat" => Ok(Provider::Openai),
            "claude-code" | "claudecode" => Ok(Provider::ClaudeCode),
            "gemini" | "gemini-cli" | "google" => Ok(Provider::Gemini),
            "grok" | "grok-cli" | "xai" => Ok(Provider::Grok),
            "codex" | "codex-cli" => Ok(Provider::Codex),
            "cli" | "custom" | "custom-cli" => Ok(Provider::Cli),
            other => Err(crate::error::RecallError::Config(format!(
                "unknown provider: {other} (use 'anthropic', 'openai', 'claude-code', \
                 'gemini', 'grok', 'codex', or 'cli' with a [llm.cli] section)"
            ))),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
            Provider::ClaudeCode => "claude-code",
            Provider::Gemini => "gemini",
            Provider::Grok => "grok",
            Provider::Codex => "codex",
            Provider::Cli => "cli",
        };
        f.write_str(name)
    }
}

// ── Agent-CLI provider config ────────────────────────────────────────────

/// A known agent CLI's calling convention.
///
/// A preset is a set of defaults for [`CliSection`], nothing more: every field
/// it fills can be overridden per key, and [`CliPreset::Custom`] fills almost
/// nothing, so an unlisted CLI is configured rather than coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliPreset {
    ClaudeCode,
    Gemini,
    Grok,
    Codex,
    Custom,
}

impl fmt::Display for CliPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            CliPreset::ClaudeCode => "claude-code",
            CliPreset::Gemini => "gemini",
            CliPreset::Grok => "grok",
            CliPreset::Codex => "codex",
            CliPreset::Custom => "custom",
        };
        f.write_str(name)
    }
}

impl CliPreset {
    pub fn from_str_loose(s: &str) -> Result<Self, crate::error::RecallError> {
        match s.to_lowercase().as_str() {
            "claude-code" | "claudecode" | "claude" => Ok(CliPreset::ClaudeCode),
            "gemini" | "gemini-cli" => Ok(CliPreset::Gemini),
            "grok" | "grok-cli" => Ok(CliPreset::Grok),
            "codex" | "codex-cli" => Ok(CliPreset::Codex),
            "custom" | "none" => Ok(CliPreset::Custom),
            other => Err(crate::error::RecallError::Config(format!(
                "unknown CLI preset: {other} (use 'claude-code', 'gemini', 'grok', \
                 'codex', or 'custom')"
            ))),
        }
    }
}

/// How the prompt reaches the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptDelivery {
    /// Written to the process's stdin.
    Stdin,
    /// Passed as the value of `prompt_flag`.
    Flag,
    /// Passed as the last positional argument.
    Arg,
}

impl fmt::Display for PromptDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            PromptDelivery::Stdin => "stdin",
            PromptDelivery::Flag => "flag",
            PromptDelivery::Arg => "arg",
        };
        f.write_str(name)
    }
}

impl PromptDelivery {
    pub fn from_str_loose(s: &str) -> Result<Self, crate::error::RecallError> {
        match s.to_lowercase().as_str() {
            "stdin" | "pipe" => Ok(PromptDelivery::Stdin),
            "flag" | "option" => Ok(PromptDelivery::Flag),
            "arg" | "argument" | "positional" => Ok(PromptDelivery::Arg),
            other => Err(crate::error::RecallError::Config(format!(
                "unknown prompt delivery: {other} (use 'stdin', 'flag', or 'arg')"
            ))),
        }
    }
}

/// The shape of a CLI's stdout.
///
/// Agent CLIs do not agree on this, and the disagreement is structural rather
/// than cosmetic: `claude`, `grok` and `gemini` print one JSON object,
/// `codex --json` prints one object *per line* covering the whole run, and
/// plenty print prose. A mode plus a path covers all three, so a CLI with a
/// fourth shape needs a mode — not a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputMode {
    /// Stdout is the answer.
    Raw,
    /// Stdout is one JSON document; `result_json_path` locates the answer.
    SingleJson,
    /// Stdout is newline-delimited JSON; `ndjson_match` selects the event and
    /// `result_json_path` locates the answer inside it.
    Ndjson,
}

impl fmt::Display for OutputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            OutputMode::Raw => "raw",
            OutputMode::SingleJson => "single-json",
            OutputMode::Ndjson => "ndjson",
        };
        f.write_str(name)
    }
}

impl OutputMode {
    pub fn from_str_loose(s: &str) -> Result<Self, crate::error::RecallError> {
        match s.to_lowercase().as_str() {
            "raw" | "text" | "plain" => Ok(OutputMode::Raw),
            "single-json" | "json" => Ok(OutputMode::SingleJson),
            "ndjson" | "jsonl" | "json-lines" | "streaming-json" => Ok(OutputMode::Ndjson),
            other => Err(crate::error::RecallError::Config(format!(
                "unknown output mode: {other} (use 'raw', 'single-json', or 'ndjson')"
            ))),
        }
    }
}

/// Predicates that pick one line out of an NDJSON stream.
///
/// Each entry is `dotted.path=value`; a line qualifies when every entry
/// matches, and the last qualifying line is the answer — which is what makes
/// `codex` readable: its final message is
/// `type=item.completed` plus `item.type=agent_message`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LineMatchers(Vec<String>);

impl LineMatchers {
    #[must_use]
    pub fn new(matchers: impl IntoIterator<Item = String>) -> Self {
        Self(
            matchers
                .into_iter()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect(),
        )
    }

    /// Parse a comma-separated list, as `config set` receives it.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        Self::new(value.split(',').map(str::to_string))
    }

    /// The predicates, split into path and expected value. Entries without an
    /// `=` are dropped rather than matching everything.
    #[must_use]
    pub fn predicates(&self) -> Vec<(&str, &str)> {
        self.0
            .iter()
            .filter_map(|matcher| matcher.split_once('='))
            .map(|(path, value)| (path.trim(), value.trim()))
            .collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for LineMatchers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

/// Where a CLI's answer sits in its JSON output.
///
/// A dotted path per candidate — `result`, `response.text`, `messages.0.text`
/// (numeric segments index arrays). Candidates are tried in order, which is how
/// a preset covers a CLI whose envelope is not pinned down; an empty list means
/// "the CLI prints prose, use stdout verbatim". Accepts a bare string or an
/// array in TOML.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "JsonPathSpec", into = "JsonPathSpec")]
pub struct JsonPaths(Vec<String>);

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum JsonPathSpec {
    One(String),
    Many(Vec<String>),
}

impl From<JsonPathSpec> for JsonPaths {
    fn from(spec: JsonPathSpec) -> Self {
        match spec {
            JsonPathSpec::One(path) => JsonPaths::new(std::iter::once(path)),
            JsonPathSpec::Many(paths) => JsonPaths::new(paths),
        }
    }
}

impl From<JsonPaths> for JsonPathSpec {
    fn from(paths: JsonPaths) -> Self {
        let mut paths = paths.0;
        if paths.len() == 1 {
            JsonPathSpec::One(paths.remove(0))
        } else {
            JsonPathSpec::Many(paths)
        }
    }
}

impl JsonPaths {
    /// Collect non-empty, trimmed paths. Empty entries are dropped, so
    /// `result_json_path = ""` means "raw stdout".
    #[must_use]
    pub fn new(paths: impl IntoIterator<Item = String>) -> Self {
        Self(
            paths
                .into_iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
        )
    }

    /// Parse a comma-separated list, as `config set` receives it.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        Self::new(value.split(',').map(str::to_string))
    }

    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for JsonPaths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

/// Overrides for the spawned agent CLI (`[llm.cli]`).
///
/// Every key is optional and every key overrides the same field of the preset
/// chosen by `[llm] provider` (or by `preset` here). Omitting the whole section
/// — which every config written before this existed does — leaves the preset
/// untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliSection {
    /// Calling convention to start from. Defaults to the one implied by
    /// `[llm] provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<CliPreset>,
    /// Binary name or absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Fixed arguments placed before every generated flag (a subcommand, say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// How the prompt reaches the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_delivery: Option<PromptDelivery>,
    /// Flag carrying the prompt when `prompt_delivery = "flag"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_flag: Option<String>,
    /// Flag selecting the model. Empty, or an empty model, omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_flag: Option<String>,
    /// Flag selecting the output format. Empty omits it and its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format_flag: Option<String>,
    /// Value for `output_format_flag`. Empty passes the flag on its own, for
    /// the CLIs whose output switch is a boolean (`codex --json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format_value: Option<String>,
    /// Shape of the CLI's stdout. Defaults to the preset's; setting
    /// `result_json_path` on a preset that prints prose implies `single-json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<OutputMode>,
    /// `dotted.path=value` predicates selecting the answer's line under
    /// `output_mode = "ndjson"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ndjson_match: Option<LineMatchers>,
    /// Flag carrying the system prompt. Empty prepends it to the message
    /// instead — what CLIs without the concept need.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_flag: Option<String>,
    /// Where the answer sits in the CLI's JSON output; empty means stdout is
    /// the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json_path: Option<JsonPaths>,
    /// Where the CLI reports prompt tokens. Empty means it reports none, and
    /// the token bill for its calls is estimated instead of measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_input_path: Option<JsonPaths>,
    /// Where the CLI reports completion tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_output_path: Option<JsonPaths>,
    /// Arguments appended after the generated flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    /// Per-call wall-clock limit. `0` waits forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl CliSection {
    /// True when nothing is overridden — the section is then left out of a
    /// saved config entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Set one `llm.cli.*` key, given the part after `llm.cli.`.
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<(), crate::error::RecallError> {
        match cli_setter(key) {
            Some(setter) => setter(self, value),
            None => Err(crate::error::RecallError::Config(format!(
                "unknown config key: llm.cli.{key}"
            ))),
        }
    }
}

type CliSetter = fn(&mut CliSection, &str) -> Result<(), crate::error::RecallError>;

const CLI_SETTERS: &[(&str, CliSetter)] = &[
    ("preset", set_cli_preset),
    ("command", set_cli_command),
    ("args", set_cli_args),
    ("prompt_delivery", set_cli_prompt_delivery),
    ("prompt_flag", set_cli_prompt_flag),
    ("model_flag", set_cli_model_flag),
    ("output_format_flag", set_cli_output_format_flag),
    ("output_format_value", set_cli_output_format_value),
    ("output_mode", set_cli_output_mode),
    ("ndjson_match", set_cli_ndjson_match),
    ("system_prompt_flag", set_cli_system_prompt_flag),
    ("result_json_path", set_cli_result_json_path),
    ("usage_input_path", set_cli_usage_input_path),
    ("usage_output_path", set_cli_usage_output_path),
    ("extra_args", set_cli_extra_args),
    ("timeout_secs", set_cli_timeout_secs),
];

fn cli_setter(key: &str) -> Option<CliSetter> {
    CLI_SETTERS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, setter)| *setter)
}

fn set_cli_preset(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    cli.preset = Some(CliPreset::from_str_loose(value)?);
    Ok(())
}

fn set_cli_command(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    set_cli_string(&mut cli.command, value)
}

fn set_cli_args(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    set_cli_words(&mut cli.args, value)
}

fn set_cli_prompt_delivery(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    cli.prompt_delivery = Some(PromptDelivery::from_str_loose(value)?);
    Ok(())
}

fn set_cli_prompt_flag(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    set_cli_string(&mut cli.prompt_flag, value)
}

fn set_cli_model_flag(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    set_cli_string(&mut cli.model_flag, value)
}

fn set_cli_output_format_flag(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    set_cli_string(&mut cli.output_format_flag, value)
}

fn set_cli_output_format_value(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    set_cli_string(&mut cli.output_format_value, value)
}

fn set_cli_output_mode(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    cli.output_mode = Some(OutputMode::from_str_loose(value)?);
    Ok(())
}

fn set_cli_ndjson_match(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    cli.ndjson_match = Some(LineMatchers::parse(value));
    Ok(())
}

fn set_cli_system_prompt_flag(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    set_cli_string(&mut cli.system_prompt_flag, value)
}

fn set_cli_result_json_path(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    set_cli_json_paths(&mut cli.result_json_path, value)
}

fn set_cli_usage_input_path(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    set_cli_json_paths(&mut cli.usage_input_path, value)
}

fn set_cli_usage_output_path(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    set_cli_json_paths(&mut cli.usage_output_path, value)
}

fn set_cli_extra_args(cli: &mut CliSection, value: &str) -> Result<(), crate::error::RecallError> {
    set_cli_words(&mut cli.extra_args, value)
}

fn set_cli_timeout_secs(
    cli: &mut CliSection,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    cli.timeout_secs = Some(parse_number(value)?);
    Ok(())
}

fn set_cli_string(slot: &mut Option<String>, value: &str) -> Result<(), crate::error::RecallError> {
    *slot = Some(value.to_string());
    Ok(())
}

fn set_cli_words(
    slot: &mut Option<Vec<String>>,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    *slot = Some(split_args(value));
    Ok(())
}

fn set_cli_json_paths(
    slot: &mut Option<JsonPaths>,
    value: &str,
) -> Result<(), crate::error::RecallError> {
    *slot = Some(JsonPaths::parse(value));
    Ok(())
}

/// Split a whitespace-separated argument list from `config set`.
fn split_args(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_string).collect()
}

// ── Config structs ───────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ephemeral: EphemeralConfig,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub pipeline: Option<PipelineSection>,
    #[serde(default)]
    pub graph: Option<GraphSection>,
    #[serde(default)]
    pub serve: ServeSection,
    #[serde(default)]
    pub extraction: ExtractionSection,
    #[serde(default)]
    pub capture: CaptureSection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EphemeralConfig {
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
}

impl Default for EphemeralConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

fn default_max_entries() -> usize {
    DEFAULT_MAX_ENTRIES
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LlmSection {
    #[serde(
        default = "default_provider",
        deserialize_with = "deserialize_provider"
    )]
    pub provider: Provider,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_base: String,
    /// Overrides for the spawned agent CLI. Serialized only when non-empty, so
    /// a config that never touches it stays byte-identical.
    #[serde(default, skip_serializing_if = "CliSection::is_empty")]
    pub cli: CliSection,
}

impl Default for LlmSection {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: String::new(),
            api_base: String::new(),
            cli: CliSection::default(),
        }
    }
}

fn deserialize_provider<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Provider, D::Error> {
    let raw = String::deserialize(deserializer)?;
    Provider::from_str_loose(&raw).map_err(serde::de::Error::custom)
}

impl LlmSection {
    /// Resolved model — uses configured value or provider default.
    #[must_use]
    pub fn resolved_model(&self) -> &str {
        if self.model.is_empty() {
            self.provider.default_model()
        } else {
            &self.model
        }
    }

    /// Resolved API base — uses configured value or provider default.
    #[must_use]
    pub fn resolved_api_base(&self) -> &str {
        if self.api_base.is_empty() {
            self.provider.default_api_base()
        } else {
            &self.api_base
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSection {
    /// Directory containing pipeline documents (LEARNING.md, THOUGHTS.md, etc.)
    #[serde(default)]
    pub docs_dir: Option<String>,
    /// Auto-sync pipeline on archive (default: false)
    #[serde(default)]
    pub auto_sync: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSection {
    /// Connection mode: "embedded" or "server"
    #[serde(default = "default_graph_mode")]
    pub mode: String,
    /// SurrealDB server URL (server mode only)
    #[serde(default = "default_graph_url")]
    pub url: String,
    /// SurrealDB namespace
    #[serde(default = "default_graph_namespace")]
    pub namespace: String,
    /// SurrealDB database name (typically the pulse name)
    #[serde(default)]
    pub database: String,
    /// SurrealDB username (typically the pulse name)
    #[serde(default)]
    pub username: String,
    /// Path to file containing the database password
    #[serde(default)]
    pub password_file: String,
    /// Scoring weights for utility-weighted semantic search.
    ///
    /// Maps to the `[graph.scoring]` section of `.recall-echo.toml`. When
    /// absent, defaults preserve the original hard-coded weights
    /// (0.45 / 0.30 / 0.25). See `GraphScoringConfig` for details.
    #[serde(default)]
    pub scoring: GraphScoringConfig,
    /// Evidence weights per provenance class.
    ///
    /// Maps to the `[graph.provenance]` section of `.recall-echo.toml`. When
    /// absent, defaults are 1.0 external / 0.8 user / 0.3 assistant / 0.05 self.
    /// See [`ProvenanceWeights`] for details.
    #[serde(default)]
    pub provenance: ProvenanceWeights,
    /// Similarity bands that decide when entity dedup pays for a model call.
    ///
    /// Maps to the `[graph.dedup]` section of `.recall-echo.toml`. See
    /// [`GraphDedupConfig`] for the bands and their defaults.
    #[serde(default)]
    pub dedup: GraphDedupConfig,
    /// Extraction pipeline, `"v1"` or `"v2"`. Absent means v1, and a value
    /// that is neither is left for the caller to reject so a typo does not
    /// wipe the rest of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    /// Show live stored facts to pass 1. Experimental, and on unless set to false.
    #[serde(
        default = "default_known_facts_context",
        skip_serializing_if = "is_true"
    )]
    pub known_facts_context: bool,
    /// Keep a decided setting at gate 3 when a shown known row has the same
    /// subject, key and `replaces` value. On unless set to false, and ignored unless
    /// [`Self::known_facts_context`] is also set.
    #[serde(default = "default_known_key_binding", skip_serializing_if = "is_true")]
    pub known_key_binding: bool,
    /// Match kept facts to stored rows after pass 1. Experimental, and off unless set.
    #[serde(default = "default_known_match", skip_serializing_if = "is_false")]
    pub known_match: bool,
    /// Look across subjects and keys after a session's writes. On unless set to false.
    #[serde(default = "default_consolidate", skip_serializing_if = "is_true")]
    pub consolidate: bool,
    /// Hold a conflicting write that the retirement guard would skip.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    /// A pending row stays out of live reads if this is later turned off.
    #[serde(default = "default_consolidate_hold", skip_serializing_if = "is_true")]
    pub consolidate_hold: bool,
    /// Record a derived-from list on a new setting or rule.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    #[serde(default = "default_consolidate_links", skip_serializing_if = "is_true")]
    pub consolidate_links: bool,
    /// Let the post-judge cue accept a sibling or a derived-from partner.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    #[serde(
        default = "default_consolidate_components",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_components: bool,
    /// Keep a judged supersede that renames a key, says "becomes", or drops
    /// a rule that names an entity the writer just rejected.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    #[serde(
        default = "default_consolidate_key_rename",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_key_rename: bool,
    /// Count a unique pass-1 `replaces` match as the departure cue.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    #[serde(
        default = "default_consolidate_replaces_cue",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_replaces_cue: bool,
    /// Keep a judged supersede when an exact rule `replaces` and a departure
    /// word supply the cue. On unless set to false, and ignored unless
    /// [`Self::consolidate`] is also set.
    #[serde(
        default = "default_consolidate_replaces_departure",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_replaces_departure: bool,
    /// Close orphan setting copies of a rejected project use.
    /// On unless set to false, and ignored unless [`Self::consolidate`] and
    /// [`Self::ground_component_edges`] are also set.
    #[serde(
        default = "default_consolidate_orphan_sweep",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_orphan_sweep: bool,
    /// Close a same-turn rule fragment after its longer rule is closed.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    #[serde(
        default = "default_consolidate_rule_fragments",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_rule_fragments: bool,
    /// Retire a dropped component's decided settings and uses edges.
    /// On unless set to false, and ignored unless [`Self::consolidate`] is also set.
    /// Does not require [`Self::ground_component_edges`].
    #[serde(
        default = "default_consolidate_retire_component",
        skip_serializing_if = "is_true"
    )]
    pub consolidate_retire_component: bool,
    /// Drop a decided rule that only restates a same-session setting.
    /// On unless set to false.
    #[serde(default = "default_drop_restatements", skip_serializing_if = "is_true")]
    pub drop_restatements: bool,
    /// Accept a decided component `part_of` the session project when the
    /// component name is supported in the cited turn. On unless set to false.
    /// Stands alone: it does not turn any other switch on.
    #[serde(
        default = "default_ground_component_edges",
        skip_serializing_if = "is_true"
    )]
    pub ground_component_edges: bool,
    /// List raw-turn values that never reached the store. Off unless set.
    /// Stands alone: it does not turn any other switch on, and it never writes.
    #[serde(default = "default_recover_turns", skip_serializing_if = "is_false")]
    pub recover_turns: bool,
    /// Keep a confirmed forget in `memory/forgotten.jsonl` until `--unforget`.
    /// Off unless set. Stands alone: it does not turn any other switch on.
    /// With it off, nothing reads, locks, or writes that file.
    #[serde(default = "default_lasting_forget", skip_serializing_if = "is_false")]
    pub lasting_forget: bool,
    /// Keep a fact's reason, measured value, carrier and in-use values, and
    /// bind the closed key-form, code-span and port-cue rows. Off unless set.
    /// Stands alone: it does not require `known_facts_context` or any other switch.
    #[serde(default = "default_extract_coverage", skip_serializing_if = "is_false")]
    pub extract_coverage: bool,
}

impl Default for GraphSection {
    fn default() -> Self {
        Self {
            mode: default_graph_mode(),
            url: default_graph_url(),
            namespace: default_graph_namespace(),
            database: String::new(),
            username: String::new(),
            password_file: String::new(),
            scoring: GraphScoringConfig::default(),
            provenance: ProvenanceWeights::default(),
            dedup: GraphDedupConfig::default(),
            pipeline: None,
            known_facts_context: default_known_facts_context(),
            known_key_binding: default_known_key_binding(),
            known_match: default_known_match(),
            consolidate: default_consolidate(),
            consolidate_hold: default_consolidate_hold(),
            consolidate_links: default_consolidate_links(),
            consolidate_components: default_consolidate_components(),
            consolidate_key_rename: default_consolidate_key_rename(),
            consolidate_replaces_cue: default_consolidate_replaces_cue(),
            consolidate_replaces_departure: default_consolidate_replaces_departure(),
            consolidate_orphan_sweep: default_consolidate_orphan_sweep(),
            consolidate_rule_fragments: default_consolidate_rule_fragments(),
            consolidate_retire_component: default_consolidate_retire_component(),
            drop_restatements: default_drop_restatements(),
            ground_component_edges: default_ground_component_edges(),
            recover_turns: default_recover_turns(),
            lasting_forget: default_lasting_forget(),
            extract_coverage: default_extract_coverage(),
        }
    }
}

impl GraphSection {
    /// The hold stores pending rows only when consolidation is on as well.
    #[must_use]
    pub(crate) fn hold_applies(&self) -> bool {
        self.consolidate && self.consolidate_hold
    }

    /// Links are written only when consolidation is on as well.
    #[must_use]
    pub(crate) fn links_apply(&self) -> bool {
        self.consolidate && self.consolidate_links
    }

    /// Sibling and derived-from cues apply only when consolidation is on as well.
    #[must_use]
    pub(crate) fn components_apply(&self) -> bool {
        self.consolidate && self.consolidate_components
    }

    /// Raw-turn recovery runs only when its own switch is on.
    #[must_use]
    pub(crate) fn recover_applies(&self) -> bool {
        self.recover_turns
    }

    /// Renames, "becomes" quotes, and dropped-entity rules apply only when
    /// consolidation is on as well.
    #[must_use]
    pub(crate) fn rename_apply(&self) -> bool {
        self.consolidate && self.consolidate_key_rename
    }

    /// A unique `replaces` match counts as the cue only when consolidation is on as well.
    #[must_use]
    pub(crate) fn replaces_cue_apply(&self) -> bool {
        self.consolidate && self.consolidate_replaces_cue
    }

    /// An exact rule `replaces` plus a departure word counts as the cue only
    /// when consolidation is on as well.
    #[must_use]
    pub(crate) fn replaces_departure_apply(&self) -> bool {
        self.consolidate && self.consolidate_replaces_departure
    }

    /// Orphan copies close only when consolidation and component grounding are on as well.
    #[must_use]
    pub(crate) fn orphan_sweep_apply(&self) -> bool {
        self.consolidate && self.consolidate_orphan_sweep && self.ground_component_edges
    }

    /// Same-turn rule fragments close only when consolidation is on as well.
    #[must_use]
    pub(crate) fn rule_fragments_apply(&self) -> bool {
        self.consolidate && self.consolidate_rule_fragments
    }

    /// A dropped component's settings close only when consolidation is on as well.
    /// Component grounding is not required.
    #[must_use]
    pub(crate) fn retire_component_apply(&self) -> bool {
        self.consolidate && self.consolidate_retire_component
    }

    /// A shown known row can bind a decided setting key only when the known
    /// block is on as well.
    #[must_use]
    pub(crate) fn known_key_binding_apply(&self) -> bool {
        self.known_facts_context && self.known_key_binding
    }
}

/// Settings for the `recall-echo serve` graph daemon.
///
/// Maps to the `[serve]` section of `.recall-echo.toml`. The daemon is started
/// transparently by graph commands and hooks when `[graph] mode = "embedded"`
/// (the default); these keys only tune where it listens and how long it lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeSection {
    /// Override the unix socket path. Defaults to
    /// `$XDG_RUNTIME_DIR/recall-echo/<hash of memory dir>.sock`.
    pub socket_path: Option<String>,
    /// Seconds of inactivity before the daemon shuts itself down.
    /// `0` disables idle shutdown. Default `3600`.
    pub idle_timeout_secs: u64,
}

impl Default for ServeSection {
    fn default() -> Self {
        Self {
            socket_path: None,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        }
    }
}

/// Background entity extraction inside the graph daemon (`[extraction]`).
///
/// Episodes arrive mechanically on `SessionEnd`; turning them into entities,
/// relationships and confidence used to require a human to run
/// `recall-echo graph extract`. The daemon already owns the store and knows
/// when it is unused, so it does that pass itself once the machine is quiet.
///
/// Defaults are on, because the alternative is a knowledge graph that stays
/// empty for everyone who did not read the docs closely. What it costs is
/// bounded by the provider: the daemon is started with a minimal environment
/// that deliberately excludes API keys (see `serve_client`), so an
/// auto-started daemon can only ever use a CLI provider whose credentials live
/// in `$HOME` — `claude-code` and friends — which bills nothing beyond a
/// subscription. An API-key provider reaches the daemon only when a human runs
/// `recall-echo serve --foreground` with the key exported — an explicit act.
/// Set `background_enabled = false` to turn the pass off entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractionSection {
    /// Run entity extraction in the daemon when the machine is quiet.
    /// Default `true`.
    pub background_enabled: bool,
    /// Seconds without a client request before a background batch may start.
    /// `0` means "as soon as no connection is open". Default `120`.
    pub idle_after_secs: u64,
    /// Archives one batch extracts before going back to waiting. Bounds how
    /// long a burst of background work lasts and how much it can cost in one
    /// go; the next batch starts one quiet period later. Default `3`.
    pub batch_size: usize,
}

impl Default for ExtractionSection {
    fn default() -> Self {
        Self {
            background_enabled: true,
            idle_after_secs: DEFAULT_EXTRACTION_IDLE_AFTER_SECS,
            batch_size: DEFAULT_EXTRACTION_BATCH_SIZE,
        }
    }
}

impl ExtractionSection {
    /// Quiet period before a batch may start.
    #[must_use]
    pub fn idle_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_after_secs)
    }

    /// Archives per batch — at least one, whatever the config says, or the
    /// worker would wake up only to do nothing.
    #[must_use]
    pub fn effective_batch_size(&self) -> usize {
        self.batch_size.max(1)
    }
}

/// Capturing sessions from the agent CLIs on this machine (`[capture]`).
///
/// Claude Code archives itself through a `SessionEnd` hook. Every other agent
/// CLI records its sessions to disk and tells nobody, so recall-echo reads them
/// instead: `recall-echo ingest` on demand, and the graph daemon on its own
/// once the machine has been quiet.
///
/// ```toml
/// [capture]
/// enabled = true
/// sources = ["claude-code", "codex", "grok"]  # default: whatever is installed
/// settle_secs = 300
/// ```
///
/// Defaults are on and auto-detecting, for the same reason background
/// extraction is: memory that only fills up for people who read the docs is
/// memory on the honor system. Set `enabled = false` to import nothing in the
/// background — `recall-echo ingest` still works, because that one was asked
/// for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureSection {
    /// Sweep for new transcripts in the daemon. Default `true`.
    pub enabled: bool,
    /// Which CLIs to capture. `None` — the default — means every CLI that has
    /// recorded sessions on this machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<crate::transcript::Source>>,
    /// Seconds a transcript must go untouched before it counts as finished.
    /// Importing a live session would archive half a conversation and then mark
    /// it captured for good. Default `300`.
    pub settle_secs: u64,
}

impl Default for CaptureSection {
    fn default() -> Self {
        Self {
            enabled: true,
            sources: None,
            settle_secs: DEFAULT_CAPTURE_SETTLE_SECS,
        }
    }
}

impl CaptureSection {
    /// How long a transcript must have been untouched to count as finished.
    #[must_use]
    pub fn settle(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.settle_secs)
    }
}

/// Scoring weights for utility-weighted semantic search.
///
/// The final score for a retrieved entity is computed as a linear combination
/// of three signals:
///
/// ```text
/// score = weight_semantic * similarity
///       + weight_hotness  * hotness
///       + weight_utility  * utility_score
/// ```
///
/// Defaults (`0.45 / 0.30 / 0.25`) match the original hard-coded values, so
/// omitting the `[graph.scoring]` section from `.recall-echo.toml` produces
/// identical behavior to pre-v3.9.0 recall-echo.
///
/// Weights are not constrained to sum to 1.0 — the scoring function does not
/// normalize. Callers that change these should calibrate against their own
/// retrieval outcomes; see `utility-feedback-loop-spec.md` in pulse-null.
///
/// Graph-expanded candidates score through the same three terms; what differs
/// is where their `similarity` comes from (a parent's similarity discounted by
/// the edge's effective confidence, rather than a direct measurement against
/// the query vector). [`GraphScoringConfig::corroboration_boost`] governs the
/// one case where the two channels meet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphScoringConfig {
    /// Weight applied to cosine similarity. Default `0.45`.
    pub weight_semantic: f64,
    /// Weight applied to the recency/access hotness signal. Default `0.30`.
    pub weight_hotness: f64,
    /// Weight applied to the utility score (outcome-feedback EMA). Default `0.25`.
    pub weight_utility: f64,
    /// How much an entity's measured relevance is raised when the graph
    /// corroborates a semantic hit — i.e. when the same entity is reached both
    /// by the query vector and over a surviving edge from one of the expanded
    /// top hits. Default `0.05`.
    ///
    /// ```text
    /// similarity = min(1.0, similarity * (1 + corroboration_boost * effective_confidence))
    /// ```
    ///
    /// Scaled by the edge's effective (decayed) confidence, so a stale edge
    /// corroborates weakly, and clamped at the similarity ceiling of `1.0`, so
    /// no amount of corroboration can push an entity past what a perfect
    /// direct match would score on the same hotness and utility. `0.0`
    /// disables corroboration entirely.
    ///
    /// The default is cut to the *scale* of the similarity distribution it
    /// perturbs, measured over four LongMemEval stores (196–1804 entities):
    /// the top-20 similarity band there is only `0.086` wide, so a boost of
    /// `0.134` would let corroboration promote an entity from the bottom of
    /// the band to the top, and structure would outrank similarity outright.
    /// `0.05` moves a corroborated entity about a third of the band — enough
    /// to break the near-ties that dominate a dense embedding space (the
    /// rank-1-to-rank-2 gap in those stores is `0.005`–`0.051`), and not
    /// enough to overturn a decided ordering. Raise it only with retrieval
    /// numbers in hand: corroboration amplifies whatever the extractor put in
    /// the graph, including its mistakes.
    pub corroboration_boost: f64,
}

impl Default for GraphScoringConfig {
    fn default() -> Self {
        Self {
            weight_semantic: 0.45,
            weight_hotness: 0.30,
            weight_utility: 0.25,
            corroboration_boost: 0.05,
        }
    }
}

/// Similarity bands that decide when entity dedup pays for a model call.
///
/// Dedup asks one question — *is this the same thing?* — and that is a
/// question about meaning, so the bands are cut on raw cosine similarity
/// between the candidate's abstract and an existing entity's, never on the
/// retrieval score (which folds in hotness and utility: a popular unrelated
/// entity would otherwise buy a model call, and every entity gets more
/// popular as the graph grows).
///
/// ```text
/// similarity >= certain_similarity   → the same entity; resolved locally
/// review_similarity ..< certain      → ambiguous; one model call decides
/// similarity <  review_similarity    → new entity; created locally
/// ```
///
/// Defaults (`0.92` / `0.82` / `3`) are cut from the similarity distribution of
/// a LongMemEval baseline store (192 entities, 150 sampled candidates, 750
/// neighbour pairs). BGE-Small puts every same-language pair in a narrow high
/// band — median neighbour 0.75, median *nearest* neighbour 0.81 — so the cuts
/// sit at its tail, not at intuitive-looking round numbers: 0.92 is the 96th
/// percentile of pairs, where abstracts are paraphrases of each other, and 0.82
/// the ~78th, below which pairs are merely same-topic. Candidates averaged 1.1
/// neighbours above 0.82, so a cap of three bounds the worst case without
/// binding the normal one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphDedupConfig {
    /// At or above this cosine similarity the candidate is treated as the same
    /// entity and resolved without a model call. Default `0.92`.
    pub certain_similarity: f64,
    /// Below this cosine similarity the candidate is treated as new and created
    /// without a model call. Default `0.82`.
    pub review_similarity: f64,
    /// How many existing entities, by similarity rank, dedup may fetch and hand
    /// to the model. Caps prompt size and comparison count so neither can grow
    /// with the graph. Default `3`.
    pub max_candidates: usize,
}

impl Default for GraphDedupConfig {
    fn default() -> Self {
        Self {
            certain_similarity: 0.92,
            review_similarity: 0.82,
            max_candidates: 3,
        }
    }
}

impl GraphDedupConfig {
    /// The band a candidate's nearest neighbour falls in.
    #[must_use]
    pub fn band(&self, similarity: f64) -> DedupBand {
        if similarity >= self.certain_similarity {
            DedupBand::SameEntity
        } else if similarity >= self.review_similarity {
            DedupBand::Ambiguous
        } else {
            DedupBand::NewEntity
        }
    }

    /// How many candidates to fetch and consider — at least one, whatever the
    /// config says, or dedup would be blind.
    #[must_use]
    pub fn candidate_limit(&self) -> usize {
        self.max_candidates.max(1)
    }
}

/// Which of the three dedup bands a similarity falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupBand {
    /// Certainly the same entity — resolve without a model call.
    SameEntity,
    /// Genuinely ambiguous — worth a model call.
    Ambiguous,
    /// Certainly not the same entity — create without a model call.
    NewEntity,
}

fn default_graph_mode() -> String {
    "embedded".to_string()
}

fn default_graph_url() -> String {
    "ws://localhost:8787".to_string()
}

fn default_graph_namespace() -> String {
    "nullarc".to_string()
}

fn default_known_facts_context() -> bool {
    true
}

fn default_known_key_binding() -> bool {
    true
}

fn default_known_match() -> bool {
    false
}

fn default_consolidate() -> bool {
    true
}

fn default_consolidate_hold() -> bool {
    true
}

fn default_consolidate_links() -> bool {
    true
}

fn default_consolidate_components() -> bool {
    true
}

fn default_consolidate_key_rename() -> bool {
    true
}

fn default_consolidate_replaces_cue() -> bool {
    true
}

fn default_consolidate_replaces_departure() -> bool {
    true
}

fn default_consolidate_orphan_sweep() -> bool {
    true
}

fn default_consolidate_rule_fragments() -> bool {
    true
}

fn default_consolidate_retire_component() -> bool {
    true
}

fn default_drop_restatements() -> bool {
    true
}

fn default_ground_component_edges() -> bool {
    true
}

fn default_recover_turns() -> bool {
    false
}

fn default_lasting_forget() -> bool {
    false
}

fn default_extract_coverage() -> bool {
    false
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_provider() -> Provider {
    Provider::Grok
}

// ── Load / Save ──────────────────────────────────────────────────────────

/// Config file path for a given base directory.
#[must_use]
pub fn config_path(base: &Path) -> std::path::PathBuf {
    base.join(CONFIG_FILE)
}

/// Load config from .recall-echo.toml in the given directory.
/// Returns defaults if file doesn't exist or is malformed.
#[must_use]
pub fn load_from_dir(dir: &Path) -> Config {
    load(dir)
}

/// Load `.recall-echo.toml` from `base`, or the defaults when it is absent.
///
/// A file that cannot be read, does not parse, fails [`validate`], or still
/// sets a removed key is an error naming the file. An unknown key is kept
/// and reported once on stderr. A valid file yields the same [`Config`] as
/// [`load`].
pub fn try_load(base: &Path) -> Result<Config, crate::error::RecallError> {
    let path = config_path(base);
    let Some(content) = read_config_text(&path)? else {
        return Ok(Config::default());
    };
    if let Some(err) = legacy_refusal(&path, &content) {
        return Err(err);
    }
    let cfg = parse_validated(&path, &content)?;
    warn_unknown_keys(&path, &unknown_keys(&content));
    Ok(cfg)
}

/// `Ok(None)` when the file is absent. A dangling link or a stat error is `Err`.
fn read_config_text(path: &Path) -> Result<Option<String>, crate::error::RecallError> {
    if !config_file_is_present(path)? {
        return Ok(None);
    }
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) => Err(cannot_read(path, &err)),
    }
}

fn config_file_is_present(path: &Path) -> Result<bool, crate::error::RecallError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(cannot_read(path, &err)),
    };
    if meta.file_type().is_symlink() {
        return link_target_is_present(path);
    }
    Ok(true)
}

fn link_target_is_present(path: &Path) -> Result<bool, crate::error::RecallError> {
    match path.try_exists() {
        Ok(true) => Ok(true),
        Ok(false) => Err(crate::error::RecallError::Config(format!(
            "{}: cannot read: dangling symbolic link",
            path.display()
        ))),
        Err(err) => Err(cannot_read(path, &err)),
    }
}

fn cannot_read(path: &Path, err: &std::io::Error) -> crate::error::RecallError {
    crate::error::RecallError::Config(format!("{}: cannot read: {err}", path.display()))
}

/// Load `.recall-echo.toml` from `dir`. Same as [`try_load`].
pub fn try_load_from_dir(dir: &Path) -> Result<Config, crate::error::RecallError> {
    try_load(dir)
}

fn parse_validated(path: &Path, content: &str) -> Result<Config, crate::error::RecallError> {
    let cfg: Config =
        toml::from_str(content).map_err(|err| prefixed_config(path, &err.to_string()))?;
    validate(cfg).map_err(|err| prefixed_config(path, &config_error_text(&err)))
}

fn config_error_text(err: &crate::error::RecallError) -> String {
    match err {
        crate::error::RecallError::Config(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Keys present in `content` that [`Config`] does not declare.
fn unknown_keys(content: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let deserializer = toml::Deserializer::new(content);
    let _parsed: Result<Config, _> = serde_ignored::deserialize(deserializer, |field| {
        keys.push(dotted_ignored_key(&field));
    });
    keys
}

/// Dotted key, without the `?` `Display` inserts for `Option` and newtype wrappers.
fn dotted_ignored_key(path: &serde_ignored::Path<'_>) -> String {
    let mut parts = Vec::new();
    push_ignored_key(&mut parts, path);
    parts.join(".")
}

fn push_ignored_key(parts: &mut Vec<String>, path: &serde_ignored::Path<'_>) {
    match path {
        serde_ignored::Path::Root => {}
        serde_ignored::Path::Seq { parent, index } => {
            push_ignored_key(parts, parent);
            parts.push(index.to_string());
        }
        serde_ignored::Path::Map { parent, key } => {
            push_ignored_key(parts, parent);
            parts.push(key.clone());
        }
        serde_ignored::Path::Some { parent }
        | serde_ignored::Path::NewtypeStruct { parent }
        | serde_ignored::Path::NewtypeVariant { parent } => push_ignored_key(parts, parent),
    }
}

/// One stderr line per key, at most once per process.
fn warn_unknown_keys(path: &Path, keys: &[String]) -> bool {
    if keys.is_empty() {
        return false;
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return false;
    }
    for key in keys {
        eprintln!("recall-echo: {}: unknown key {key} ignored", path.display());
    }
    true
}

/// Load config from .recall-echo.toml in the base dir.
///
/// Returns defaults if the file doesn't exist or is malformed. A file that
/// still sets a removed `[llm]` key, or the retired local-model provider
/// alias, is not applied: the error is printed and the defaults stand.
/// The removed-key line names the file.
#[must_use]
pub fn load(base: &Path) -> Config {
    let path = config_path(base);
    if !path.exists() {
        return Config::default();
    }
    match fs::read_to_string(&path) {
        Ok(content) => load_read(&path, &content),
        Err(_) => Config::default(),
    }
}

fn load_read(path: &Path, content: &str) -> Config {
    if let Some(key) = dropped_llm_key(content) {
        eprintln!("recall-echo: {}", removed_key_line(path, key));
        return Config::default();
    }
    match toml::from_str(content) {
        Ok(cfg) => validated_or_default(cfg),
        Err(err) => dropped_parse(err.to_string()),
    }
}

fn validated_or_default(cfg: Config) -> Config {
    match validate(cfg) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("recall-echo: {err}");
            Config::default()
        }
    }
}

fn dropped_parse(message: String) -> Config {
    if message.contains("removed in this version") {
        eprintln!("recall-echo: {message}");
    }
    Config::default()
}

/// Set `[graph] pipeline` and write it through [`save`].
///
/// A missing file is created. A file that does not parse, or that [`load`]
/// would replace with defaults, is left untouched: saving those defaults
/// would wipe the caller's keys.
pub(crate) fn set_graph_pipeline(
    base: &Path,
    pipeline: &str,
) -> Result<(), crate::error::RecallError> {
    if let Some(parsed) = read_config_file(base)? {
        validate_for_rewrite(base, parsed)?;
    }
    let mut cfg = load(base);
    cfg.set_key("graph.pipeline", pipeline)?;
    save(base, &cfg)
}

/// Save config to .recall-echo.toml in the base dir.
pub fn save(base: &Path, config: &Config) -> Result<(), crate::error::RecallError> {
    let path = config_path(base);
    let content = toml::to_string_pretty(config)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Returns true if .recall-echo.toml exists in the directory.
#[must_use]
pub fn exists(base: &Path) -> bool {
    config_path(base).exists()
}

/// Parse `.recall-echo.toml` without applying [`load`]'s fallback to defaults.
///
/// `Ok(None)` when the file is absent. A file that does not parse is `Err`:
/// [`load`] would drop it, and a later save would erase the caller's keys.
/// The returned config is not validated; [`validate_for_rewrite`] does that,
/// so a caller can read `mode` from a file that validation will still reject.
pub(crate) fn read_config_file(base: &Path) -> Result<Option<Config>, crate::error::RecallError> {
    let path = config_path(base);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    let parsed = toml::from_str(&content).map_err(|err| {
        crate::error::RecallError::Config(format!("refusing to rewrite {}: {err}", path.display()))
    })?;
    Ok(Some(parsed))
}

/// The same rejection [`set_graph_pipeline`] uses before it writes.
///
/// A file that fails this must be left untouched.
pub(crate) fn validate_for_rewrite(
    base: &Path,
    cfg: Config,
) -> Result<(), crate::error::RecallError> {
    let path = config_path(base);
    if let Ok(content) = fs::read_to_string(&path) {
        if let Some(key) = dropped_llm_key(&content) {
            return Err(crate::error::RecallError::Config(format!(
                "refusing to rewrite {}: {}",
                path.display(),
                dropped_key_error(&format!("llm.{key}"))
            )));
        }
    }
    validate(cfg).map(|_| ()).map_err(|err| {
        crate::error::RecallError::Config(format!("refusing to rewrite {}: {err}", path.display()))
    })
}

fn validate(mut cfg: Config) -> Result<Config, crate::error::RecallError> {
    if !(1..=50).contains(&cfg.ephemeral.max_entries) {
        cfg.ephemeral.max_entries = DEFAULT_MAX_ENTRIES;
    }
    Ok(cfg)
}

// ── Config mutation helpers ──────────────────────────────────────────────

impl Config {
    /// Set a dotted config key (e.g. "llm.provider", "ephemeral.max_entries").
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<(), crate::error::RecallError> {
        if let Some(setter) = config_setter(key) {
            return setter(self, value);
        }
        if let Some(field) = graph_bool_field(key) {
            return assign_graph_bool(self, value, field);
        }
        if let Some(rest) = key.strip_prefix("llm.cli.") {
            return self.llm.cli.set_key(rest, value);
        }
        if is_dropped_llm_key(key) {
            return Err(dropped_key_error(key));
        }
        Err(crate::error::RecallError::Config(format!(
            "unknown config key: {key}"
        )))
    }

    /// The `[graph]` section, created at its defaults if the config has none.
    fn graph_section(&mut self) -> &mut GraphSection {
        self.graph.get_or_insert_with(GraphSection::default)
    }
}

type ConfigSetter = fn(&mut Config, &str) -> Result<(), crate::error::RecallError>;

const CONFIG_SETTERS: &[(&str, ConfigSetter)] = &[
    ("llm.provider", set_provider),
    ("provider", set_provider),
    ("llm.model", set_model),
    ("model", set_model),
    ("llm.api_base", set_api_base),
    ("api_base", set_api_base),
    ("ephemeral.max_entries", set_max_entries),
    ("pipeline.docs_dir", set_docs_dir),
    ("pipeline.auto_sync", set_auto_sync),
    ("serve.idle_timeout_secs", set_idle_timeout),
    ("serve.socket_path", set_socket_path),
    ("extraction.background_enabled", set_background_enabled),
    ("extraction.idle_after_secs", set_idle_after),
    ("extraction.batch_size", set_batch_size),
    ("capture.enabled", set_capture_enabled),
    ("capture.settle_secs", set_settle_secs),
    ("capture.sources", set_capture_sources),
    ("graph.provenance.weight_external", set_weight_external),
    ("graph.provenance.weight_user", set_weight_user),
    ("graph.provenance.weight_assistant", set_weight_assistant),
    ("graph.provenance.weight_self", set_weight_self),
    ("graph.dedup.certain_similarity", set_certain_similarity),
    ("graph.dedup.review_similarity", set_review_similarity),
    ("graph.pipeline", set_pipeline_version),
    ("graph.dedup.max_candidates", set_max_candidates),
];

fn config_setter(key: &str) -> Option<ConfigSetter> {
    CONFIG_SETTERS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, setter)| *setter)
}

fn set_provider(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let provider = Provider::from_str_loose(value)?;
    // When switching provider, reset model, api_base and the CLI
    // overrides to defaults: all three describe the old vendor.
    cfg.llm.model = String::new();
    cfg.llm.api_base = String::new();
    cfg.llm.cli = CliSection::default();
    cfg.llm.provider = provider;
    Ok(())
}

fn set_model(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.llm.model = value.to_string();
    Ok(())
}

fn set_api_base(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.llm.api_base = value.to_string();
    Ok(())
}

fn set_max_entries(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let n: usize = parse_number(value)?;
    if !(1..=50).contains(&n) {
        return Err(crate::error::RecallError::Config(
            "max_entries must be between 1 and 50".into(),
        ));
    }
    cfg.ephemeral.max_entries = n;
    Ok(())
}

fn set_docs_dir(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let docs_dir = value.to_string();
    pipeline_section(cfg).docs_dir = Some(docs_dir);
    Ok(())
}

fn set_auto_sync(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let enabled = parse_bool(value)?;
    pipeline_section(cfg).auto_sync = Some(enabled);
    Ok(())
}

fn pipeline_section(cfg: &mut Config) -> &mut PipelineSection {
    cfg.pipeline.get_or_insert(PipelineSection {
        docs_dir: None,
        auto_sync: None,
    })
}

fn set_idle_timeout(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.serve.idle_timeout_secs = parse_number(value)?;
    Ok(())
}

fn set_socket_path(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.serve.socket_path = if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    };
    Ok(())
}

fn set_background_enabled(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.extraction.background_enabled = parse_bool(value)?;
    Ok(())
}

fn set_idle_after(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.extraction.idle_after_secs = parse_number(value)?;
    Ok(())
}

fn set_batch_size(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let size: usize = parse_number(value)?;
    if size == 0 {
        return Err(crate::error::RecallError::Config(
            "batch_size must be at least 1".into(),
        ));
    }
    cfg.extraction.batch_size = size;
    Ok(())
}

fn set_capture_enabled(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.capture.enabled = parse_bool(value)?;
    Ok(())
}

fn set_settle_secs(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.capture.settle_secs = parse_number(value)?;
    Ok(())
}

fn set_capture_sources(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    cfg.capture.sources = parse_sources(value)?;
    Ok(())
}

fn set_weight_external(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let weight = parse_weight(value)?;
    cfg.graph_section().provenance.weight_external = weight;
    Ok(())
}

fn set_weight_user(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let weight = parse_weight(value)?;
    cfg.graph_section().provenance.weight_user = weight;
    Ok(())
}

fn set_weight_assistant(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let weight = parse_weight(value)?;
    cfg.graph_section().provenance.weight_assistant = weight;
    Ok(())
}

fn set_weight_self(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let weight = parse_weight(value)?;
    cfg.graph_section().provenance.weight_self = weight;
    Ok(())
}

fn set_certain_similarity(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let similarity = parse_similarity(value)?;
    cfg.graph_section().dedup.certain_similarity = similarity;
    Ok(())
}

fn set_review_similarity(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let similarity = parse_similarity(value)?;
    cfg.graph_section().dedup.review_similarity = similarity;
    Ok(())
}

fn set_pipeline_version(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    if value != "v1" && value != "v2" {
        return Err(crate::error::RecallError::Config(
            "graph.pipeline must be \"v1\" or \"v2\"".into(),
        ));
    }
    cfg.graph_section().pipeline = Some(value.to_string());
    Ok(())
}

fn set_max_candidates(cfg: &mut Config, value: &str) -> Result<(), crate::error::RecallError> {
    let count: usize = parse_number(value)?;
    if count == 0 {
        return Err(crate::error::RecallError::Config(
            "max_candidates must be at least 1".into(),
        ));
    }
    cfg.graph_section().dedup.max_candidates = count;
    Ok(())
}

type GraphBoolField = fn(&mut GraphSection) -> &mut bool;

const GRAPH_BOOLS: &[(&str, GraphBoolField)] = &[
    ("graph.known_facts_context", flag_known_facts_context),
    ("graph.known_key_binding", flag_known_key_binding),
    ("graph.known_match", flag_known_match),
    ("graph.consolidate", flag_consolidate),
    ("graph.consolidate_hold", flag_consolidate_hold),
    ("graph.consolidate_links", flag_consolidate_links),
    ("graph.consolidate_components", flag_consolidate_components),
    ("graph.consolidate_key_rename", flag_consolidate_key_rename),
    (
        "graph.consolidate_replaces_cue",
        flag_consolidate_replaces_cue,
    ),
    (
        "graph.consolidate_replaces_departure",
        flag_consolidate_replaces_departure,
    ),
    (
        "graph.consolidate_orphan_sweep",
        flag_consolidate_orphan_sweep,
    ),
    (
        "graph.consolidate_rule_fragments",
        flag_consolidate_rule_fragments,
    ),
    (
        "graph.consolidate_retire_component",
        flag_consolidate_retire_component,
    ),
    ("graph.drop_restatements", flag_drop_restatements),
    ("graph.ground_component_edges", flag_ground_component_edges),
    ("graph.recover_turns", flag_recover_turns),
    ("graph.lasting_forget", flag_lasting_forget),
    ("graph.extract_coverage", flag_extract_coverage),
];

fn graph_bool_field(key: &str) -> Option<GraphBoolField> {
    GRAPH_BOOLS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, field)| *field)
}

fn assign_graph_bool(
    cfg: &mut Config,
    value: &str,
    field: GraphBoolField,
) -> Result<(), crate::error::RecallError> {
    let flag = parse_bool(value)?;
    *field(cfg.graph_section()) = flag;
    Ok(())
}

fn flag_known_facts_context(graph: &mut GraphSection) -> &mut bool {
    &mut graph.known_facts_context
}

fn flag_known_key_binding(graph: &mut GraphSection) -> &mut bool {
    &mut graph.known_key_binding
}

fn flag_known_match(graph: &mut GraphSection) -> &mut bool {
    &mut graph.known_match
}

fn flag_consolidate(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate
}

fn flag_consolidate_hold(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_hold
}

fn flag_consolidate_links(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_links
}

fn flag_consolidate_components(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_components
}

fn flag_consolidate_key_rename(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_key_rename
}

fn flag_consolidate_replaces_cue(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_replaces_cue
}

fn flag_consolidate_replaces_departure(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_replaces_departure
}

fn flag_consolidate_orphan_sweep(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_orphan_sweep
}

fn flag_consolidate_rule_fragments(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_rule_fragments
}

fn flag_consolidate_retire_component(graph: &mut GraphSection) -> &mut bool {
    &mut graph.consolidate_retire_component
}

fn flag_drop_restatements(graph: &mut GraphSection) -> &mut bool {
    &mut graph.drop_restatements
}

fn flag_ground_component_edges(graph: &mut GraphSection) -> &mut bool {
    &mut graph.ground_component_edges
}

fn flag_recover_turns(graph: &mut GraphSection) -> &mut bool {
    &mut graph.recover_turns
}

fn flag_lasting_forget(graph: &mut GraphSection) -> &mut bool {
    &mut graph.lasting_forget
}

fn flag_extract_coverage(graph: &mut GraphSection) -> &mut bool {
    &mut graph.extract_coverage
}

fn parse_bool(value: &str) -> Result<bool, crate::error::RecallError> {
    value
        .parse()
        .map_err(|_| crate::error::RecallError::Config(format!("invalid boolean: {value}")))
}

fn parse_number<T: std::str::FromStr>(value: &str) -> Result<T, crate::error::RecallError> {
    value
        .parse()
        .map_err(|_| crate::error::RecallError::Config(format!("invalid number: {value}")))
}

/// Parse a comma-separated CLI list. Empty means "auto-detect".
fn parse_sources(
    value: &str,
) -> Result<Option<Vec<crate::transcript::Source>>, crate::error::RecallError> {
    let names: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return Ok(None);
    }
    let mut sources = Vec::with_capacity(names.len());
    for name in names {
        let source = crate::transcript::Source::from_str_loose(name)?;
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    Ok(Some(sources))
}

/// Parse an evidence weight: a finite, non-negative number.
///
/// Zero is allowed — it is how a class is switched off entirely.
fn parse_weight(value: &str) -> Result<f64, crate::error::RecallError> {
    use crate::error::RecallError;
    let weight: f64 = parse_number(value)?;
    if !weight.is_finite() || weight < 0.0 {
        return Err(RecallError::Config(format!(
            "evidence weight must be finite and non-negative, got {value}"
        )));
    }
    Ok(weight)
}

/// Parse a cosine-similarity threshold: a finite number in `0.0..=1.0`.
fn parse_similarity(value: &str) -> Result<f64, crate::error::RecallError> {
    use crate::error::RecallError;
    let similarity: f64 = parse_number(value)?;
    if !similarity.is_finite() || !(0.0..=1.0).contains(&similarity) {
        return Err(RecallError::Config(format!(
            "similarity threshold must be between 0.0 and 1.0, got {value}"
        )));
    }
    Ok(similarity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.ephemeral.max_entries, 5);
        assert_eq!(cfg.llm.provider, Provider::Grok);
        assert!(cfg.llm.model.is_empty());
    }

    #[test]
    fn parse_ephemeral_only() {
        let cfg: Config = toml::from_str("[ephemeral]\nmax_entries = 10\n").unwrap();
        assert_eq!(cfg.ephemeral.max_entries, 10);
        assert_eq!(cfg.llm.provider, Provider::Grok);
    }

    #[test]
    fn graph_mode_defaults_to_embedded() {
        let cfg: Config = toml::from_str("[graph]\n").unwrap();
        assert_eq!(cfg.graph.unwrap().mode, "embedded");
    }

    #[test]
    fn graph_mode_parses_server() {
        let cfg: Config =
            toml::from_str("[graph]\nmode = \"server\"\nurl = \"ws://db.local:8787\"\n").unwrap();
        let g = cfg.graph.unwrap();
        assert_eq!(g.mode, "server");
        assert_eq!(g.url, "ws://db.local:8787");
    }

    #[test]
    fn serve_defaults_when_section_absent() {
        let cfg: Config = toml::from_str("[ephemeral]\nmax_entries = 3\n").unwrap();
        assert_eq!(cfg.serve.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT_SECS);
        assert!(cfg.serve.socket_path.is_none());
    }

    #[test]
    fn serve_section_parses_overrides() {
        let cfg: Config = toml::from_str(
            "[serve]\nsocket_path = \"/run/re/graph.sock\"\nidle_timeout_secs = 60\n",
        )
        .unwrap();
        assert_eq!(cfg.serve.idle_timeout_secs, 60);
        assert_eq!(cfg.serve.socket_path.as_deref(), Some("/run/re/graph.sock"));
    }

    #[test]
    fn set_key_serve_idle_timeout() {
        let mut cfg = Config::default();
        cfg.set_key("serve.idle_timeout_secs", "120").unwrap();
        assert_eq!(cfg.serve.idle_timeout_secs, 120);
        assert!(cfg.set_key("serve.idle_timeout_secs", "soon").is_err());
    }

    #[test]
    fn parse_llm_section() {
        let cfg: Config = toml::from_str(
            "[llm]\nprovider = \"openai\"\nmodel = \"mistral-7b\"\napi_base = \"https://example.test/v1\"\n",
        )
        .unwrap();
        assert_eq!(cfg.llm.provider, Provider::Openai);
        assert_eq!(cfg.llm.model, "mistral-7b");
        assert_eq!(cfg.llm.api_base, "https://example.test/v1");
    }

    #[test]
    fn parse_claude_code_provider() {
        let cfg: Config = toml::from_str("[llm]\nprovider = \"claude-code\"\n").unwrap();
        assert_eq!(cfg.llm.provider, Provider::ClaudeCode);
    }

    #[test]
    fn resolved_defaults() {
        let llm = LlmSection::default();
        assert_eq!(llm.provider, Provider::Grok);
        assert_eq!(llm.resolved_model(), "");
        assert_eq!(llm.resolved_api_base(), "");

        let anthropic = LlmSection {
            provider: Provider::Anthropic,
            ..LlmSection::default()
        };
        assert_eq!(anthropic.resolved_model(), "claude-haiku-4-5-20251001");
        assert_eq!(
            anthropic.resolved_api_base(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn resolved_custom_overrides_default() {
        let llm = LlmSection {
            provider: Provider::Openai,
            model: "mistral-7b".into(),
            ..LlmSection::default()
        };
        assert_eq!(llm.resolved_model(), "mistral-7b");
        assert_eq!(llm.resolved_api_base(), "");
    }

    #[test]
    fn round_trip_toml() {
        let cfg = Config {
            ephemeral: EphemeralConfig { max_entries: 3 },
            llm: LlmSection {
                provider: Provider::Openai,
                model: "mistral-7b".into(),
                api_base: "https://example.test/v1".into(),
                ..LlmSection::default()
            },
            ..Config::default()
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&s).unwrap();
        assert_eq!(parsed.ephemeral.max_entries, 3);
        assert_eq!(parsed.llm.provider, Provider::Openai);
        assert_eq!(parsed.llm.model, "mistral-7b");
        assert_eq!(parsed.llm.api_base, "https://example.test/v1");
    }

    #[test]
    fn set_key_provider() {
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "openai").unwrap();
        assert_eq!(cfg.llm.provider, Provider::Openai);
        assert!(cfg.llm.model.is_empty());
    }

    #[test]
    fn set_key_model() {
        let mut cfg = Config::default();
        cfg.set_key("llm.model", "claude-sonnet-4-6").unwrap();
        assert_eq!(cfg.llm.model, "claude-sonnet-4-6");
    }

    #[test]
    fn a_removed_llm_key_fails_and_load_drops_the_file() {
        for key in dropped_llm_keys() {
            let toml = format!("[llm]\nprovider = \"grok\"\nmodel = \"kept\"\n{key} = 1\n");
            let message = dropped_key_error(&format!("llm.{key}")).to_string();
            assert!(
                message.contains("removed in this version; delete the key"),
                "{message}"
            );
            assert_eq!(dropped_llm_key(&toml), Some(key));
            let mut cfg = Config::default();
            let err = cfg.set_key(key, "1").unwrap_err().to_string();
            assert!(err.contains("removed in this version"), "{err}");
            let tmp = tempfile::tempdir().unwrap();
            fs::write(config_path(tmp.path()), &toml).unwrap();
            assert!(load(tmp.path()).llm.model.is_empty());
            assert_eq!(load(tmp.path()).llm.provider, Provider::Grok);
        }
    }

    #[test]
    fn the_retired_local_provider_alias_points_at_the_cli_providers() {
        let alias = retired_local_provider_alias();
        let message = Provider::from_str_loose(alias).unwrap_err().to_string();
        assert!(message.contains("removed in this version"), "{message}");
        assert!(
            message.contains("claude-code") && message.contains("'cli'"),
            "{message}"
        );
        let parsed = toml::from_str::<Config>(&format!("[llm]\nprovider = \"{alias}\"\n"));
        assert!(parsed.unwrap_err().to_string().contains("claude-code"));
        let mut cfg = Config::default();
        assert!(cfg.set_key("provider", alias).is_err());
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            config_path(tmp.path()),
            format!("[llm]\nprovider = \"{alias}\"\nmodel = \"kept\"\n"),
        )
        .unwrap();
        assert_eq!(load(tmp.path()).llm.provider, Provider::Grok);
        assert!(load(tmp.path()).llm.model.is_empty());
    }

    #[test]
    fn a_legacy_llm_section_is_refused_before_a_provider_is_built() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(legacy_llm_refusal(tmp.path()).is_none());
        for body in [
            format!(
                "[llm]\nprovider = \"grok\"\n{} = 1\n",
                dropped_llm_keys()[0]
            ),
            format!(
                "[llm]\nprovider = \"grok\"\n{} = \"x\"\n",
                dropped_llm_keys()[1]
            ),
            format!("[llm]\nprovider = \"{}\"\n", retired_local_provider_alias()),
        ] {
            fs::write(config_path(tmp.path()), &body).unwrap();
            let err = legacy_llm_refusal(tmp.path()).expect("refused");
            assert!(err.to_string().contains("removed in this version"), "{err}");
        }
        fs::write(config_path(tmp.path()), "[llm]\nprovider = \"grok\"\n").unwrap();
        assert!(legacy_llm_refusal(tmp.path()).is_none());
    }

    fn write_config(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = config_path(dir);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn try_load_without_a_file_is_the_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = try_load(tmp.path()).unwrap();
        assert_eq!(format!("{loaded:?}"), format!("{:?}", Config::default()));
        assert_eq!(format!("{loaded:?}"), format!("{:?}", load(tmp.path())));
    }

    #[test]
    fn try_load_of_a_valid_file_equals_load() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            "[ephemeral]\nmax_entries = 9\n\n[llm]\nprovider = \"grok\"\nmodel = \"alpha\"\n",
        );
        let loaded = try_load(tmp.path()).unwrap();
        assert_eq!(format!("{loaded:?}"), format!("{:?}", load(tmp.path())));
        assert_eq!(loaded.ephemeral.max_entries, 9);
        assert_eq!(loaded.llm.model, "alpha");
    }

    #[test]
    fn try_load_of_a_malformed_file_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "this is not toml [[[\n");
        let err = try_load(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("TOML parse error"), "{err}");
    }

    #[test]
    fn try_load_of_a_type_error_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "[ephemeral]\nmax_entries = \"alpha\"\n");
        let err = try_load(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(
            err.contains("max_entries") || err.contains("invalid type"),
            "{err}"
        );
    }

    #[test]
    fn try_load_of_a_removed_key_names_the_key_and_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let key = dropped_llm_keys()[0];
        let path = write_config(
            tmp.path(),
            &format!("[llm]\nprovider = \"grok\"\n{key} = 1\n"),
        );
        let expected = format!(
            "{}: llm.{key} was removed in this version; delete the key",
            path.display()
        );
        let err = try_load(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(&expected), "{err}");
        let refusal = legacy_llm_refusal(tmp.path()).unwrap().to_string();
        assert!(refusal.contains(&expected), "{refusal}");
    }

    #[test]
    fn try_load_of_the_retired_alias_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let alias = retired_local_provider_alias();
        let path = write_config(tmp.path(), &format!("[llm]\nprovider = \"{alias}\"\n"));
        let err = try_load(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(
            err.contains("that provider was removed in this version"),
            "{err}"
        );
        assert!(err.contains("claude-code"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn try_load_of_an_unreadable_file_names_the_file() {
        // SAFETY: geteuid is a pure query of the process credentials.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "[ephemeral]\nmax_entries = 5\n");
        let unreadable = std::fs::Permissions::from_mode(0o000);
        fs::set_permissions(&path, unreadable).unwrap();
        let err = try_load(tmp.path()).unwrap_err();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("cannot read"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn try_load_of_a_dangling_symlink_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = config_path(tmp.path());
        std::os::unix::fs::symlink("missing-alpha", &path).unwrap();
        let err = try_load(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("dangling symbolic link"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_reported_not_fatal() {
        let body = "[ephemeral]\nmax_entries = 7\n\n[llm]\nnote = \"alpha\"\n";
        assert_eq!(unknown_keys(body), vec!["llm.note".to_string()]);
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), body);
        let loaded = try_load(tmp.path()).unwrap();
        assert_eq!(loaded.ephemeral.max_entries, 7);
        let line = format!(
            "recall-echo: {}: unknown key llm.note ignored",
            path.display()
        );
        assert_eq!(unknown_key_lines(&path, &unknown_keys(body)), vec![line]);
        assert!(!warn_unknown_keys(&path, &["llm.note".to_string()]));
    }

    #[test]
    fn unknown_keys_under_option_sections_have_no_question_mark() {
        assert_eq!(
            unknown_keys("[graph]\nconsolidate_retire_compnent = false\n"),
            vec!["graph.consolidate_retire_compnent".to_string()]
        );
        assert_eq!(
            unknown_keys("[pipeline]\ndocs_dri = \"alpha\"\n"),
            vec!["pipeline.docs_dri".to_string()]
        );
        assert_eq!(
            unknown_keys("[graph.scoring]\nweight_semantc = 0.1\n"),
            vec!["graph.scoring.weight_semantc".to_string()]
        );
    }

    fn unknown_key_lines(path: &Path, keys: &[String]) -> Vec<String> {
        keys.iter()
            .map(|key| format!("recall-echo: {}: unknown key {key} ignored", path.display()))
            .collect()
    }

    /// Every section a default config omits on write is set here, including
    /// the bools that skip-serialize at their default. A known key reported
    /// as unknown would show up.
    #[test]
    fn a_known_key_file_reports_no_unknown_keys() {
        let body = "\
[ephemeral]
max_entries = 5

[llm]
provider = \"grok\"
model = \"\"
api_base = \"\"

[llm.cli]
preset = \"grok\"
command = \"alpha\"
args = [\"alpha\"]
prompt_delivery = \"stdin\"
prompt_flag = \"alpha\"
model_flag = \"alpha\"
output_format_flag = \"alpha\"
output_format_value = \"alpha\"
output_mode = \"raw\"
ndjson_match = [\"type=alpha\"]
system_prompt_flag = \"alpha\"
result_json_path = [\"alpha\"]
usage_input_path = [\"alpha\"]
usage_output_path = [\"alpha\"]
extra_args = [\"alpha\"]
timeout_secs = 1

[pipeline]
docs_dir = \"alpha\"
auto_sync = false

[graph]
mode = \"embedded\"
url = \"ws://localhost:8787\"
namespace = \"nullarc\"
database = \"\"
username = \"\"
password_file = \"\"
pipeline = \"v1\"
known_facts_context = true
known_key_binding = true
known_match = false
consolidate = true
consolidate_hold = true
consolidate_links = true
consolidate_components = true
consolidate_key_rename = true
consolidate_replaces_cue = true
consolidate_replaces_departure = true
consolidate_orphan_sweep = true
consolidate_rule_fragments = true
consolidate_retire_component = true
drop_restatements = true
ground_component_edges = true
recover_turns = false

[graph.scoring]
weight_semantic = 0.45
weight_hotness = 0.30
weight_utility = 0.25
corroboration_boost = 0.05

[graph.provenance]
weight_external = 1.0
weight_user = 0.8
weight_assistant = 0.3
weight_self = 0.05

[graph.dedup]
certain_similarity = 0.92
review_similarity = 0.82
max_candidates = 3

[serve]
socket_path = \"alpha.sock\"
idle_timeout_secs = 3600

[extraction]
background_enabled = true
idle_after_secs = 120
batch_size = 3

[capture]
enabled = true
sources = [\"grok\"]
settle_secs = 300
";
        assert!(
            unknown_keys(body).is_empty(),
            "known keys reported: {:?}",
            unknown_keys(body)
        );
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), body);
        let loaded = try_load(tmp.path()).unwrap();
        assert_eq!(format!("{loaded:?}"), format!("{:?}", load(tmp.path())));
    }

    #[test]
    fn set_key_unknown_fails() {
        let mut cfg = Config::default();
        assert!(cfg.set_key("nonexistent.key", "value").is_err());
    }

    #[test]
    fn set_key_cli_overrides() {
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "gemini").unwrap();
        cfg.set_key("llm.cli.command", "/opt/bin/gemini").unwrap();
        cfg.set_key("llm.cli.result_json_path", "response, result")
            .unwrap();
        cfg.set_key("llm.cli.extra_args", "--yolo --quiet").unwrap();
        cfg.set_key("llm.cli.prompt_delivery", "stdin").unwrap();
        cfg.set_key("llm.cli.timeout_secs", "45").unwrap();

        let cli = &cfg.llm.cli;
        assert_eq!(cli.command.as_deref(), Some("/opt/bin/gemini"));
        assert_eq!(
            cli.result_json_path.as_ref().unwrap().paths(),
            ["response", "result"]
        );
        assert_eq!(
            cli.extra_args.as_deref(),
            Some(["--yolo".to_string(), "--quiet".to_string()].as_slice())
        );
        assert_eq!(cli.prompt_delivery, Some(PromptDelivery::Stdin));
        assert_eq!(cli.timeout_secs, Some(45));

        assert!(cfg.set_key("llm.cli.nonexistent", "x").is_err());
        assert!(cfg.set_key("llm.cli.timeout_secs", "soon").is_err());
        assert!(cfg.set_key("llm.cli.preset", "nonesuch").is_err());
    }

    /// The overrides describe one vendor's binary; carrying them to the next
    /// provider would spawn the wrong tool with the right flags.
    #[test]
    fn switching_provider_clears_the_cli_overrides() {
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "gemini").unwrap();
        cfg.set_key("llm.cli.command", "/opt/bin/gemini").unwrap();
        cfg.set_key("llm.provider", "grok").unwrap();

        assert_eq!(cfg.llm.provider, Provider::Grok);
        assert!(cfg.llm.cli.is_empty());
    }

    #[test]
    fn cli_section_parses_from_toml() {
        let cfg: Config = toml::from_str(
            "[llm]\nprovider = \"cli\"\n\n[llm.cli]\ncommand = \"mycli\"\n\
             prompt_delivery = \"flag\"\nprompt_flag = \"--ask\"\n\
             result_json_path = [\"data.text\", \"text\"]\nargs = [\"chat\"]\n",
        )
        .expect("parse [llm.cli]");
        let cli = cfg.llm.cli;
        assert_eq!(cfg.llm.provider, Provider::Cli);
        assert_eq!(cli.command.as_deref(), Some("mycli"));
        assert_eq!(cli.prompt_delivery, Some(PromptDelivery::Flag));
        assert_eq!(cli.prompt_flag.as_deref(), Some("--ask"));
        assert_eq!(
            cli.result_json_path.as_ref().unwrap().paths(),
            ["data.text", "text"]
        );
        assert_eq!(cli.args.as_deref(), Some(["chat".to_string()].as_slice()));
    }

    #[test]
    fn set_key_output_mode_and_ndjson_match() {
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "cli").unwrap();
        cfg.set_key("llm.cli.output_mode", "ndjson").unwrap();
        cfg.set_key(
            "llm.cli.ndjson_match",
            "type=item.completed, item.type=agent_message",
        )
        .unwrap();

        assert_eq!(cfg.llm.cli.output_mode, Some(OutputMode::Ndjson));
        assert_eq!(
            cfg.llm.cli.ndjson_match.as_ref().unwrap().predicates(),
            [("type", "item.completed"), ("item.type", "agent_message")]
        );
        assert!(cfg.set_key("llm.cli.output_mode", "yaml").is_err());
    }

    #[test]
    fn output_mode_accepts_the_obvious_spellings() {
        assert_eq!(
            OutputMode::from_str_loose("jsonl").unwrap(),
            OutputMode::Ndjson
        );
        assert_eq!(
            OutputMode::from_str_loose("json").unwrap(),
            OutputMode::SingleJson
        );
        assert_eq!(OutputMode::from_str_loose("TEXT").unwrap(), OutputMode::Raw);
    }

    /// A predicate without a value would match every line; dropping it is
    /// safer than treating it as a wildcard nobody asked for.
    #[test]
    fn line_matchers_drop_entries_without_a_value() {
        let matchers = LineMatchers::parse("type=item.completed, garbage, ");
        assert_eq!(matchers.predicates(), [("type", "item.completed")]);
    }

    #[test]
    fn result_json_path_accepts_a_bare_string() {
        let cli: CliSection = toml::from_str("result_json_path = \"result\"\n").expect("parse");
        assert_eq!(cli.result_json_path.unwrap().paths(), ["result"]);
    }

    #[test]
    fn an_empty_result_json_path_means_raw_stdout() {
        let cli: CliSection = toml::from_str("result_json_path = \"\"\n").expect("parse");
        assert!(cli.result_json_path.unwrap().is_empty());
    }

    /// Configs written before `[llm.cli]` existed must keep loading, and keep
    /// saving without gaining a section their owner never asked for.
    #[test]
    fn a_config_without_a_cli_section_round_trips_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "claude-code").unwrap();
        save(tmp.path(), &cfg).unwrap();

        let rendered = fs::read_to_string(config_path(tmp.path())).unwrap();
        assert!(!rendered.contains("[llm.cli]"), "{rendered}");
        assert_eq!(load(tmp.path()).llm.provider, Provider::ClaudeCode);
    }

    #[test]
    fn cli_overrides_survive_a_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "cli").unwrap();
        cfg.set_key("llm.cli.command", "mycli").unwrap();
        cfg.set_key("llm.cli.result_json_path", "data.text")
            .unwrap();
        save(tmp.path(), &cfg).unwrap();

        let loaded = load(tmp.path());
        assert_eq!(loaded.llm.provider, Provider::Cli);
        assert_eq!(loaded.llm.cli.command.as_deref(), Some("mycli"));
        assert_eq!(
            loaded.llm.cli.result_json_path.unwrap().paths(),
            ["data.text"]
        );
    }

    #[test]
    fn cli_providers_are_distinguished_from_http_ones() {
        assert!(Provider::ClaudeCode.is_cli());
        assert!(Provider::Gemini.is_cli());
        assert!(Provider::Grok.is_cli());
        assert!(Provider::Codex.is_cli());
        assert!(Provider::Cli.is_cli());
        assert!(!Provider::Anthropic.is_cli());
        assert!(!Provider::Openai.is_cli());
    }

    #[test]
    fn provider_from_str_loose_accepts_the_cli_vendors() {
        assert_eq!(
            Provider::from_str_loose("gemini").unwrap(),
            Provider::Gemini
        );
        assert_eq!(
            Provider::from_str_loose("gemini-cli").unwrap(),
            Provider::Gemini
        );
        assert_eq!(Provider::from_str_loose("Grok").unwrap(), Provider::Grok);
        assert_eq!(Provider::from_str_loose("xai").unwrap(), Provider::Grok);
        assert_eq!(Provider::from_str_loose("cli").unwrap(), Provider::Cli);
        assert_eq!(Provider::from_str_loose("custom").unwrap(), Provider::Cli);
    }

    #[test]
    fn codex_resolves_to_its_own_preset() {
        assert_eq!(Provider::from_str_loose("codex").unwrap(), Provider::Codex);
        assert_eq!(
            Provider::Codex.default_cli_preset(),
            Some(CliPreset::Codex),
            "codex must not fall through to the custom preset"
        );
    }

    /// A vendor with no preset at all gets the generic mechanism, not a dead
    /// end — the error names it.
    #[test]
    fn an_unknown_vendor_is_pointed_at_the_cli_provider() {
        let err = Provider::from_str_loose("some-new-agent").expect_err("no such preset");
        assert!(err.to_string().contains("[llm.cli]"), "{err}");
    }

    #[test]
    fn provider_display_round_trips_through_from_str_loose() {
        for provider in [
            Provider::Anthropic,
            Provider::Openai,
            Provider::ClaudeCode,
            Provider::Gemini,
            Provider::Grok,
            Provider::Codex,
            Provider::Cli,
        ] {
            let rendered = provider.to_string();
            assert_eq!(
                Provider::from_str_loose(&rendered).unwrap(),
                provider,
                "{rendered}"
            );
        }
    }

    #[test]
    fn provider_from_str_loose() {
        assert_eq!(
            Provider::from_str_loose("openai").unwrap(),
            Provider::Openai
        );
        assert_eq!(
            Provider::from_str_loose("claude").unwrap(),
            Provider::Anthropic
        );
        assert_eq!(
            Provider::from_str_loose("claude-code").unwrap(),
            Provider::ClaudeCode
        );
        assert!(Provider::from_str_loose("unknown").is_err());
    }

    #[test]
    fn save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            ephemeral: EphemeralConfig { max_entries: 7 },
            llm: LlmSection {
                provider: Provider::ClaudeCode,
                ..LlmSection::default()
            },
            ..Config::default()
        };
        save(tmp.path(), &cfg).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded.ephemeral.max_entries, 7);
        assert_eq!(loaded.llm.provider, Provider::ClaudeCode);
    }

    #[test]
    fn load_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load(tmp.path());
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn validate_out_of_range() {
        let cfg = validate(Config {
            ephemeral: EphemeralConfig { max_entries: 100 },
            ..Config::default()
        })
        .unwrap();
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn capture_defaults_are_on_and_auto_detecting() {
        let capture = CaptureSection::default();
        assert!(capture.enabled);
        assert!(capture.sources.is_none());
        assert_eq!(capture.settle(), std::time::Duration::from_secs(300));
    }

    /// A config written before `[capture]` existed must keep loading, with the
    /// defaults it would have had.
    #[test]
    fn a_config_without_a_capture_section_still_loads() {
        let cfg: Config = toml::from_str("[ephemeral]\nmax_entries = 3\n").unwrap();
        assert!(cfg.capture.enabled);
        assert!(cfg.capture.sources.is_none());
    }

    #[test]
    fn capture_section_parses_an_explicit_source_list() {
        let cfg: Config = toml::from_str(
            "[capture]\nenabled = false\nsources = [\"codex\", \"grok\"]\nsettle_secs = 60\n",
        )
        .expect("parse [capture]");
        assert!(!cfg.capture.enabled);
        assert_eq!(cfg.capture.settle_secs, 60);
        assert_eq!(
            cfg.capture.sources.as_deref(),
            Some(
                [
                    crate::transcript::Source::Codex,
                    crate::transcript::Source::Grok
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn set_key_capture_values() {
        let mut cfg = Config::default();
        cfg.set_key("capture.enabled", "false").unwrap();
        cfg.set_key("capture.settle_secs", "30").unwrap();
        cfg.set_key("capture.sources", "codex, claude").unwrap();

        assert!(!cfg.capture.enabled);
        assert_eq!(cfg.capture.settle_secs, 30);
        assert_eq!(
            cfg.capture.sources.as_deref(),
            Some(
                [
                    crate::transcript::Source::Codex,
                    crate::transcript::Source::ClaudeCode
                ]
                .as_slice()
            )
        );

        // An empty list means "back to auto-detect", not "capture nothing".
        cfg.set_key("capture.sources", "").unwrap();
        assert!(cfg.capture.sources.is_none());
        assert!(cfg.set_key("capture.sources", "cursor").is_err());
        assert!(cfg.set_key("capture.enabled", "maybe").is_err());
    }

    #[test]
    fn graph_scoring_defaults_match_legacy_hardcodes() {
        let scoring = GraphScoringConfig::default();
        assert!((scoring.weight_semantic - 0.45).abs() < f64::EPSILON);
        assert!((scoring.weight_hotness - 0.30).abs() < f64::EPSILON);
        assert!((scoring.weight_utility - 0.25).abs() < f64::EPSILON);
        assert!((scoring.corroboration_boost - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_scoring_partial_toml_fills_defaults() {
        let scoring: GraphScoringConfig =
            toml::from_str("weight_utility = 0.5\n").expect("parse partial scoring");
        assert!((scoring.weight_semantic - 0.45).abs() < f64::EPSILON);
        assert!((scoring.weight_hotness - 0.30).abs() < f64::EPSILON);
        assert!((scoring.weight_utility - 0.5).abs() < f64::EPSILON);
        assert!((scoring.corroboration_boost - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_scoring_corroboration_boost_is_configurable() {
        let scoring: GraphScoringConfig =
            toml::from_str("corroboration_boost = 0.0\n").expect("parse corroboration boost");
        assert!(scoring.corroboration_boost.abs() < f64::EPSILON);
        assert!((scoring.weight_semantic - 0.45).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_scoring_empty_section_yields_defaults() {
        let section: GraphSection = toml::from_str("").expect("parse empty graph section");
        let defaults = GraphScoringConfig::default();
        assert!((section.scoring.weight_semantic - defaults.weight_semantic).abs() < f64::EPSILON);
        assert!((section.scoring.weight_hotness - defaults.weight_hotness).abs() < f64::EPSILON);
        assert!((section.scoring.weight_utility - defaults.weight_utility).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_provenance_defaults_when_section_absent() {
        let section: GraphSection = toml::from_str("mode = \"embedded\"\n").expect("parse section");
        let defaults = ProvenanceWeights::default();
        assert_eq!(section.provenance, defaults);
        assert!((defaults.weight_external - 1.0).abs() < f64::EPSILON);
        assert!((defaults.weight_user - 0.8).abs() < f64::EPSILON);
        assert!((defaults.weight_assistant - 0.3).abs() < f64::EPSILON);
        assert!((defaults.weight_self - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_provenance_partial_toml_fills_defaults() {
        let cfg: Config =
            toml::from_str("[graph]\n\n[graph.provenance]\nweight_self = 0.5\n").expect("parse");
        let provenance = cfg.graph.expect("graph section present").provenance;
        assert!((provenance.weight_self - 0.5).abs() < f64::EPSILON);
        assert!((provenance.weight_external - 1.0).abs() < f64::EPSILON);
        assert!((provenance.weight_user - 0.8).abs() < f64::EPSILON);
        assert!((provenance.weight_assistant - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn set_key_provenance_weights() {
        let mut cfg = Config::default();
        cfg.set_key("graph.provenance.weight_self", "0.2").unwrap();
        cfg.set_key("graph.provenance.weight_user", "0").unwrap();
        cfg.set_key("graph.provenance.weight_assistant", "0.4")
            .unwrap();
        cfg.set_key("graph.provenance.weight_external", "1.5")
            .unwrap();

        let provenance = cfg
            .graph
            .as_ref()
            .expect("graph section created")
            .provenance;
        assert!((provenance.weight_self - 0.2).abs() < f64::EPSILON);
        assert!(provenance.weight_user.abs() < f64::EPSILON);
        assert!((provenance.weight_assistant - 0.4).abs() < f64::EPSILON);
        assert!((provenance.weight_external - 1.5).abs() < f64::EPSILON);

        assert!(cfg.set_key("graph.provenance.weight_self", "-1").is_err());
        assert!(cfg.set_key("graph.provenance.weight_self", "lots").is_err());
    }

    #[test]
    fn weight_assistant_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.set_key("graph.provenance.weight_assistant", "0.4")
            .unwrap();
        let rendered = toml::to_string_pretty(&cfg).expect("render");
        assert!(rendered.contains("weight_assistant"));
        let parsed: Config = toml::from_str(&rendered).expect("reparse");
        let provenance = parsed.graph.expect("graph section survives").provenance;
        assert!((provenance.weight_assistant - 0.4).abs() < f64::EPSILON);
        assert!((provenance.weight_user - 0.8).abs() < f64::EPSILON);
        assert!((provenance.weight_self - 0.05).abs() < f64::EPSILON);

        let partial: Config =
            toml::from_str("[graph.provenance]\nweight_user = 0.8\n").expect("parse");
        let provenance = partial.graph.expect("graph section").provenance;
        assert!((provenance.weight_assistant - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn provenance_weights_round_trip_through_toml() {
        let mut cfg = Config::default();
        cfg.set_key("graph.provenance.weight_self", "0.05").unwrap();
        let rendered = toml::to_string_pretty(&cfg).expect("render");
        let parsed: Config = toml::from_str(&rendered).expect("reparse");
        assert_eq!(
            parsed.graph.expect("graph section survives").provenance,
            ProvenanceWeights::default()
        );
    }

    #[test]
    fn dedup_defaults_leave_a_gap_between_the_bands() {
        let dedup = GraphDedupConfig::default();
        assert!((dedup.certain_similarity - 0.92).abs() < f64::EPSILON);
        assert!((dedup.review_similarity - 0.82).abs() < f64::EPSILON);
        assert_eq!(dedup.max_candidates, 3);
        assert!(dedup.review_similarity < dedup.certain_similarity);
    }

    #[test]
    fn dedup_bands_are_cut_at_the_thresholds() {
        let dedup = GraphDedupConfig::default();
        assert_eq!(dedup.band(0.99), DedupBand::SameEntity);
        assert_eq!(dedup.band(0.92), DedupBand::SameEntity);
        assert_eq!(dedup.band(0.9), DedupBand::Ambiguous);
        assert_eq!(dedup.band(0.82), DedupBand::Ambiguous);
        assert_eq!(dedup.band(0.8), DedupBand::NewEntity);
        assert_eq!(dedup.band(0.0), DedupBand::NewEntity);
    }

    /// A store configured to fetch nothing would resolve every candidate as
    /// new; the floor of one keeps dedup able to see.
    #[test]
    fn dedup_candidate_limit_never_falls_below_one() {
        let dedup = GraphDedupConfig {
            max_candidates: 0,
            ..GraphDedupConfig::default()
        };
        assert_eq!(dedup.candidate_limit(), 1);
    }

    #[test]
    fn dedup_partial_toml_fills_defaults() {
        let cfg: Config = toml::from_str("[graph]\n\n[graph.dedup]\ncertain_similarity = 0.95\n")
            .expect("parse dedup section");
        let dedup = cfg.graph.expect("graph section present").dedup;
        assert!((dedup.certain_similarity - 0.95).abs() < f64::EPSILON);
        assert!((dedup.review_similarity - 0.82).abs() < f64::EPSILON);
        assert_eq!(dedup.max_candidates, 3);
    }

    #[test]
    fn set_key_dedup_thresholds() {
        let mut cfg = Config::default();
        cfg.set_key("graph.dedup.certain_similarity", "0.9")
            .unwrap();
        cfg.set_key("graph.dedup.review_similarity", "0.6").unwrap();
        cfg.set_key("graph.dedup.max_candidates", "5").unwrap();

        let dedup = &cfg.graph.as_ref().expect("graph section created").dedup;
        assert!((dedup.certain_similarity - 0.9).abs() < f64::EPSILON);
        assert!((dedup.review_similarity - 0.6).abs() < f64::EPSILON);
        assert_eq!(dedup.max_candidates, 5);

        assert!(cfg
            .set_key("graph.dedup.certain_similarity", "1.5")
            .is_err());
        assert!(cfg
            .set_key("graph.dedup.review_similarity", "-0.1")
            .is_err());
        assert!(cfg.set_key("graph.dedup.max_candidates", "0").is_err());
    }

    /// The measured consolidation stack is on when the key is missing, and an
    /// explicit false survives a serialize-and-parse. A true value is left out
    /// of the render, so a rewrite does not pin today's default. `Default`
    /// matches that empty decode. `known_match` and `recover_turns` stay off.
    #[test]
    fn measured_stack_defaults_on_and_false_round_trips() {
        let from_default = GraphSection::default();
        let from_empty: GraphSection = toml::from_str("").expect("empty section");
        let absent: Config = toml::from_str("").expect("empty config");
        let absent_graph = absent.graph.unwrap_or_default();
        let rendered_default = toml::to_string(&from_default).expect("render default");
        assert_eq!(
            rendered_default,
            toml::to_string(&from_empty).expect("render empty"),
        );

        let keys = [
            "consolidate",
            "consolidate_hold",
            "drop_restatements",
            "consolidate_links",
            "consolidate_components",
            "ground_component_edges",
            "consolidate_key_rename",
            "consolidate_replaces_cue",
            "known_facts_context",
            "consolidate_replaces_departure",
            "known_key_binding",
            "consolidate_orphan_sweep",
            "consolidate_rule_fragments",
            "consolidate_retire_component",
        ];
        for key in keys {
            assert!(
                !rendered_default.contains(&format!("{key} =")),
                "{key} pinned in {rendered_default}"
            );
            assert!(stack_flag(&from_default, key), "{key} default");
            assert!(stack_flag(&from_empty, key), "{key} empty");
            assert!(stack_flag(&absent_graph, key), "{key} absent config");
            let body = format!("[graph]\n{key} = false\n");
            let parsed: Config = toml::from_str(&body).unwrap_or_else(|err| panic!("{key}: {err}"));
            let graph = parsed.graph.expect("graph");
            assert!(!stack_flag(&graph, key), "{key} false");
            let rendered = toml::to_string(&graph).expect("render false");
            let line = format!("{key} = false");
            assert!(rendered.contains(&line), "{key} missing from {rendered}");
            for other in keys {
                if other == key {
                    continue;
                }
                assert!(
                    !rendered.contains(&format!("{other} =")),
                    "{other} pinned beside {key}: {rendered}"
                );
            }
            let again: GraphSection = toml::from_str(&rendered).expect("reparse false");
            assert!(!stack_flag(&again, key), "{key} round trip");
            for other in keys {
                if other != key {
                    assert!(stack_flag(&again, other), "{other} dropped beside {key}");
                }
            }
        }

        assert!(!from_default.known_match);
        assert!(!from_default.recover_turns);
        assert!(!from_empty.known_match);
        assert!(!from_empty.recover_turns);
        let rendered = toml::to_string(&from_default).expect("render default");
        assert!(!rendered.contains("known_match"), "{rendered}");
        assert!(!rendered.contains("recover_turns"), "{rendered}");
    }

    fn stack_flag(graph: &GraphSection, key: &str) -> bool {
        match key {
            "consolidate" => graph.consolidate,
            "consolidate_hold" => graph.consolidate_hold,
            "drop_restatements" => graph.drop_restatements,
            "consolidate_links" => graph.consolidate_links,
            "consolidate_components" => graph.consolidate_components,
            "ground_component_edges" => graph.ground_component_edges,
            "consolidate_key_rename" => graph.consolidate_key_rename,
            "consolidate_replaces_cue" => graph.consolidate_replaces_cue,
            "known_facts_context" => graph.known_facts_context,
            "consolidate_replaces_departure" => graph.consolidate_replaces_departure,
            "known_key_binding" => graph.known_key_binding,
            "consolidate_orphan_sweep" => graph.consolidate_orphan_sweep,
            "consolidate_rule_fragments" => graph.consolidate_rule_fragments,
            "consolidate_retire_component" => graph.consolidate_retire_component,
            _ => panic!("unknown stack key {key}"),
        }
    }

    #[test]
    fn known_facts_context_defaults_off() {
        assert!(default_known_facts_context());
        assert!(GraphSection::default().known_facts_context);
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().known_facts_context);
        let section: Config =
            toml::from_str("[graph]\nmode = \"embedded\"\n").expect("graph section");
        assert!(section.graph.expect("graph").known_facts_context);
        let mut cfg = Config::default();
        cfg.set_key("graph.known_facts_context", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").known_facts_context);
        assert!(cfg.set_key("graph.known_facts_context", "yes").is_err());
    }

    #[test]
    fn known_match_defaults_off() {
        assert!(!default_known_match());
        assert!(!GraphSection::default().known_match);
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(!absent.graph.unwrap_or_default().known_match);
        let mut cfg = Config::default();
        cfg.set_key("graph.known_match", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").known_match);
        assert!(cfg.set_key("graph.known_match", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").known_facts_context);
    }

    #[test]
    fn consolidate_defaults_off() {
        assert!(default_consolidate());
        assert!(GraphSection::default().consolidate);
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().consolidate);
        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").consolidate);
        assert!(cfg.set_key("graph.consolidate", "yes").is_err());
    }

    #[test]
    fn consolidate_hold_defaults_off_and_needs_consolidate() {
        assert!(default_consolidate_hold());
        assert!(GraphSection::default().consolidate_hold);
        assert!(GraphSection::default().hold_applies());
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().hold_applies());
        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate", "false").unwrap();
        cfg.set_key("graph.consolidate_hold", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").consolidate_hold);
        assert!(!cfg.graph.as_ref().expect("graph").hold_applies());
        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").hold_applies());
        assert!(cfg.set_key("graph.consolidate_hold", "yes").is_err());
    }

    #[test]
    fn consolidate_links_defaults_off_round_trips_and_needs_consolidate() {
        assert!(default_consolidate_links());
        assert!(GraphSection::default().consolidate_links);
        assert!(GraphSection::default().links_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().links_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_links ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_links = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_links);
        assert!(!false_graph.links_apply());
        assert!(false_graph.consolidate);
        assert!(false_graph.consolidate_hold);
        assert!(false_graph.drop_restatements);
        assert!(!false_graph.known_match);
        assert!(false_graph.known_facts_context);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_links = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_links", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_links);
        assert!(graph.links_apply());
        assert!(graph.consolidate);
        assert!(graph.consolidate_hold);
        assert!(graph.drop_restatements);
        assert!(!graph.known_match);
        assert!(graph.known_facts_context);
        let rendered = toml::to_string_pretty(&cfg).expect("render links only");
        assert!(!rendered.contains("consolidate_links ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("consolidate_hold ="));
        assert!(!rendered.contains("drop_restatements ="));
        assert!(!rendered.contains("known_match"));
        assert!(!rendered.contains("known_facts_context ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse links only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.consolidate_links);
        assert!(parsed_graph.links_apply());
        assert!(parsed_graph.consolidate);
        assert!(parsed_graph.consolidate_hold);
        assert!(parsed_graph.drop_restatements);
        assert!(!parsed_graph.known_match);
        assert!(parsed_graph.known_facts_context);

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").links_apply());
        cfg.set_key("graph.consolidate_hold", "true").unwrap();
        cfg.set_key("graph.drop_restatements", "true").unwrap();
        cfg.set_key("graph.known_match", "true").unwrap();
        cfg.set_key("graph.known_facts_context", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.links_apply());
        assert!(graph.hold_applies());
        assert!(graph.drop_restatements);
        assert!(graph.known_match);
        assert!(graph.known_facts_context);

        cfg.set_key("graph.consolidate_links", "false").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.consolidate_links);
        assert!(!graph.links_apply());
        assert!(graph.consolidate);
        assert!(graph.hold_applies());
        assert!(graph.drop_restatements);
        assert!(graph.known_match);
        assert!(graph.known_facts_context);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_links = false"));
        let parsed: Config = toml::from_str(&rendered).expect("reparse cleared");
        let cleared = parsed.graph.unwrap_or_default();
        assert!(!cleared.consolidate_links);
        assert!(!cleared.links_apply());

        cfg.set_key("graph.consolidate_links", "true").unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").links_apply());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_links);
        assert!(cfg.set_key("graph.consolidate_links", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_links);
    }

    #[test]
    fn consolidate_components_defaults_off_round_trips_and_needs_consolidate() {
        assert!(default_consolidate_components());
        assert!(GraphSection::default().consolidate_components);
        assert!(GraphSection::default().components_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().components_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_components ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_components = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_components);
        assert!(!false_graph.components_apply());
        assert!(false_graph.consolidate);
        assert!(false_graph.consolidate_hold);
        assert!(false_graph.consolidate_links);
        assert!(false_graph.drop_restatements);
        assert!(!false_graph.known_match);
        assert!(false_graph.known_facts_context);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_components = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_components", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_components);
        assert!(graph.components_apply());
        assert!(graph.consolidate);
        assert!(graph.consolidate_hold);
        assert!(graph.consolidate_links);
        assert!(graph.drop_restatements);
        assert!(!graph.known_match);
        assert!(graph.known_facts_context);
        let rendered = toml::to_string_pretty(&cfg).expect("render components only");
        assert!(!rendered.contains("consolidate_components ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("consolidate_hold ="));
        assert!(!rendered.contains("consolidate_links ="));
        assert!(!rendered.contains("drop_restatements ="));
        assert!(!rendered.contains("known_match"));
        assert!(!rendered.contains("known_facts_context ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse components only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.consolidate_components);
        assert!(parsed_graph.components_apply());
        assert!(parsed_graph.consolidate);
        assert!(parsed_graph.consolidate_hold);
        assert!(parsed_graph.consolidate_links);
        assert!(parsed_graph.drop_restatements);
        assert!(!parsed_graph.known_match);
        assert!(parsed_graph.known_facts_context);

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").components_apply());
        cfg.set_key("graph.consolidate_hold", "true").unwrap();
        cfg.set_key("graph.consolidate_links", "true").unwrap();
        cfg.set_key("graph.drop_restatements", "true").unwrap();
        cfg.set_key("graph.known_match", "true").unwrap();
        cfg.set_key("graph.known_facts_context", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.components_apply());
        assert!(graph.links_apply());
        assert!(graph.hold_applies());
        assert!(graph.drop_restatements);
        assert!(graph.known_match);
        assert!(graph.known_facts_context);

        cfg.set_key("graph.consolidate_components", "false")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.consolidate_components);
        assert!(!graph.components_apply());
        assert!(graph.consolidate);
        assert!(graph.links_apply());
        assert!(graph.hold_applies());
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_components = false"));
        let parsed: Config = toml::from_str(&rendered).expect("reparse cleared");
        let cleared = parsed.graph.unwrap_or_default();
        assert!(!cleared.consolidate_components);
        assert!(!cleared.components_apply());

        cfg.set_key("graph.consolidate_components", "true").unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").components_apply());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_components);
        assert!(cfg.set_key("graph.consolidate_components", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_components);
    }

    #[test]
    fn consolidate_key_rename_defaults_off_round_trips_and_needs_consolidate() {
        assert!(default_consolidate_key_rename());
        assert!(GraphSection::default().consolidate_key_rename);
        assert!(GraphSection::default().rename_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().rename_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_key_rename ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_key_rename = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_key_rename);
        assert!(!false_graph.rename_apply());
        assert!(false_graph.consolidate);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_key_rename = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_key_rename", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_key_rename);
        assert!(graph.rename_apply());
        assert!(graph.consolidate);
        assert!(graph.consolidate_components);
        assert!(graph.consolidate_hold);
        assert!(graph.consolidate_links);
        let rendered = toml::to_string_pretty(&cfg).expect("render rename only");
        assert!(!rendered.contains("consolidate_key_rename ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("consolidate_components ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse rename only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.consolidate_key_rename);
        assert!(parsed_graph.rename_apply());

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").rename_apply());
        cfg.set_key("graph.consolidate_key_rename", "false")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.consolidate_key_rename);
        assert!(!graph.rename_apply());
        assert!(graph.consolidate);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_key_rename = false"));
        cfg.set_key("graph.consolidate_key_rename", "true").unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").rename_apply());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_key_rename);
        assert!(cfg.set_key("graph.consolidate_key_rename", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_key_rename);
    }

    #[test]
    fn consolidate_replaces_cue_defaults_off_round_trips_and_needs_consolidate() {
        assert!(default_consolidate_replaces_cue());
        assert!(GraphSection::default().consolidate_replaces_cue);
        assert!(GraphSection::default().replaces_cue_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().replaces_cue_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_replaces_cue ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_replaces_cue = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_replaces_cue);
        assert!(!false_graph.replaces_cue_apply());
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_replaces_cue = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_replaces_cue", "true")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_replaces_cue);
        assert!(graph.replaces_cue_apply());
        assert!(graph.consolidate);
        assert!(graph.rename_apply());
        assert!(graph.components_apply());
        let rendered = toml::to_string_pretty(&cfg).expect("render cue only");
        assert!(!rendered.contains("consolidate_replaces_cue ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("consolidate_key_rename ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse cue only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.consolidate_replaces_cue);
        assert!(parsed_graph.replaces_cue_apply());

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").replaces_cue_apply());
        cfg.set_key("graph.consolidate_replaces_cue", "false")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.consolidate_replaces_cue);
        assert!(!graph.replaces_cue_apply());
        assert!(graph.consolidate);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_replaces_cue = false"));
        cfg.set_key("graph.consolidate_replaces_cue", "true")
            .unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").replaces_cue_apply());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_replaces_cue);
        assert!(cfg
            .set_key("graph.consolidate_replaces_cue", "yes")
            .is_err());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_replaces_cue);
    }

    #[test]
    fn consolidate_replaces_departure_defaults_off_round_trips_and_needs_consolidate() {
        assert!(default_consolidate_replaces_departure());
        assert!(GraphSection::default().consolidate_replaces_departure);
        assert!(GraphSection::default().replaces_departure_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().replaces_departure_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_replaces_departure ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_replaces_departure = false\n")
                .expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_replaces_departure);
        assert!(!false_graph.replaces_departure_apply());
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_replaces_departure = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_replaces_departure", "true")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_replaces_departure);
        assert!(graph.replaces_departure_apply());
        assert!(graph.consolidate);
        assert!(graph.replaces_cue_apply());
        assert!(graph.consolidate_replaces_cue);
        let rendered = toml::to_string_pretty(&cfg).expect("render departure only");
        assert!(!rendered.contains("consolidate_replaces_departure ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("consolidate_replaces_cue ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse departure only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.consolidate_replaces_departure);
        assert!(parsed_graph.replaces_departure_apply());

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg
            .graph
            .as_ref()
            .expect("graph")
            .replaces_departure_apply());
        cfg.set_key("graph.consolidate_replaces_departure", "false")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.consolidate_replaces_departure);
        assert!(!graph.replaces_departure_apply());
        assert!(graph.consolidate);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_replaces_departure = false"));
        cfg.set_key("graph.consolidate_replaces_departure", "true")
            .unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg
            .graph
            .as_ref()
            .expect("graph")
            .replaces_departure_apply());
        assert!(
            cfg.graph
                .as_ref()
                .expect("graph")
                .consolidate_replaces_departure
        );
        assert!(cfg
            .set_key("graph.consolidate_replaces_departure", "yes")
            .is_err());
        assert!(
            cfg.graph
                .as_ref()
                .expect("graph")
                .consolidate_replaces_departure
        );
    }

    #[test]
    fn consolidate_orphan_sweep_defaults_off_round_trips_and_needs_grounding() {
        assert!(default_consolidate_orphan_sweep());
        assert!(GraphSection::default().consolidate_orphan_sweep);
        assert!(GraphSection::default().orphan_sweep_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().orphan_sweep_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_orphan_sweep ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_orphan_sweep = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_orphan_sweep);
        assert!(!false_graph.orphan_sweep_apply());
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_orphan_sweep = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_orphan_sweep", "true")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_orphan_sweep);
        assert!(graph.orphan_sweep_apply());
        assert!(graph.consolidate);
        assert!(graph.ground_component_edges);
        let rendered = toml::to_string_pretty(&cfg).expect("render sweep only");
        assert!(!rendered.contains("consolidate_orphan_sweep ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("ground_component_edges ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse sweep only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.consolidate_orphan_sweep);
        assert!(parsed_graph.orphan_sweep_apply());

        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").orphan_sweep_apply());
        cfg.set_key("graph.consolidate", "true").unwrap();
        cfg.set_key("graph.ground_component_edges", "false")
            .unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").orphan_sweep_apply());
        cfg.set_key("graph.ground_component_edges", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").orphan_sweep_apply());
        cfg.set_key("graph.consolidate_orphan_sweep", "false")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.consolidate_orphan_sweep);
        assert!(!graph.orphan_sweep_apply());
        assert!(graph.consolidate);
        assert!(graph.ground_component_edges);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_orphan_sweep = false"));
        cfg.set_key("graph.consolidate_orphan_sweep", "true")
            .unwrap();
        cfg.set_key("graph.ground_component_edges", "false")
            .unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").orphan_sweep_apply());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_orphan_sweep);
        cfg.set_key("graph.consolidate", "false").unwrap();
        cfg.set_key("graph.ground_component_edges", "true").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").orphan_sweep_apply());
        assert!(cfg
            .set_key("graph.consolidate_orphan_sweep", "yes")
            .is_err());
        assert!(cfg.graph.as_ref().expect("graph").consolidate_orphan_sweep);
    }

    #[test]
    fn consolidate_rule_fragments_defaults_off_round_trips_and_needs_consolidate() {
        assert!(default_consolidate_rule_fragments());
        assert!(GraphSection::default().consolidate_rule_fragments);
        assert!(GraphSection::default().rule_fragments_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().rule_fragments_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_rule_fragments ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_rule_fragments = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_rule_fragments);
        assert!(!false_graph.rule_fragments_apply());
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_rule_fragments = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_rule_fragments", "true")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_rule_fragments);
        assert!(graph.rule_fragments_apply());
        assert!(graph.consolidate);
        assert!(graph.ground_component_edges);
        let rendered = toml::to_string_pretty(&cfg).expect("render fragments only");
        assert!(!rendered.contains("consolidate_rule_fragments ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("ground_component_edges ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse fragments only");
        assert!(parsed.graph.expect("graph").consolidate_rule_fragments);

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").rule_fragments_apply());
        cfg.set_key("graph.consolidate_rule_fragments", "false")
            .unwrap();
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_rule_fragments = false"));
        cfg.set_key("graph.consolidate_rule_fragments", "true")
            .unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").rule_fragments_apply());
        assert!(cfg
            .set_key("graph.consolidate_rule_fragments", "yes")
            .is_err());
        assert!(
            cfg.graph
                .as_ref()
                .expect("graph")
                .consolidate_rule_fragments
        );
    }

    #[test]
    fn consolidate_retire_component_defaults_off_and_needs_only_consolidate() {
        assert!(default_consolidate_retire_component());
        assert!(GraphSection::default().consolidate_retire_component);
        assert!(GraphSection::default().retire_component_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().retire_component_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("consolidate_retire_component ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nconsolidate_retire_component = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.consolidate_retire_component);
        assert!(!false_graph.retire_component_apply());
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("consolidate_retire_component = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.consolidate_retire_component", "true")
            .unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.consolidate_retire_component);
        assert!(graph.retire_component_apply());
        assert!(graph.consolidate);
        assert!(graph.ground_component_edges);
        let rendered = toml::to_string_pretty(&cfg).expect("render retire only");
        assert!(!rendered.contains("consolidate_retire_component ="));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("ground_component_edges ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse retire only");
        assert!(parsed.graph.expect("graph").consolidate_retire_component);

        cfg.set_key("graph.consolidate", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").retire_component_apply());
        cfg.set_key("graph.ground_component_edges", "false")
            .unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").ground_component_edges);
        cfg.set_key("graph.ground_component_edges", "true").unwrap();
        cfg.set_key("graph.consolidate_retire_component", "false")
            .unwrap();
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("consolidate_retire_component = false"));
        cfg.set_key("graph.consolidate_retire_component", "true")
            .unwrap();
        cfg.set_key("graph.consolidate", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").retire_component_apply());
        assert!(cfg
            .set_key("graph.consolidate_retire_component", "yes")
            .is_err());
        assert!(
            cfg.graph
                .as_ref()
                .expect("graph")
                .consolidate_retire_component
        );
    }

    #[test]
    fn known_key_binding_defaults_off_round_trips_and_needs_the_known_block() {
        assert!(default_known_key_binding());
        assert!(GraphSection::default().known_key_binding);
        assert!(GraphSection::default().known_key_binding_apply());

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().known_key_binding_apply());
        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("known_key_binding ="));

        let explicit_false: Config =
            toml::from_str("[graph]\nknown_key_binding = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.known_key_binding);
        assert!(!false_graph.known_key_binding_apply());
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("known_key_binding = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.known_key_binding", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.known_key_binding);
        assert!(graph.known_key_binding_apply());
        assert!(graph.known_facts_context);
        let rendered = toml::to_string_pretty(&cfg).expect("render binding only");
        assert!(!rendered.contains("known_key_binding ="));
        assert!(!rendered.contains("known_facts_context ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse binding only");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.known_key_binding);
        assert!(parsed_graph.known_key_binding_apply());

        cfg.set_key("graph.known_facts_context", "true").unwrap();
        assert!(cfg.graph.as_ref().expect("graph").known_key_binding_apply());
        cfg.set_key("graph.known_key_binding", "false").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(!graph.known_key_binding);
        assert!(!graph.known_key_binding_apply());
        assert!(graph.known_facts_context);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("known_key_binding = false"));
        cfg.set_key("graph.known_key_binding", "true").unwrap();
        cfg.set_key("graph.known_facts_context", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").known_key_binding_apply());
        assert!(cfg.graph.as_ref().expect("graph").known_key_binding);
        assert!(cfg.set_key("graph.known_key_binding", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").known_key_binding);
    }

    #[test]
    fn drop_restatements_defaults_off_round_trips_and_stays_independent() {
        assert!(default_drop_restatements());
        assert!(GraphSection::default().drop_restatements);

        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().drop_restatements);

        let explicit_false: Config =
            toml::from_str("[graph]\ndrop_restatements = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.drop_restatements);
        assert!(false_graph.known_facts_context);
        assert!(!false_graph.known_match);
        assert!(false_graph.consolidate);
        assert!(false_graph.consolidate_hold);

        let explicit_true: Config =
            toml::from_str("[graph]\ndrop_restatements = true\n").expect("parse true");
        let true_graph = explicit_true.graph.expect("graph");
        assert!(true_graph.drop_restatements);
        assert!(true_graph.known_facts_context);
        assert!(!true_graph.known_match);
        assert!(true_graph.consolidate);
        assert!(true_graph.consolidate_hold);

        let rendered_default = toml::to_string(&GraphSection::default()).expect("render default");
        assert!(!rendered_default.contains("drop_restatements ="));
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("drop_restatements = false"));

        let mut cfg = Config::default();
        cfg.set_key("graph.drop_restatements", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.drop_restatements);
        assert!(graph.known_facts_context);
        assert!(!graph.known_match);
        assert!(graph.consolidate);
        assert!(graph.consolidate_hold);
        let rendered = toml::to_string_pretty(&cfg).expect("render true");
        assert!(!rendered.contains("drop_restatements ="));
        assert!(!rendered.contains("known_facts_context ="));
        assert!(!rendered.contains("known_match"));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("consolidate_hold ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse true");
        let parsed_graph = parsed.graph.expect("graph");
        assert!(parsed_graph.drop_restatements);
        assert!(parsed_graph.known_facts_context);
        assert!(!parsed_graph.known_match);
        assert!(parsed_graph.consolidate);
        assert!(parsed_graph.consolidate_hold);

        cfg.set_key("graph.drop_restatements", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").drop_restatements);
        let rendered = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(rendered.contains("drop_restatements = false"));
        let parsed: Config = toml::from_str(&rendered).expect("reparse cleared");
        assert!(!parsed.graph.unwrap_or_default().drop_restatements);

        cfg.set_key("graph.known_facts_context", "true").unwrap();
        cfg.set_key("graph.known_match", "true").unwrap();
        cfg.set_key("graph.consolidate", "true").unwrap();
        cfg.set_key("graph.consolidate_hold", "true").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").drop_restatements);
        cfg.set_key("graph.drop_restatements", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.drop_restatements);
        assert!(graph.known_facts_context);
        assert!(graph.known_match);
        assert!(graph.consolidate);
        assert!(graph.consolidate_hold);

        assert!(cfg.set_key("graph.drop_restatements", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").drop_restatements);
    }

    #[test]
    fn ground_component_edges_defaults_off_round_trips_and_stands_alone() {
        assert!(default_ground_component_edges());
        assert!(GraphSection::default().ground_component_edges);
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(absent.graph.unwrap_or_default().ground_component_edges);
        let explicit_false: Config =
            toml::from_str("[graph]\nground_component_edges = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.ground_component_edges);
        assert!(false_graph.consolidate);
        assert!(false_graph.drop_restatements);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(rendered_false.contains("ground_component_edges = false"));

        let explicit_true: Config =
            toml::from_str("[graph]\nground_component_edges = true\n").expect("parse true");
        let true_graph = explicit_true.graph.expect("graph");
        assert!(true_graph.ground_component_edges);
        assert!(true_graph.consolidate);
        assert!(true_graph.consolidate_links);
        assert!(true_graph.known_facts_context);
        assert!(!true_graph.known_match);
        assert!(true_graph.drop_restatements);
        let mut cfg = Config::default();
        cfg.set_key("graph.ground_component_edges", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.ground_component_edges);
        assert!(graph.consolidate);
        let rendered = toml::to_string_pretty(&cfg).expect("render true");
        assert!(!rendered.contains("ground_component_edges ="));
        assert!(!rendered.contains("consolidate ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse true");
        assert!(parsed.graph.expect("graph").ground_component_edges);

        cfg.set_key("graph.ground_component_edges", "false")
            .unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").ground_component_edges);
        let cleared = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(cleared.contains("ground_component_edges = false"));
        cfg.set_key("graph.ground_component_edges", "true").unwrap();
        assert!(cfg.set_key("graph.ground_component_edges", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").ground_component_edges);
    }

    #[test]
    fn recover_turns_defaults_off_round_trips_and_stands_alone() {
        assert!(!default_recover_turns());
        assert!(!GraphSection::default().recover_turns);
        assert!(!GraphSection::default().recover_applies());
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(!absent.graph.unwrap_or_default().recover_turns);
        let explicit_false: Config =
            toml::from_str("[graph]\nrecover_turns = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.recover_applies());
        assert!(false_graph.ground_component_edges);
        assert!(false_graph.consolidate);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(!rendered_false.contains("recover_turns"));

        let explicit_true: Config =
            toml::from_str("[graph]\nrecover_turns = true\n").expect("parse true");
        let true_graph = explicit_true.graph.expect("graph");
        assert!(true_graph.recover_applies());
        assert!(true_graph.consolidate);
        assert!(true_graph.known_facts_context);
        assert!(!true_graph.known_match);
        assert!(true_graph.drop_restatements);
        assert!(true_graph.ground_component_edges);
        let mut cfg = Config::default();
        cfg.set_key("graph.recover_turns", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.recover_turns);
        assert!(graph.consolidate);
        let rendered = toml::to_string_pretty(&cfg).expect("render true");
        assert!(rendered.contains("recover_turns = true"));
        assert!(!rendered.contains("consolidate ="));
        let parsed: Config = toml::from_str(&rendered).expect("reparse true");
        assert!(parsed.graph.expect("graph").recover_turns);

        cfg.set_key("graph.recover_turns", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").recover_turns);
        let cleared = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(!cleared.contains("recover_turns"));
        cfg.set_key("graph.recover_turns", "true").unwrap();
        assert!(cfg.set_key("graph.recover_turns", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").recover_turns);
    }

    #[test]
    fn lasting_forget_defaults_off_round_trips_and_stands_alone() {
        assert!(!default_lasting_forget());
        assert!(!GraphSection::default().lasting_forget);
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(!absent.graph.unwrap_or_default().lasting_forget);
        let explicit_false: Config =
            toml::from_str("[graph]\nlasting_forget = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.lasting_forget);
        assert!(false_graph.ground_component_edges);
        assert!(false_graph.consolidate);
        assert!(!false_graph.recover_turns);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(!rendered_false.contains("lasting_forget"));
        assert!(!rendered_false.contains("recover_turns"));

        let explicit_true: Config =
            toml::from_str("[graph]\nlasting_forget = true\n").expect("parse true");
        let true_graph = explicit_true.graph.expect("graph");
        assert!(true_graph.lasting_forget);
        assert!(true_graph.consolidate);
        assert!(true_graph.known_facts_context);
        assert!(!true_graph.known_match);
        assert!(!true_graph.recover_turns);
        let mut cfg = Config::default();
        cfg.set_key("graph.lasting_forget", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.lasting_forget);
        assert!(graph.consolidate);
        assert!(!graph.recover_turns);
        let rendered = toml::to_string_pretty(&cfg).expect("render true");
        assert!(rendered.contains("lasting_forget = true"));
        assert!(!rendered.contains("consolidate ="));
        assert!(!rendered.contains("recover_turns"));
        let parsed: Config = toml::from_str(&rendered).expect("reparse true");
        assert!(parsed.graph.expect("graph").lasting_forget);

        cfg.set_key("graph.lasting_forget", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").lasting_forget);
        let cleared = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(!cleared.contains("lasting_forget"));
        cfg.set_key("graph.lasting_forget", "true").unwrap();
        assert!(cfg.set_key("graph.lasting_forget", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").lasting_forget);
    }

    #[test]
    fn extract_coverage_defaults_off_round_trips_and_stands_alone() {
        assert!(!default_extract_coverage());
        assert!(!GraphSection::default().extract_coverage);
        let absent: Config = toml::from_str("").expect("empty config");
        assert!(!absent.graph.unwrap_or_default().extract_coverage);
        let explicit_false: Config =
            toml::from_str("[graph]\nextract_coverage = false\n").expect("parse false");
        let false_graph = explicit_false.graph.expect("graph");
        assert!(!false_graph.extract_coverage);
        assert!(false_graph.known_facts_context);
        assert!(false_graph.ground_component_edges);
        assert!(false_graph.consolidate);
        assert!(!false_graph.lasting_forget);
        let rendered_false = toml::to_string(&false_graph).expect("render false");
        assert!(!rendered_false.contains("extract_coverage"));
        assert!(!rendered_false.contains("lasting_forget"));

        let explicit_true: Config =
            toml::from_str("[graph]\nextract_coverage = true\n").expect("parse true");
        let true_graph = explicit_true.graph.expect("graph");
        assert!(true_graph.extract_coverage);
        assert!(true_graph.known_facts_context);
        assert!(!true_graph.known_match);
        assert!(!true_graph.lasting_forget);
        let mut cfg = Config::default();
        cfg.set_key("graph.extract_coverage", "true").unwrap();
        let graph = cfg.graph.as_ref().expect("graph");
        assert!(graph.extract_coverage);
        assert!(graph.known_facts_context);
        assert!(!graph.lasting_forget);
        let rendered = toml::to_string_pretty(&cfg).expect("render true");
        assert!(rendered.contains("extract_coverage = true"));
        assert!(!rendered.contains("known_facts_context ="));
        assert!(!rendered.contains("lasting_forget"));
        let parsed: Config = toml::from_str(&rendered).expect("reparse true");
        assert!(parsed.graph.expect("graph").extract_coverage);

        cfg.set_key("graph.extract_coverage", "false").unwrap();
        assert!(!cfg.graph.as_ref().expect("graph").extract_coverage);
        let cleared = toml::to_string_pretty(&cfg).expect("render cleared");
        assert!(!cleared.contains("extract_coverage"));
        cfg.set_key("graph.extract_coverage", "true").unwrap();
        assert!(cfg.set_key("graph.extract_coverage", "yes").is_err());
        assert!(cfg.graph.as_ref().expect("graph").extract_coverage);
    }

    #[test]
    fn graph_pipeline_is_v1_or_v2() {
        let absent: Config = toml::from_str("[graph]\nmode = \"embedded\"\n").expect("parse");
        assert!(absent.graph.expect("graph").pipeline.is_none());

        let present: Config =
            toml::from_str("[graph]\npipeline = \"v2\"\n").expect("parse pipeline");
        assert_eq!(
            present.graph.expect("graph").pipeline.as_deref(),
            Some("v2")
        );

        let mut cfg = Config::default();
        cfg.set_key("graph.pipeline", "v2").unwrap();
        assert_eq!(
            cfg.graph.as_ref().expect("graph").pipeline.as_deref(),
            Some("v2")
        );
        assert!(cfg.set_key("graph.pipeline", "v3").is_err());
    }

    #[test]
    fn setting_graph_pipeline_keeps_other_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = config_path(dir.path());
        fs::write(
            &path,
            "[llm]\nmodel = \"kept-model\"\napi_base = \"https://example.test/v1\"\n\n\
             [capture]\nenabled = false\n\n\
             [graph.provenance]\nweight_user = 0.5\n",
        )
        .expect("write config");

        set_graph_pipeline(dir.path(), "v2").expect("set pipeline");

        let cfg = load(dir.path());
        assert_eq!(cfg.llm.model, "kept-model");
        assert_eq!(cfg.llm.api_base, "https://example.test/v1");
        assert!(!cfg.capture.enabled);
        assert_eq!(
            cfg.graph
                .as_ref()
                .and_then(|graph| graph.pipeline.as_deref()),
            Some("v2")
        );
        let weight = cfg.graph.as_ref().expect("graph").provenance.weight_user;
        assert!((weight - 0.5).abs() < f64::EPSILON, "{weight}");
    }

    #[test]
    fn setting_graph_pipeline_does_not_rewrite_a_file_load_would_drop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = config_path(dir.path());
        let broken = "this is not toml [[[";
        fs::write(&path, broken).expect("write broken");
        let err = set_graph_pipeline(dir.path(), "v2").expect_err("parse");
        assert!(err.to_string().contains("refusing to rewrite"), "{err}");
        assert_eq!(fs::read_to_string(&path).expect("read"), broken);

        let removed = format!("[llm]\n{} = 1\nmodel = \"kept\"\n", dropped_llm_keys()[0]);
        fs::write(&path, &removed).expect("write removed key");
        let err = set_graph_pipeline(dir.path(), "v2").expect_err("removed key");
        assert!(err.to_string().contains("refusing to rewrite"), "{err}");
        assert!(err.to_string().contains("removed in this version"), "{err}");
        assert_eq!(fs::read_to_string(&path).expect("read"), removed);
    }

    #[test]
    fn graph_scoring_nested_under_graph() {
        let cfg: Config = toml::from_str(
            "[graph]\nmode = \"embedded\"\n\n[graph.scoring]\nweight_utility = 0.5\n",
        )
        .expect("parse nested scoring");
        let scoring = cfg.graph.expect("graph section present").scoring;
        assert!((scoring.weight_semantic - 0.45).abs() < f64::EPSILON);
        assert!((scoring.weight_hotness - 0.30).abs() < f64::EPSILON);
        assert!((scoring.weight_utility - 0.5).abs() < f64::EPSILON);
    }

    /// Pins every `config set` spelling, alias, boundary and error text.
    ///
    /// `RECALL_ECHO_BLESS=1` rewrites the fixture from the current code. The
    /// fixture is generated once and then compared byte for byte.
    #[test]
    fn set_key_golden_pins_every_key_and_error() {
        let cases = set_key_cases();
        assert_set_key_coverage(&cases);
        let rendered = render_set_key_golden(&cases);
        let path = set_key_golden_path();
        if std::env::var_os("RECALL_ECHO_BLESS").is_some() {
            write_golden(&path, &rendered);
        }
        let expected = fs::read_to_string(&path).expect("set_key golden");
        assert_eq!(rendered, expected);
    }

    fn write_golden(path: &std::path::Path, rendered: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture directory");
        }
        fs::write(path, rendered).expect("write golden");
    }

    fn set_key_golden_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/config/set_key.golden")
    }

    fn set_key_cases() -> Vec<(&'static str, &'static str)> {
        let mut cases = Vec::new();
        push_provider_cases(&mut cases);
        push_cli_cases(&mut cases);
        push_llm_string_cases(&mut cases);
        push_removed_cases(&mut cases);
        push_ephemeral_cases(&mut cases);
        push_pipeline_cases(&mut cases);
        push_serve_cases(&mut cases);
        push_extraction_cases(&mut cases);
        push_capture_cases(&mut cases);
        push_weight_cases(&mut cases);
        push_similarity_cases(&mut cases);
        push_graph_pipeline_cases(&mut cases);
        push_max_candidates_cases(&mut cases);
        push_bool_cases(&mut cases);
        push_unknown_cases(&mut cases);
        cases
    }

    fn push_pairs(
        cases: &mut Vec<(&'static str, &'static str)>,
        key: &'static str,
        values: &[&'static str],
    ) {
        for value in values {
            cases.push((key, value));
        }
    }

    fn push_provider_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "llm.provider", &["cli", "nonesuch"]);
        push_pairs(cases, "provider", &["cli", "nonesuch"]);
    }

    fn push_cli_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "llm.cli.preset", &["custom", "nonesuch"]);
        push_pairs(cases, "llm.cli.command", &["alpha"]);
        push_pairs(cases, "llm.cli.args", &["alpha"]);
        push_pairs(cases, "llm.cli.prompt_delivery", &["stdin", "nonesuch"]);
        push_pairs(cases, "llm.cli.prompt_flag", &["alpha"]);
        push_pairs(cases, "llm.cli.model_flag", &["alpha"]);
        push_pairs(cases, "llm.cli.output_format_flag", &["alpha"]);
        push_pairs(cases, "llm.cli.output_format_value", &["alpha"]);
        push_pairs(cases, "llm.cli.output_mode", &["raw", "nonesuch"]);
        push_pairs(cases, "llm.cli.ndjson_match", &["alpha"]);
        push_pairs(cases, "llm.cli.system_prompt_flag", &["alpha"]);
        push_pairs(cases, "llm.cli.result_json_path", &["alpha"]);
        push_pairs(cases, "llm.cli.usage_input_path", &["alpha"]);
        push_pairs(cases, "llm.cli.usage_output_path", &["alpha"]);
        push_pairs(cases, "llm.cli.extra_args", &["alpha"]);
        push_pairs(cases, "llm.cli.timeout_secs", &["1", "0", "alpha", ""]);
    }

    fn push_llm_string_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "llm.model", &["alpha"]);
        push_pairs(cases, "model", &["alpha"]);
        push_pairs(cases, "llm.api_base", &["/x"]);
        push_pairs(cases, "api_base", &["/x"]);
    }

    fn push_removed_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        for key in [
            "num_ctx",
            "llm.num_ctx",
            "api_style",
            "llm.api_style",
            "llm.cli.num_ctx",
        ] {
            cases.push((key, "1"));
        }
    }

    fn push_ephemeral_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(
            cases,
            "ephemeral.max_entries",
            &["10", "0", "1", "50", "51", "alpha", ""],
        );
    }

    fn push_pipeline_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "pipeline.docs_dir", &["/x"]);
    }

    fn push_serve_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "serve.idle_timeout_secs", &["1", "0", "alpha", ""]);
        push_pairs(cases, "serve.socket_path", &["/x", "", "   ", "  /x  "]);
    }

    fn push_extraction_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(
            cases,
            "extraction.idle_after_secs",
            &["1", "0", "alpha", ""],
        );
        push_pairs(cases, "extraction.batch_size", &["1", "0", "alpha", ""]);
    }

    fn push_capture_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "capture.settle_secs", &["1", "0", "alpha", ""]);
        push_pairs(
            cases,
            "capture.sources",
            &["codex", "", "codex,codex", "nonesuch"],
        );
    }

    fn push_weight_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        for key in WEIGHT_KEYS {
            push_pairs(cases, key, WEIGHT_VALUES);
        }
    }

    fn push_similarity_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        for key in SIMILARITY_KEYS {
            push_pairs(cases, key, SIMILARITY_VALUES);
        }
    }

    fn push_graph_pipeline_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(cases, "graph.pipeline", &["v1", "v2", "v3"]);
    }

    fn push_max_candidates_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        push_pairs(
            cases,
            "graph.dedup.max_candidates",
            &["1", "0", "alpha", ""],
        );
    }

    fn push_bool_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        for key in BOOL_KEYS {
            push_pairs(cases, key, BOOL_SPELLINGS);
        }
    }

    fn push_unknown_cases(cases: &mut Vec<(&'static str, &'static str)>) {
        for key in UNKNOWN_KEYS {
            cases.push((key, "alpha"));
        }
    }

    const BOOL_SPELLINGS: &[&str] = &["true", "false", "yes", "True", "1"];

    const BOOL_KEYS: &[&str] = &[
        "pipeline.auto_sync",
        "extraction.background_enabled",
        "capture.enabled",
        "graph.known_facts_context",
        "graph.known_key_binding",
        "graph.known_match",
        "graph.consolidate",
        "graph.consolidate_hold",
        "graph.consolidate_links",
        "graph.consolidate_components",
        "graph.consolidate_key_rename",
        "graph.consolidate_replaces_cue",
        "graph.consolidate_replaces_departure",
        "graph.consolidate_orphan_sweep",
        "graph.consolidate_rule_fragments",
        "graph.consolidate_retire_component",
        "graph.drop_restatements",
        "graph.ground_component_edges",
        "graph.recover_turns",
    ];

    const WEIGHT_KEYS: &[&str] = &[
        "graph.provenance.weight_external",
        "graph.provenance.weight_user",
        "graph.provenance.weight_assistant",
        "graph.provenance.weight_self",
    ];

    const WEIGHT_VALUES: &[&str] = &["1", "0", "-1", "inf", "NaN", "alpha", ""];

    const SIMILARITY_KEYS: &[&str] = &[
        "graph.dedup.certain_similarity",
        "graph.dedup.review_similarity",
    ];

    const SIMILARITY_VALUES: &[&str] = &["0.0", "1.0", "1.5", "-0.1", "NaN", "alpha", ""];

    const UNKNOWN_KEYS: &[&str] = &[
        "nonexistent.key",
        "",
        "llm.cli.",
        "llm.cli",
        "llm.cli.nonexistent",
        "LLM.MODEL",
        "llm.model ",
    ];

    const ACCEPTED_SPELLINGS: &[&str] = &[
        "llm.provider",
        "provider",
        "llm.model",
        "model",
        "llm.api_base",
        "api_base",
        "llm.cli.preset",
        "llm.cli.command",
        "llm.cli.args",
        "llm.cli.prompt_delivery",
        "llm.cli.prompt_flag",
        "llm.cli.model_flag",
        "llm.cli.output_format_flag",
        "llm.cli.output_format_value",
        "llm.cli.output_mode",
        "llm.cli.ndjson_match",
        "llm.cli.system_prompt_flag",
        "llm.cli.result_json_path",
        "llm.cli.usage_input_path",
        "llm.cli.usage_output_path",
        "llm.cli.extra_args",
        "llm.cli.timeout_secs",
        "ephemeral.max_entries",
        "pipeline.docs_dir",
        "pipeline.auto_sync",
        "serve.idle_timeout_secs",
        "serve.socket_path",
        "extraction.background_enabled",
        "extraction.idle_after_secs",
        "extraction.batch_size",
        "capture.enabled",
        "capture.settle_secs",
        "capture.sources",
        "graph.provenance.weight_external",
        "graph.provenance.weight_user",
        "graph.provenance.weight_assistant",
        "graph.provenance.weight_self",
        "graph.dedup.certain_similarity",
        "graph.dedup.review_similarity",
        "graph.pipeline",
        "graph.dedup.max_candidates",
        "graph.known_facts_context",
        "graph.known_key_binding",
        "graph.known_match",
        "graph.consolidate",
        "graph.consolidate_hold",
        "graph.consolidate_links",
        "graph.consolidate_components",
        "graph.consolidate_key_rename",
        "graph.consolidate_replaces_cue",
        "graph.consolidate_replaces_departure",
        "graph.consolidate_orphan_sweep",
        "graph.consolidate_rule_fragments",
        "graph.consolidate_retire_component",
        "graph.drop_restatements",
        "graph.ground_component_edges",
        "graph.recover_turns",
    ];

    fn assert_set_key_coverage(cases: &[(&str, &str)]) {
        assert_eq!(ACCEPTED_SPELLINGS.len(), 57);
        assert_eq!(BOOL_KEYS.len(), 19);
        assert_eq!(BOOL_SPELLINGS, ["true", "false", "yes", "True", "1"]);
        for key in ACCEPTED_SPELLINGS {
            assert!(case_has_key(cases, key), "missing {key}");
        }
        assert_has_values(
            cases,
            "ephemeral.max_entries",
            &["10", "0", "1", "50", "51", "alpha", ""],
        );
        assert_has_values(cases, "extraction.batch_size", &["1", "0", "alpha", ""]);
        assert_has_values(
            cases,
            "graph.dedup.max_candidates",
            &["1", "0", "alpha", ""],
        );
        assert_has_values(cases, "serve.socket_path", &["/x", "", "   ", "  /x  "]);
        assert_has_values(
            cases,
            "capture.sources",
            &["codex", "", "codex,codex", "nonesuch"],
        );
        assert_has_values(cases, "graph.pipeline", &["v1", "v2", "v3"]);
        assert_has_values(cases, "llm.provider", &["cli", "nonesuch"]);
        assert_has_values(cases, "provider", &["cli", "nonesuch"]);
        for key in WEIGHT_KEYS {
            assert_has_values(cases, key, WEIGHT_VALUES);
        }
        for key in SIMILARITY_KEYS {
            assert_has_values(cases, key, SIMILARITY_VALUES);
        }
        for key in BOOL_KEYS {
            assert_has_values(cases, key, BOOL_SPELLINGS);
        }
        for key in [
            "num_ctx",
            "llm.num_ctx",
            "api_style",
            "llm.api_style",
            "llm.cli.num_ctx",
        ] {
            assert!(case_has_key(cases, key), "missing {key}");
        }
        for key in UNKNOWN_KEYS {
            assert!(case_has_key(cases, key), "missing {key}");
        }
    }

    fn case_has_key(cases: &[(&str, &str)], key: &str) -> bool {
        cases.iter().any(|(case_key, _)| *case_key == key)
    }

    fn assert_has_values(cases: &[(&str, &str)], key: &str, values: &[&str]) {
        for value in values {
            assert!(
                cases
                    .iter()
                    .any(|(case_key, case_value)| { *case_key == key && *case_value == *value }),
                "missing {key}={value}"
            );
        }
    }

    fn render_set_key_golden(cases: &[(&str, &str)]) -> String {
        let mut out = String::new();
        for (name, base) in set_key_bases() {
            assert_config_round_trip(&base);
            for (key, value) in cases {
                out.push_str(&render_set_key_case(name, &base, key, value));
                out.push('\n');
            }
        }
        out.push_str(&render_pipeline_sequence());
        out.push_str(&render_provider_reset_sequence());
        out
    }

    fn set_key_bases() -> Vec<(&'static str, Config)> {
        vec![
            ("default", Config::default()),
            ("populated", populated_config()),
        ]
    }

    fn populated_config() -> Config {
        let mut cfg = Config::default();
        cfg.llm.model = "alpha".to_string();
        cfg.llm.api_base = "/x".to_string();
        cfg.llm.cli.command = Some("alpha".to_string());
        cfg.pipeline = Some(PipelineSection {
            docs_dir: Some("/x".to_string()),
            auto_sync: Some(true),
        });
        cfg.graph = Some(populated_graph());
        cfg.serve.socket_path = Some("/x".to_string());
        cfg.serve.idle_timeout_secs = 9;
        cfg.extraction.background_enabled = false;
        cfg.extraction.idle_after_secs = 9;
        cfg.extraction.batch_size = 9;
        cfg.capture.enabled = false;
        cfg.capture.sources = Some(vec![crate::transcript::Source::Codex]);
        cfg.capture.settle_secs = 9;
        cfg
    }

    fn populated_graph() -> GraphSection {
        let mut graph = GraphSection::default();
        enable_graph_flags(&mut graph);
        graph.provenance.weight_external = 2.0;
        graph.provenance.weight_user = 3.0;
        graph.provenance.weight_assistant = 4.0;
        graph.provenance.weight_self = 5.0;
        graph.dedup.certain_similarity = 0.5;
        graph.dedup.review_similarity = 0.25;
        graph.dedup.max_candidates = 7;
        graph.pipeline = Some("v2".to_string());
        graph
    }

    fn enable_graph_flags(graph: &mut GraphSection) {
        graph.known_facts_context = true;
        graph.known_key_binding = true;
        graph.known_match = true;
        graph.consolidate = true;
        graph.consolidate_hold = true;
        graph.consolidate_links = true;
        graph.consolidate_components = true;
        graph.consolidate_key_rename = true;
        graph.consolidate_replaces_cue = true;
        graph.consolidate_replaces_departure = true;
        graph.consolidate_orphan_sweep = true;
        graph.consolidate_rule_fragments = true;
        graph.consolidate_retire_component = true;
        graph.drop_restatements = true;
        graph.ground_component_edges = true;
        graph.recover_turns = true;
    }

    fn render_set_key_case(base_name: &str, base: &Config, key: &str, value: &str) -> String {
        let mut cfg = clone_config(base);
        let header = format!("{base_name} {key} = {value}\n");
        match apply_set_key(&mut cfg, key, value) {
            Ok(diff) => format!("{header}ok\n{diff}"),
            Err(err) => format!("{header}err {err}\n"),
        }
    }

    fn apply_set_key(cfg: &mut Config, key: &str, value: &str) -> Result<String, String> {
        let before = config_lines(cfg);
        let base_toml = toml::to_string(cfg).unwrap();
        match cfg.set_key(key, value) {
            Ok(()) => {
                assert_config_round_trip(cfg);
                Ok(diff_lines(&before, &config_lines(cfg)))
            }
            Err(err) => {
                assert_eq!(before, config_lines(cfg), "error changed {key}");
                assert_eq!(
                    toml::to_string(cfg).unwrap(),
                    base_toml,
                    "error changed {key}"
                );
                Err(err.to_string())
            }
        }
    }

    fn render_pipeline_sequence() -> String {
        let mut cfg = Config::default();
        let before = config_lines(&cfg);
        cfg.set_key("pipeline.docs_dir", "/x").expect("docs_dir");
        cfg.set_key("pipeline.auto_sync", "true")
            .expect("auto_sync");
        let section = cfg.pipeline.as_ref().expect("pipeline");
        assert_eq!(section.docs_dir.as_deref(), Some("/x"));
        assert_eq!(section.auto_sync, Some(true));
        assert_config_round_trip(&cfg);
        let diff = diff_lines(&before, &config_lines(&cfg));
        format!("seq default pipeline.docs_dir = /x then pipeline.auto_sync = true\nok\n{diff}\n")
    }

    fn render_provider_reset_sequence() -> String {
        let mut cfg = Config::default();
        let before = config_lines(&cfg);
        cfg.set_key("llm.model", "alpha").expect("model");
        cfg.set_key("provider", "cli").expect("provider");
        assert!(cfg.llm.model.is_empty());
        assert!(cfg.llm.api_base.is_empty());
        assert!(cfg.llm.cli.is_empty());
        assert_eq!(cfg.llm.provider, Provider::Cli);
        assert_config_round_trip(&cfg);
        let diff = diff_lines(&before, &config_lines(&cfg));
        format!("seq default llm.model = alpha then provider = cli\nok\n{diff}\n")
    }

    fn clone_config(cfg: &Config) -> Config {
        let text = toml::to_string(cfg).expect("config serializes");
        toml::from_str(&text).expect("config parses")
    }

    fn assert_config_round_trip(cfg: &Config) {
        let once = toml::to_string(cfg).expect("config serializes");
        let parsed: Config = toml::from_str(&once).expect("config parses");
        let twice = toml::to_string(&parsed).expect("config serializes again");
        assert_eq!(once, twice, "config serialization is not a round trip");
    }

    fn config_lines(cfg: &Config) -> Vec<String> {
        let text = toml::to_string(cfg).expect("config serializes");
        let value: toml::Value = toml::from_str(&text).expect("config toml parses");
        let mut lines = Vec::new();
        flatten_toml("", &value, &mut lines);
        lines
    }

    fn flatten_toml(prefix: &str, value: &toml::Value, out: &mut Vec<String>) {
        if let toml::Value::Table(table) = value {
            for (key, child) in table {
                flatten_toml(&join_toml_path(prefix, key), child, out);
            }
            return;
        }
        out.push(format!("{prefix} = {}", toml_leaf(value)));
    }

    fn join_toml_path(prefix: &str, key: &str) -> String {
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        }
    }

    fn toml_leaf(value: &toml::Value) -> String {
        let mut table = toml::map::Map::new();
        table.insert("v".to_string(), value.clone());
        let text = toml::to_string(&table).expect("leaf serializes");
        text.trim_start_matches("v = ")
            .trim_end_matches(['\r', '\n'])
            .to_string()
    }

    fn diff_lines(before: &[String], after: &[String]) -> String {
        let mut out = String::new();
        for path in flat_paths(before, after) {
            push_changed(
                &mut out,
                path,
                flat_get(before, path),
                flat_get(after, path),
            );
        }
        out
    }

    fn flat_paths<'a>(
        before: &'a [String],
        after: &'a [String],
    ) -> std::collections::BTreeSet<&'a str> {
        before
            .iter()
            .chain(after.iter())
            .map(|line| split_flat(line).0)
            .collect()
    }

    fn flat_get<'a>(lines: &'a [String], path: &str) -> Option<&'a str> {
        lines.iter().find_map(|line| {
            let (key, value) = split_flat(line);
            (key == path).then_some(value)
        })
    }

    fn split_flat(line: &str) -> (&str, &str) {
        line.split_once(" = ").unwrap_or((line, ""))
    }

    fn push_changed(out: &mut String, path: &str, old: Option<&str>, new: Option<&str>) {
        if old == new {
            return;
        }
        if let Some(value) = old {
            out.push_str(&format!("-{path} = {value}\n"));
        }
        if let Some(value) = new {
            out.push_str(&format!("+{path} = {value}\n"));
        }
    }
}
