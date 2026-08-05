// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LLM providers implementing crate::graph::LlmProvider.
//!
//! Two families:
//! - **HTTP** — Anthropic (x-api-key) or any OpenAI-compatible endpoint,
//!   Ollama included. Billed per token, or free when the endpoint is local.
//! - **Agent CLI** — spawns the tool the user already subscribes to
//!   (`claude`, `gemini`, `grok`, `codex`, or anything described in
//!   `[llm.cli]`). No API key, no per-token billing. See
//!   [`crate::cli_provider`].
//!
//! Provider/model/api_base loaded from `.recall-echo.toml` config.
//! API keys read from environment variables (never stored in config).

use std::env;
use std::path::Path;

use crate::graph::error::GraphError;
use crate::graph::llm::LlmProvider;

use crate::cli_provider::{CliProvider, CliSpec};
use crate::config::{self, Provider};

// ── Factory ──────────────────────────────────────────────────────────────

/// Create the appropriate LlmProvider from config, with optional CLI overrides.
///
/// Returns the provider and the model name it settled on (empty when the CLI
/// picks its own default).
pub fn create_provider(
    memory_dir: &Path,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<(Box<dyn LlmProvider>, String), crate::error::RecallError> {
    let mut cfg = config::load(memory_dir).llm;

    if let Some(p) = provider_override {
        cfg.provider = Provider::from_str_loose(p)?;
    }
    if let Some(m) = model_override {
        cfg.model = m.to_string();
    }

    if cfg.provider.is_cli() {
        let spec = CliSpec::resolve(&cfg.provider, &cfg.cli)?;
        let model = spec.resolve_model(&cfg.model);
        let provider = CliProvider::new(spec, model.clone());
        return Ok((Box::new(provider), model));
    }

    let config = HttpConfig::from_config_section(&cfg)?;
    let model = config.model.clone();
    let provider = HttpLlmProvider::new(config);
    Ok((Box::new(provider), model))
}

// ── Claude Code provider (subprocess) ────────────────────────────────────

/// LLM provider that shells out to `claude -p` for completions.
/// No API key needed — uses the user's Claude Code subscription.
///
/// A thin front for [`CliProvider`] on the `claude-code` preset, which builds
/// the same argv this type always built.
pub struct ClaudeCodeProvider {
    inner: CliProvider,
}

impl ClaudeCodeProvider {
    #[must_use]
    pub fn new(model: String) -> Self {
        let spec = CliSpec::preset(config::CliPreset::ClaudeCode);
        let model = spec.resolve_model(&model);
        Self {
            inner: CliProvider::new(spec, model),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ClaudeCodeProvider {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError> {
        self.inner
            .complete(system_prompt, user_message, max_tokens)
            .await
    }
}

// ── HTTP providers (Anthropic + OpenAI-compat) ───────────────────────────

/// API protocol style.
#[derive(Debug, Clone)]
pub enum ApiStyle {
    Anthropic,
    OpenAiCompat,
}

/// Resolved configuration for an HTTP LLM provider.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
    pub api_style: ApiStyle,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
}

impl HttpConfig {
    /// Build from a config LlmSection (for Anthropic/OpenAI providers only).
    pub fn from_config_section(
        llm: &config::LlmSection,
    ) -> Result<Self, crate::error::RecallError> {
        let api_style = match &llm.provider {
            Provider::Anthropic => ApiStyle::Anthropic,
            Provider::Openai => ApiStyle::OpenAiCompat,
            other => {
                return Err(crate::error::RecallError::Config(format!(
                    "provider {other} spawns a CLI — use create_provider()"
                )))
            }
        };

        let api_key = env::var("RECALL_LLM_API_KEY")
            .or_else(|_| match &api_style {
                ApiStyle::Anthropic => env::var("ANTHROPIC_API_KEY"),
                ApiStyle::OpenAiCompat => {
                    env::var("OPENAI_API_KEY").or_else(|_| Ok("ollama".into()))
                }
            })
            .map_err(|_| {
                crate::error::RecallError::Config(
                    "No API key found. Set ANTHROPIC_API_KEY or OPENAI_API_KEY in your environment."
                        .into(),
                )
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
    config: HttpConfig,
}

impl HttpLlmProvider {
    pub fn new(config: HttpConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
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
                truncate_str(&text, 300)
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
                truncate_str(&text, 300)
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

// ── Helpers ──────────────────────────────────────────────────────────────

fn truncate_str(text: &str, max: usize) -> &str {
    let end = text.len().min(max);
    let mut i = end;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    &text[..i]
}
