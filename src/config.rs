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
    #[serde(default)]
    pub serve: ServeSection,
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
            serve: ServeSection::default(),
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
            serve: ServeSection::default(),
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
            serve: ServeSection::default(),
        });
        assert_eq!(cfg.ephemeral.max_entries, 5);
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
