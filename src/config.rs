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
    pub fn default_model(&self) -> &'static str {
        match self {
            Provider::Anthropic => "claude-haiku-4-5-20251001",
            Provider::Openai => "llama3.2",
            Provider::ClaudeCode => "",
        }
    }

    pub fn default_api_base(&self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::Openai => "http://localhost:11434/v1",
            Provider::ClaudeCode => "",
        }
    }

    pub fn from_str_loose(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "openai" | "ollama" => Ok(Provider::Openai),
            "claude-code" | "claudecode" => Ok(Provider::ClaudeCode),
            other => Err(format!(
                "unknown provider: {other} (use 'anthropic', 'ollama', or 'claude-code')"
            )),
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
    pub fn resolved_model(&self) -> &str {
        if self.model.is_empty() {
            self.provider.default_model()
        } else {
            &self.model
        }
    }

    /// Resolved API base — uses configured value or provider default.
    pub fn resolved_api_base(&self) -> &str {
        if self.api_base.is_empty() {
            self.provider.default_api_base()
        } else {
            &self.api_base
        }
    }
}

fn default_provider() -> Provider {
    Provider::Anthropic
}

// ── Load / Save ──────────────────────────────────────────────────────────

/// Config file path for a given base directory.
pub fn config_path(base: &Path) -> std::path::PathBuf {
    base.join(CONFIG_FILE)
}

/// Load config from .recall-echo.toml in the given directory.
/// Returns defaults if file doesn't exist or is malformed.
pub fn load_from_dir(dir: &Path) -> Config {
    load(dir)
}

/// Load config from .recall-echo.toml in the base dir.
/// Returns defaults if file doesn't exist or is malformed.
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
pub fn save(base: &Path, config: &Config) -> Result<(), String> {
    let path = config_path(base);
    let content = toml::to_string_pretty(config).map_err(|e| format!("serialize config: {e}"))?;
    fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Returns true if .recall-echo.toml exists in the directory.
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
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<(), String> {
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
                    .map_err(|_| format!("invalid number: {value}"))?;
                if !(1..=50).contains(&n) {
                    return Err("max_entries must be between 1 and 50".into());
                }
                self.ephemeral.max_entries = n;
                Ok(())
            }
            other => Err(format!("unknown config key: {other}")),
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
        });
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }
}
