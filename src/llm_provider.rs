//! HTTP-based LLM providers implementing recall_graph::LlmProvider.
//!
//! Loads provider/model/api_base from `.recall-echo.toml` config.
//! API keys are read from environment variables (never stored in config).

use std::env;
use std::path::Path;

use recall_graph::error::GraphError;
use recall_graph::llm::LlmProvider;

use crate::config::{self, Provider};

/// API protocol style (derived from config Provider).
#[derive(Debug, Clone)]
pub enum ApiStyle {
    Anthropic,
    OpenAiCompat,
}

/// Resolved configuration for an HTTP LLM provider.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
    pub api_style: ApiStyle,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
}

impl LlmConfig {
    /// Load LLM config from `.recall-echo.toml` in the given memory directory.
    /// Provider, model, and api_base come from config. API key comes from env.
    ///
    /// Falls back to env vars `RECALL_LLM_*` if no config file exists.
    pub fn load(memory_dir: &Path) -> Result<Self, String> {
        let cfg = config::load(memory_dir);
        Self::from_config_section(&cfg.llm)
    }

    /// Build from a config LlmSection.
    pub fn from_config_section(llm: &config::LlmSection) -> Result<Self, String> {
        if llm.provider == Provider::ClaudeCode {
            return Err(
                "Provider is set to claude-code. Entity extraction runs in-session, not via CLI.\n\
                 Use --provider to override, or run: recall-echo config set provider anthropic"
                    .into(),
            );
        }

        let api_style = match llm.provider {
            Provider::Anthropic => ApiStyle::Anthropic,
            Provider::Openai => ApiStyle::OpenAiCompat,
            Provider::ClaudeCode => unreachable!(),
        };

        let api_key = env::var("RECALL_LLM_API_KEY")
            .or_else(|_| match &api_style {
                ApiStyle::Anthropic => env::var("ANTHROPIC_API_KEY"),
                ApiStyle::OpenAiCompat => {
                    // Ollama doesn't need a real key, but the env var check still applies
                    env::var("OPENAI_API_KEY").or_else(|_| Ok("ollama".into()))
                }
            })
            .map_err(|_| {
                "No API key found. Set ANTHROPIC_API_KEY or OPENAI_API_KEY in your environment."
                    .to_string()
            })?;

        let model = llm.resolved_model().to_string();
        let api_base = llm.resolved_api_base().to_string();

        let max_retries = env::var("RECALL_LLM_MAX_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        let retry_delay_ms = env::var("RECALL_LLM_RETRY_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        Ok(Self {
            api_key,
            model,
            api_base,
            api_style,
            max_retries,
            retry_delay_ms,
        })
    }
}

/// HTTP-based LLM provider.
pub struct HttpLlmProvider {
    client: reqwest::Client,
    config: LlmConfig,
}

impl HttpLlmProvider {
    pub fn new(config: LlmConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    async fn try_complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError> {
        match &self.config.api_style {
            ApiStyle::Anthropic => {
                self.complete_anthropic(system_prompt, user_message, max_tokens)
                    .await
            }
            ApiStyle::OpenAiCompat => {
                self.complete_openai(system_prompt, user_message, max_tokens)
                    .await
            }
        }
    }

    async fn complete_anthropic(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError> {
        let body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_message}],
        });

        let response = self
            .client
            .post(&self.config.api_base)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| GraphError::Llm(format!("request failed: {e}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| GraphError::Llm(format!("read body: {e}")))?;

        if !status.is_success() {
            return Err(GraphError::Llm(format!(
                "API {}: {}",
                status,
                truncate_error(&text)
            )));
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| GraphError::Llm(format!("parse: {e}")))?;

        json["content"][0]["text"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| GraphError::Llm("no text in anthropic response".into()))
    }

    async fn complete_openai(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError> {
        let body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_message},
            ],
        });

        let url = format!(
            "{}/chat/completions",
            self.config.api_base.trim_end_matches('/')
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| GraphError::Llm(format!("request failed: {e}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| GraphError::Llm(format!("read body: {e}")))?;

        if !status.is_success() {
            return Err(GraphError::Llm(format!(
                "API {}: {}",
                status,
                truncate_error(&text)
            )));
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| GraphError::Llm(format!("parse: {e}")))?;

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| GraphError::Llm("no text in openai response".into()))
    }

    fn is_retryable(err: &GraphError) -> bool {
        if let GraphError::Llm(msg) = err {
            msg.contains("API 429") || msg.contains("API 5")
        } else {
            false
        }
    }
}

fn truncate_error(text: &str) -> &str {
    let end = text.len().min(300);
    // Find valid UTF-8 boundary
    let mut i = end;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    &text[..i]
}

#[async_trait::async_trait]
impl LlmProvider for HttpLlmProvider {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError> {
        let mut last_error = None;

        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.config.retry_delay_ms * u64::from(attempt),
                ))
                .await;
            }

            match self
                .try_complete(system_prompt, user_message, max_tokens)
                .await
            {
                Ok(text) => return Ok(text),
                Err(e) => {
                    if !Self::is_retryable(&e) || attempt == self.config.max_retries {
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| GraphError::Llm("no attempts made".into())))
    }
}
