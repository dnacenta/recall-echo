use std::fmt;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

const DEFAULT_MAX_ENTRIES: usize = 5;
const CONFIG_FILE: &str = ".recall-echo.toml";

// ── Provider enum ────────────────────────────────────────────────────────

/// LLM provider for entity extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    Openai,
    ClaudeCode,
}

impl Provider {
    #[must_use]
    pub fn default_model(&self) -> &'static str {
        match self {
            Provider::Anthropic => "claude-haiku-4-5-20251001",
            Provider::Openai => "llama3.2",
            Provider::ClaudeCode => "",
        }
    }

    #[must_use]
    pub fn default_api_base(&self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::Openai => "http://localhost:11434/v1",
            Provider::ClaudeCode => "",
        }
    }

    pub fn from_str_loose(s: &str) -> Result<Self, crate::error::RecallError> {
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "openai" | "ollama" => Ok(Provider::Openai),
            "claude-code" | "claudecode" => Ok(Provider::ClaudeCode),
            other => Err(crate::error::RecallError::Config(format!(
                "unknown provider: {other} (use 'anthropic', 'ollama', or 'claude-code')"
            ))),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Provider::Anthropic => write!(f, "anthropic"),
            Provider::Openai => write!(f, "openai"),
            Provider::ClaudeCode => write!(f, "claude-code"),
        }
    }
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
}

impl Default for LlmSection {
    fn default() -> Self {
        Self {
            provider: Provider::Anthropic,
            model: String::new(),
            api_base: String::new(),
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
        }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphScoringConfig {
    /// Weight applied to cosine similarity. Default `0.45`.
    pub weight_semantic: f64,
    /// Weight applied to the recency/access hotness signal. Default `0.30`.
    pub weight_hotness: f64,
    /// Weight applied to the utility score (outcome-feedback EMA). Default `0.25`.
    pub weight_utility: f64,
}

impl Default for GraphScoringConfig {
    fn default() -> Self {
        Self {
            weight_semantic: 0.45,
            weight_hotness: 0.30,
            weight_utility: 0.25,
        }
    }
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
                // When switching provider, reset model and api_base to defaults
                self.llm.model = String::new();
                self.llm.api_base = String::new();
                self.llm.provider = provider;
                Ok(())
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
            other => Err(RecallError::Config(format!("unknown config key: {other}"))),
        }
    }
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
            api_base: String::new(),
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
            },
            pipeline: None,
            graph: None,
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
                model: String::new(),
                api_base: String::new(),
            },
            pipeline: None,
            graph: None,
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
            llm: LlmSection::default(),
            pipeline: None,
            graph: None,
        });
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn graph_scoring_defaults_match_legacy_hardcodes() {
        let scoring = GraphScoringConfig::default();
        assert!((scoring.weight_semantic - 0.45).abs() < f64::EPSILON);
        assert!((scoring.weight_hotness - 0.30).abs() < f64::EPSILON);
        assert!((scoring.weight_utility - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn graph_scoring_partial_toml_fills_defaults() {
        let scoring: GraphScoringConfig =
            toml::from_str("weight_utility = 0.5\n").expect("parse partial scoring");
        assert!((scoring.weight_semantic - 0.45).abs() < f64::EPSILON);
        assert!((scoring.weight_hotness - 0.30).abs() < f64::EPSILON);
        assert!((scoring.weight_utility - 0.5).abs() < f64::EPSILON);
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
