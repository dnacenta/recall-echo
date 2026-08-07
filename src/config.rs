// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
use std::fs;
use std::path::Path;

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

// ── Provider enum ────────────────────────────────────────────────────────

/// LLM provider for entity extraction.
///
/// Two families. [`Provider::Anthropic`] and [`Provider::Openai`] talk HTTP —
/// an API key, billed per token (the OpenAI-compatible one also covers Ollama
/// and any local server that speaks that protocol). Everything else spawns an
/// agent CLI the user already pays a subscription for; those are all one
/// implementation driven by a [`CliPreset`], so supporting a new vendor is a
/// preset — or just a `[llm.cli]` section — rather than a new code path.
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
            Provider::Openai => "llama3.2",
            _ => "",
        }
    }

    #[must_use]
    pub fn default_api_base(&self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::Openai => "http://localhost:11434/v1",
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
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "openai" | "ollama" | "openai-compat" => Ok(Provider::Openai),
            "claude-code" | "claudecode" => Ok(Provider::ClaudeCode),
            "gemini" | "gemini-cli" | "google" => Ok(Provider::Gemini),
            "grok" | "grok-cli" | "xai" => Ok(Provider::Grok),
            "codex" | "codex-cli" => Ok(Provider::Codex),
            "cli" | "custom" | "custom-cli" => Ok(Provider::Cli),
            other => Err(crate::error::RecallError::Config(format!(
                "unknown provider: {other} (use 'anthropic', 'ollama', 'claude-code', \
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
        use crate::error::RecallError;
        match key {
            "preset" => self.preset = Some(CliPreset::from_str_loose(value)?),
            "command" => self.command = Some(value.to_string()),
            "args" => self.args = Some(split_args(value)),
            "prompt_delivery" => {
                self.prompt_delivery = Some(PromptDelivery::from_str_loose(value)?)
            }
            "prompt_flag" => self.prompt_flag = Some(value.to_string()),
            "model_flag" => self.model_flag = Some(value.to_string()),
            "output_format_flag" => self.output_format_flag = Some(value.to_string()),
            "output_format_value" => self.output_format_value = Some(value.to_string()),
            "output_mode" => self.output_mode = Some(OutputMode::from_str_loose(value)?),
            "ndjson_match" => self.ndjson_match = Some(LineMatchers::parse(value)),
            "system_prompt_flag" => self.system_prompt_flag = Some(value.to_string()),
            "result_json_path" => self.result_json_path = Some(JsonPaths::parse(value)),
            "usage_input_path" => self.usage_input_path = Some(JsonPaths::parse(value)),
            "usage_output_path" => self.usage_output_path = Some(JsonPaths::parse(value)),
            "extra_args" => self.extra_args = Some(split_args(value)),
            "timeout_secs" => {
                self.timeout_secs = Some(
                    value
                        .parse()
                        .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?,
                );
            }
            other => {
                return Err(RecallError::Config(format!(
                    "unknown config key: llm.cli.{other}"
                )))
            }
        }
        Ok(())
    }
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
    #[serde(default = "default_provider")]
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
            provider: Provider::Anthropic,
            model: String::new(),
            api_base: String::new(),
            cli: CliSection::default(),
        }
    }
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
    /// SurrealDB database name (typically the entity name)
    #[serde(default)]
    pub database: String,
    /// SurrealDB username (typically the entity name)
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
    /// absent, defaults are 1.0 external / 0.8 user / 0.05 self. See
    /// [`ProvenanceWeights`] for details.
    #[serde(default)]
    pub provenance: ProvenanceWeights,
    /// Similarity bands that decide when entity dedup pays for a model call.
    ///
    /// Maps to the `[graph.dedup]` section of `.recall-echo.toml`. See
    /// [`GraphDedupConfig`] for the bands and their defaults.
    #[serde(default)]
    pub dedup: GraphDedupConfig,
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
        }
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

fn default_provider() -> Provider {
    Provider::Anthropic
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

/// Load config from .recall-echo.toml in the base dir.
/// Returns defaults if file doesn't exist or is malformed.
#[must_use]
pub fn load(base: &Path) -> Config {
    let path = config_path(base);
    if !path.exists() {
        return Config::default();
    }

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Config::default(),
    };

    match toml::from_str(&content) {
        Ok(cfg) => validate(cfg),
        Err(_) => Config::default(),
    }
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

fn validate(mut cfg: Config) -> Config {
    if !(1..=50).contains(&cfg.ephemeral.max_entries) {
        cfg.ephemeral.max_entries = DEFAULT_MAX_ENTRIES;
    }
    cfg
}

// ── Config mutation helpers ──────────────────────────────────────────────

impl Config {
    /// Set a dotted config key (e.g. "llm.provider", "ephemeral.max_entries").
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<(), crate::error::RecallError> {
        use crate::error::RecallError;
        match key {
            "llm.provider" | "provider" => {
                let provider = Provider::from_str_loose(value)?;
                // When switching provider, reset model, api_base and the CLI
                // overrides to defaults: all three describe the old vendor.
                self.llm.model = String::new();
                self.llm.api_base = String::new();
                self.llm.cli = CliSection::default();
                self.llm.provider = provider;
                Ok(())
            }
            _ if key.starts_with("llm.cli.") => {
                self.llm.cli.set_key(&key["llm.cli.".len()..], value)
            }
            "llm.model" | "model" => {
                self.llm.model = value.to_string();
                Ok(())
            }
            "llm.api_base" | "api_base" => {
                self.llm.api_base = value.to_string();
                Ok(())
            }
            "ephemeral.max_entries" => {
                let n: usize = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
                if !(1..=50).contains(&n) {
                    return Err(RecallError::Config(
                        "max_entries must be between 1 and 50".into(),
                    ));
                }
                self.ephemeral.max_entries = n;
                Ok(())
            }
            "pipeline.docs_dir" => {
                let section = self.pipeline.get_or_insert(PipelineSection {
                    docs_dir: None,
                    auto_sync: None,
                });
                section.docs_dir = Some(value.to_string());
                Ok(())
            }
            "pipeline.auto_sync" => {
                let b: bool = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid boolean: {value}")))?;
                let section = self.pipeline.get_or_insert(PipelineSection {
                    docs_dir: None,
                    auto_sync: None,
                });
                section.auto_sync = Some(b);
                Ok(())
            }
            "serve.idle_timeout_secs" => {
                let secs: u64 = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
                self.serve.idle_timeout_secs = secs;
                Ok(())
            }
            "serve.socket_path" => {
                self.serve.socket_path = if value.trim().is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
                Ok(())
            }
            "extraction.background_enabled" => {
                self.extraction.background_enabled = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid boolean: {value}")))?;
                Ok(())
            }
            "extraction.idle_after_secs" => {
                self.extraction.idle_after_secs = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
                Ok(())
            }
            "extraction.batch_size" => {
                let size: usize = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
                if size == 0 {
                    return Err(RecallError::Config("batch_size must be at least 1".into()));
                }
                self.extraction.batch_size = size;
                Ok(())
            }
            "capture.enabled" => {
                self.capture.enabled = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid boolean: {value}")))?;
                Ok(())
            }
            "capture.settle_secs" => {
                self.capture.settle_secs = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
                Ok(())
            }
            "capture.sources" => {
                self.capture.sources = parse_sources(value)?;
                Ok(())
            }
            "graph.provenance.weight_external" => {
                self.graph_section().provenance.weight_external = parse_weight(value)?;
                Ok(())
            }
            "graph.provenance.weight_user" => {
                self.graph_section().provenance.weight_user = parse_weight(value)?;
                Ok(())
            }
            "graph.provenance.weight_self" => {
                self.graph_section().provenance.weight_self = parse_weight(value)?;
                Ok(())
            }
            "graph.dedup.certain_similarity" => {
                self.graph_section().dedup.certain_similarity = parse_similarity(value)?;
                Ok(())
            }
            "graph.dedup.review_similarity" => {
                self.graph_section().dedup.review_similarity = parse_similarity(value)?;
                Ok(())
            }
            "graph.dedup.max_candidates" => {
                let n: usize = value
                    .parse()
                    .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
                if n == 0 {
                    return Err(RecallError::Config(
                        "max_candidates must be at least 1".into(),
                    ));
                }
                self.graph_section().dedup.max_candidates = n;
                Ok(())
            }
            other => Err(RecallError::Config(format!("unknown config key: {other}"))),
        }
    }

    /// The `[graph]` section, created at its defaults if the config has none.
    fn graph_section(&mut self) -> &mut GraphSection {
        self.graph.get_or_insert_with(GraphSection::default)
    }
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
    let weight: f64 = value
        .parse()
        .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
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
    let similarity: f64 = value
        .parse()
        .map_err(|_| RecallError::Config(format!("invalid number: {value}")))?;
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
        assert_eq!(cfg.llm.provider, Provider::Anthropic);
        assert!(cfg.llm.model.is_empty());
    }

    #[test]
    fn parse_ephemeral_only() {
        let cfg: Config = toml::from_str("[ephemeral]\nmax_entries = 10\n").unwrap();
        assert_eq!(cfg.ephemeral.max_entries, 10);
        assert_eq!(cfg.llm.provider, Provider::Anthropic);
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
            "[llm]\nprovider = \"openai\"\nmodel = \"llama3.1\"\napi_base = \"http://myhost:11434/v1\"\n",
        )
        .unwrap();
        assert_eq!(cfg.llm.provider, Provider::Openai);
        assert_eq!(cfg.llm.model, "llama3.1");
        assert_eq!(cfg.llm.api_base, "http://myhost:11434/v1");
    }

    #[test]
    fn parse_claude_code_provider() {
        let cfg: Config = toml::from_str("[llm]\nprovider = \"claude-code\"\n").unwrap();
        assert_eq!(cfg.llm.provider, Provider::ClaudeCode);
    }

    #[test]
    fn resolved_defaults() {
        let llm = LlmSection::default();
        assert_eq!(llm.resolved_model(), "claude-haiku-4-5-20251001");
        assert_eq!(
            llm.resolved_api_base(),
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
        assert_eq!(llm.resolved_api_base(), "http://localhost:11434/v1");
    }

    #[test]
    fn round_trip_toml() {
        let cfg = Config {
            ephemeral: EphemeralConfig { max_entries: 3 },
            llm: LlmSection {
                provider: Provider::Openai,
                model: "llama3.2".into(),
                api_base: "http://localhost:11434/v1".into(),
                ..LlmSection::default()
            },
            ..Config::default()
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&s).unwrap();
        assert_eq!(parsed.ephemeral.max_entries, 3);
        assert_eq!(parsed.llm.provider, Provider::Openai);
        assert_eq!(parsed.llm.model, "llama3.2");
    }

    #[test]
    fn set_key_provider() {
        let mut cfg = Config::default();
        cfg.set_key("llm.provider", "ollama").unwrap();
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
            Provider::from_str_loose("ollama").unwrap(),
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
        });
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
    }

    #[test]
    fn set_key_provenance_weights() {
        let mut cfg = Config::default();
        cfg.set_key("graph.provenance.weight_self", "0.2").unwrap();
        cfg.set_key("graph.provenance.weight_user", "0").unwrap();
        cfg.set_key("graph.provenance.weight_external", "1.5")
            .unwrap();

        let provenance = cfg
            .graph
            .as_ref()
            .expect("graph section created")
            .provenance;
        assert!((provenance.weight_self - 0.2).abs() < f64::EPSILON);
        assert!(provenance.weight_user.abs() < f64::EPSILON);
        assert!((provenance.weight_external - 1.5).abs() < f64::EPSILON);

        assert!(cfg.set_key("graph.provenance.weight_self", "-1").is_err());
        assert!(cfg.set_key("graph.provenance.weight_self", "lots").is_err());
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
}
