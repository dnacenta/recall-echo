// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LLM providers implementing crate::graph::LlmProvider.
//!
//! Two families:
//! - **HTTP** — Anthropic (x-api-key), any OpenAI-compatible endpoint, or
//!   Ollama's native chat when the call carries a JSON schema. Billed per
//!   token, or free when the endpoint is local.
//! - **Agent CLI** — spawns the tool the user already subscribes to
//!   (`claude`, `gemini`, `grok`, `codex`, or anything described in
//!   `[llm.cli]`). No API key, no per-token billing. See
//!   [`crate::cli_provider`].
//!
//! Provider/model/api_base loaded from `.recall-echo.toml` config.
//! API keys read from environment variables (never stored in config).

use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::graph::error::GraphError;
use crate::graph::llm::{Completion, LlmProvider, Message, TokenUsage};

use crate::cli_provider::{CliOverrides, CliProvider, CliSpec};
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
    let handle = create_provider_with_binary(memory_dir, provider_override, model_override)?;
    Ok((handle.llm, handle.model))
}

/// A provider plus what it was built from — for the caller that wants to
/// say which binary it is running.
pub struct ProviderHandle {
    pub llm: Box<dyn LlmProvider>,
    /// The model it settled on; empty when the CLI picks its own default.
    pub model: String,
    /// Absolute path of the CLI binary it will spawn; `None` for HTTP.
    pub binary: Option<PathBuf>,
}

/// [`create_provider`], keeping the resolved binary path.
pub fn create_provider_with_binary(
    memory_dir: &Path,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<ProviderHandle, crate::error::RecallError> {
    let mut cfg = config::load(memory_dir).llm;

    if let Some(p) = provider_override {
        cfg.provider = Provider::from_str_loose(p)?;
    }
    if let Some(m) = model_override {
        cfg.model = m.to_string();
    }

    provider_from_section(cfg, &CliOverrides::default())
}

/// The provider `.recall-echo.toml` configures, spawned the way a host asks.
///
/// For a host that already knows where the CLI lives and what it may see —
/// pulse-null carries its own `cli_bin` and an allowlisted child environment
/// — and should not depend on the search, or on its own environment, lining
/// up. [`CliOverrides::command`] takes the place of `[llm.cli] command`, so it
/// wins over the `*_BIN` variable and the search alike. An HTTP provider
/// ignores every override.
pub fn create_provider_with_overrides(
    memory_dir: &Path,
    overrides: &CliOverrides,
) -> Result<ProviderHandle, crate::error::RecallError> {
    let mut cfg = config::load(memory_dir).llm;
    if let Some(command) = &overrides.command {
        let command = command.to_str().ok_or_else(|| {
            crate::error::RecallError::Config(format!(
                "CLI binary path {} is not valid UTF-8",
                command.display()
            ))
        })?;
        cfg.cli.command = Some(command.to_string());
    }
    provider_from_section(cfg, overrides)
}

fn provider_from_section(
    cfg: config::LlmSection,
    overrides: &CliOverrides,
) -> Result<ProviderHandle, crate::error::RecallError> {
    if cfg.provider.is_cli() {
        let mut spec = CliSpec::resolve(&cfg.provider, &cfg.cli)?;
        // Spawn by absolute path. The bare name only works where PATH says so,
        // and the background daemon's PATH is not the user's shell's.
        let binary = spec.locate_command()?;
        spec.use_located_command(&binary)?;
        let model = spec.resolve_model(&cfg.model);
        let provider = CliProvider::new(spec, model.clone())
            .with_env(overrides.env.clone())
            .with_current_dir(overrides.current_dir.clone());
        return Ok(ProviderHandle {
            llm: Box::new(provider),
            model,
            binary: Some(binary),
        });
    }

    let config = HttpConfig::from_config_section(&cfg)?;
    let model = config.model.clone();
    let provider = HttpLlmProvider::new(config);
    Ok(ProviderHandle {
        llm: Box::new(provider),
        model,
        binary: None,
    })
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

    async fn complete_measured(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        self.inner
            .complete_measured(system_prompt, user_message, max_tokens)
            .await
    }

    /// The default method would call this wrapper's `complete_measured` and
    /// drop the schema. The inner CLI is the one that knows the flag.
    async fn complete_structured(
        &self,
        system_prompt: &str,
        user_message: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        self.inner
            .complete_structured(system_prompt, user_message, schema, max_tokens)
            .await
    }
}

// ── HTTP providers (Anthropic + OpenAI-compat) ───────────────────────────

/// API protocol style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStyle {
    Anthropic,
    OpenAiCompat,
    /// Ollama's native `/api/chat`, which can carry a JSON schema and `num_ctx`.
    OllamaNative,
}

/// Resolved configuration for an HTTP LLM provider.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
    /// Style for [`LlmProvider::complete`] and [`LlmProvider::complete_measured`].
    /// Native chat is used here only when `[llm] api_style = "ollama"`.
    pub api_style: ApiStyle,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    /// Configured floor for `options.num_ctx` on the native chat route.
    /// A structured call may raise it to cover the prompt plus the reserved
    /// reply, up to 32768. A higher configured value is kept.
    pub num_ctx: u32,
    /// Style for [`LlmProvider::complete_structured`]. Equal to `api_style`
    /// except when `[llm] api_style` is unset and the base URL's port is
    /// 11434: structured calls then use native chat, and plain calls stay on
    /// `/v1/chat/completions`.
    pub structured_style: ApiStyle,
}

impl HttpConfig {
    /// Build from a config LlmSection (for Anthropic/OpenAI providers only).
    pub fn from_config_section(
        llm: &config::LlmSection,
    ) -> Result<Self, crate::error::RecallError> {
        let model = llm.resolved_model().to_string();
        let api_base = llm.resolved_api_base().to_string();
        let (api_style, structured_style) =
            http_styles(&llm.provider, llm.api_style.as_deref(), &api_base)?;

        let api_key = env::var("RECALL_LLM_API_KEY")
            .or_else(|_| match api_style {
                ApiStyle::Anthropic => env::var("ANTHROPIC_API_KEY"),
                ApiStyle::OpenAiCompat | ApiStyle::OllamaNative => {
                    env::var("OPENAI_API_KEY").or_else(|_| Ok("ollama".into()))
                }
            })
            .map_err(|_| {
                crate::error::RecallError::Config(
                    "No API key found. Set ANTHROPIC_API_KEY or OPENAI_API_KEY in your environment."
                        .into(),
                )
            })?;

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
            num_ctx: llm.num_ctx,
            structured_style,
        })
    }
}

/// HTTP-based LLM provider.
pub struct HttpLlmProvider {
    client: reqwest::Client,
    config: HttpConfig,
    /// Set for the life of this provider after an OpenAI-compatible server
    /// rejects `response_format`. Later structured calls skip the field.
    openai_schema_rejected: AtomicBool,
}

/// Which header authenticates one POST.
enum Auth {
    Anthropic,
    Bearer,
}

/// A plain completion, or one that must match a schema.
#[derive(Clone, Copy)]
enum Requested<'a> {
    Text,
    Structured(&'a serde_json::Value),
}

impl HttpLlmProvider {
    pub fn new(config: HttpConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            openai_schema_rejected: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn for_tests(config: HttpConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        Self {
            client,
            config,
            openai_schema_rejected: AtomicBool::new(false),
        }
    }

    async fn dispatch(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        requested: Requested<'_>,
    ) -> Result<Completion, GraphError> {
        let (style, schema) = match requested {
            Requested::Text => (self.config.api_style, None),
            Requested::Structured(schema) => (self.config.structured_style, Some(schema)),
        };
        match style {
            ApiStyle::Anthropic => {
                self.complete_anthropic(system_prompt, messages, max_tokens, schema)
                    .await
            }
            ApiStyle::OpenAiCompat => {
                self.complete_openai(system_prompt, messages, max_tokens, schema)
                    .await
            }
            ApiStyle::OllamaNative => {
                self.complete_ollama(system_prompt, messages, max_tokens, schema)
                    .await
            }
        }
    }

    async fn with_retries(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        requested: Requested<'_>,
    ) -> Result<Completion, GraphError> {
        let mut last_error = None;

        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.config.retry_delay_ms * u64::from(attempt),
                ))
                .await;
            }

            match self
                .dispatch(system_prompt, messages, max_tokens, requested)
                .await
            {
                Ok(completion) => return Ok(completion),
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

    async fn post_json(
        &self,
        url: &str,
        auth: Auth,
        body: &serde_json::Value,
    ) -> Result<(reqwest::StatusCode, String), GraphError> {
        let request = self
            .client
            .post(url)
            .header("content-type", "application/json");
        let request = match auth {
            Auth::Anthropic => request
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", "2023-06-01"),
            Auth::Bearer => {
                request.header("Authorization", format!("Bearer {}", self.config.api_key))
            }
        };
        let response = request
            .json(body)
            .send()
            .await
            .map_err(|e| GraphError::Llm(format!("request failed: {e}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| GraphError::Llm(format!("read body: {e}")))?;
        Ok((status, text))
    }

    async fn complete_anthropic(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        schema: Option<&serde_json::Value>,
    ) -> Result<Completion, GraphError> {
        let body = anthropic_body(
            &self.config.model,
            system_prompt,
            messages,
            max_tokens,
            schema,
        );
        let (status, text) = self
            .post_json(&self.config.api_base, Auth::Anthropic, &body)
            .await?;
        anthropic_completion(status, &text)
    }

    async fn complete_openai(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        schema: Option<&serde_json::Value>,
    ) -> Result<Completion, GraphError> {
        let url = openai_chat_url(&self.config.api_base);
        // One shot with the schema, then — if this server refuses the field
        // (400 or 422 naming `response_format`) — one shot without it. The
        // refusal is remembered so the next call does not pay for that error
        // again.
        let mut schema = schema.filter(|_| !self.openai_schema_rejected.load(Ordering::Relaxed));
        for _ in 0..2 {
            let body = openai_body(
                &self.config.model,
                system_prompt,
                messages,
                max_tokens,
                schema,
            );
            let (status, text) = self.post_json(&url, Auth::Bearer, &body).await?;
            if schema.is_some() && response_format_was_rejected(status, &text) {
                self.openai_schema_rejected.store(true, Ordering::Relaxed);
                schema = None;
                continue;
            }
            return openai_completion(status, &text);
        }
        Err(GraphError::Llm(
            "openai structured retry did not return".into(),
        ))
    }

    async fn complete_ollama(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        schema: Option<&serde_json::Value>,
    ) -> Result<Completion, GraphError> {
        let num_ctx = match schema {
            Some(_) => effective_num_ctx(
                self.config.num_ctx,
                prompt_chars(system_prompt, messages),
                max_tokens,
            ),
            None => self.config.num_ctx,
        };
        let body = ollama_body(
            &self.config.model,
            system_prompt,
            messages,
            max_tokens,
            num_ctx,
            schema,
        );
        let url = ollama_chat_url(&self.config.api_base);
        let (status, text) = self.post_json(&url, Auth::Bearer, &body).await?;
        let completion = ollama_completion(status, &text)?;
        if schema.is_some() {
            warn_if_prompt_truncated(&completion, num_ctx, max_tokens);
        }
        Ok(completion)
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
        Ok(self
            .complete_measured(system_prompt, user_message, max_tokens)
            .await?
            .text)
    }

    /// Both API styles report their own token counts, so an HTTP call is
    /// always measured — including the retried ones, whose counts are those of
    /// the attempt that succeeded.
    async fn complete_measured(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        let messages = [Message::user(user_message)];
        self.with_retries(system_prompt, &messages, max_tokens, Requested::Text)
            .await
    }

    async fn complete_structured(
        &self,
        system_prompt: &str,
        user_message: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        let messages = [Message::user(user_message)];
        self.with_retries(
            system_prompt,
            &messages,
            max_tokens,
            Requested::Structured(schema),
        )
        .await
    }

    /// Every HTTP style sends the turns. The system prompt stays where that API puts it.
    async fn complete_structured_chat(
        &self,
        system_prompt: &str,
        messages: &[Message],
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        self.with_retries(
            system_prompt,
            messages,
            max_tokens,
            Requested::Structured(schema),
        )
        .await
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Plain style and structured style for one HTTP provider.
///
/// `api_style = "ollama"` selects native chat for every call. Unset, on an
/// OpenAI provider whose base URL uses port 11434, selects native chat for
/// structured calls only. `api_style = "openai"` keeps `/v1/chat/completions`
/// for both, even on that port.
fn http_styles(
    provider: &Provider,
    api_style: Option<&str>,
    api_base: &str,
) -> Result<(ApiStyle, ApiStyle), crate::error::RecallError> {
    let pin = match api_style {
        None => None,
        Some(value) => Some(config::canonical_api_style(value)?),
    };
    let plain = match provider {
        Provider::Anthropic => ApiStyle::Anthropic,
        Provider::Openai => match pin.as_deref() {
            Some("ollama") => ApiStyle::OllamaNative,
            Some("openai") | None => ApiStyle::OpenAiCompat,
            Some(other) => {
                return Err(crate::error::RecallError::Config(format!(
                    "llm.api_style must be \"ollama\" or \"openai\" (got {other})"
                )))
            }
        },
        other => {
            return Err(crate::error::RecallError::Config(format!(
                "provider {other} spawns a CLI — use create_provider()"
            )))
        }
    };
    let structured = match (provider, pin.as_deref()) {
        (Provider::Openai, None) if url_port(api_base) == Some(11434) => ApiStyle::OllamaNative,
        (Provider::Openai, Some("ollama")) => ApiStyle::OllamaNative,
        _ => plain,
    };
    Ok((plain, structured))
}

fn url_port(url: &str) -> Option<u16> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let port = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once("]:")?.1
    } else {
        authority.rsplit_once(':')?.1
    };
    port.parse().ok()
}

/// Configured base with a trailing `/v1` removed, so `http://host:11434/v1`
/// becomes the native origin `http://host:11434`.
fn ollama_native_base(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

fn ollama_chat_url(api_base: &str) -> String {
    format!("{}/api/chat", ollama_native_base(api_base))
}

fn openai_chat_url(api_base: &str) -> String {
    format!("{}/chat/completions", api_base.trim_end_matches('/'))
}

/// A 400 or 422 whose body names `response_format` is this server refusing
/// the schema field. Other statuses and other bodies are returned as-is.
fn response_format_was_rejected(status: reqwest::StatusCode, body: &str) -> bool {
    matches!(status.as_u16(), 400 | 422) && body.contains("response_format")
}

fn turn_json(role: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"role": role, "content": content})
}

fn messages_with_system(system_prompt: &str, messages: &[Message]) -> Vec<serde_json::Value> {
    let mut turns = Vec::with_capacity(messages.len() + 1);
    turns.push(turn_json("system", system_prompt));
    turns.extend(
        messages
            .iter()
            .map(|message| turn_json(&message.role, &message.content)),
    );
    turns
}

fn turn_values(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|message| turn_json(&message.role, &message.content))
        .collect()
}

fn prompt_chars(system_prompt: &str, messages: &[Message]) -> usize {
    messages.iter().fold(system_prompt.len(), |total, message| {
        total.saturating_add(message.content.len())
    })
}

fn openai_body(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    max_tokens: u32,
    schema: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages_with_system(system_prompt, messages),
    });
    if let Some(schema) = schema {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "extraction",
                "schema": schema,
            },
        });
    }
    body
}

fn anthropic_body(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    max_tokens: u32,
    schema: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system_prompt,
        "messages": turn_values(messages),
    });
    if let Some(schema) = schema {
        body["output_config"] = serde_json::json!({
            "format": {
                "type": "json_schema",
                "schema": schema,
            },
        });
    }
    body
}

fn ollama_body(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    max_tokens: u32,
    num_ctx: u32,
    schema: Option<&serde_json::Value>,
) -> serde_json::Value {
    let messages = messages_with_system(system_prompt, messages);
    let options = serde_json::json!({
        "num_ctx": num_ctx,
        "temperature": 0,
        "num_predict": max_tokens,
    });
    match schema {
        Some(schema) => serde_json::json!({
            "model": model,
            "stream": false,
            "format": schema,
            "messages": messages,
            "options": options,
        }),
        None => serde_json::json!({
            "model": model,
            "stream": false,
            "messages": messages,
            "options": options,
        }),
    }
}

fn api_failure(status: reqwest::StatusCode, text: &str) -> GraphError {
    GraphError::Llm(format!("API {status}: {}", truncate_str(text, 300)))
}

fn anthropic_completion(status: reqwest::StatusCode, text: &str) -> Result<Completion, GraphError> {
    if !status.is_success() {
        return Err(api_failure(status, text));
    }
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| GraphError::Llm(format!("parse: {e}")))?;
    let reply = json["content"][0]["text"]
        .as_str()
        .ok_or_else(|| GraphError::Llm("no text in anthropic response".into()))?;
    Ok(Completion::measured(reply, anthropic_usage(&json)))
}

fn openai_completion(status: reqwest::StatusCode, text: &str) -> Result<Completion, GraphError> {
    if !status.is_success() {
        return Err(api_failure(status, text));
    }
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| GraphError::Llm(format!("parse: {e}")))?;
    let reply = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| GraphError::Llm("no text in openai response".into()))?;
    Ok(Completion::measured(reply, openai_usage(&json)))
}

fn ollama_completion(status: reqwest::StatusCode, text: &str) -> Result<Completion, GraphError> {
    if !status.is_success() {
        return Err(api_failure(status, text));
    }
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| GraphError::Llm(format!("parse: {e}")))?;
    let reply = json["message"]["content"]
        .as_str()
        .ok_or_else(|| GraphError::Llm("no text in ollama response".into()))?;
    Ok(Completion::measured(reply, ollama_usage(&json)))
}

/// Anthropic's counts: `usage.input_tokens` / `usage.output_tokens`.
fn anthropic_usage(json: &serde_json::Value) -> Option<TokenUsage> {
    TokenUsage::from_counts(
        json["usage"]["input_tokens"].as_u64(),
        json["usage"]["output_tokens"].as_u64(),
    )
}

/// The OpenAI-compatible counts. Ollama and the other compatible servers use
/// the same two keys; one that omits them is simply estimated.
fn openai_usage(json: &serde_json::Value) -> Option<TokenUsage> {
    TokenUsage::from_counts(
        json["usage"]["prompt_tokens"].as_u64(),
        json["usage"]["completion_tokens"].as_u64(),
    )
}

/// Ollama native chat reports `prompt_eval_count` and `eval_count` on the
/// top-level object, not under `usage`.
fn ollama_usage(json: &serde_json::Value) -> Option<TokenUsage> {
    TokenUsage::from_counts(
        json["prompt_eval_count"].as_u64(),
        json["eval_count"].as_u64(),
    )
}

fn truncate_str(text: &str, max: usize) -> &str {
    let end = text.len().min(max);
    let mut i = end;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    &text[..i]
}

/// Automatic raise of a structured native `num_ctx` stops here.
/// The configured value is a floor and may sit above this.
const NUM_CTX_CAP: u32 = 32_768;

/// Tokens to budget for a prompt: one per three characters, rounded up.
fn estimated_prompt_tokens(prompt_chars: usize) -> u32 {
    u32::try_from(prompt_chars.div_ceil(3)).unwrap_or(u32::MAX)
}

/// `num_ctx` for one structured native call.
///
/// `configured` is a floor. The window is at least the estimated prompt plus
/// the reserved reply, and the automatic raise never exceeds [`NUM_CTX_CAP`].
fn effective_num_ctx(configured: u32, prompt_chars: usize, max_tokens: u32) -> u32 {
    let needed = estimated_prompt_tokens(prompt_chars).saturating_add(max_tokens);
    configured.max(needed.min(NUM_CTX_CAP))
}

/// The prompt filled the window left after reserving the reply.
fn prompt_may_be_truncated(prompt_eval_count: u64, num_ctx: u32, max_tokens: u32) -> bool {
    prompt_eval_count >= u64::from(num_ctx.saturating_sub(max_tokens))
}

fn warn_if_prompt_truncated(completion: &Completion, num_ctx: u32, max_tokens: u32) {
    let Some(usage) = completion.usage else {
        return;
    };
    if prompt_may_be_truncated(usage.input_tokens, num_ctx, max_tokens) {
        eprintln!(
            "recall-echo: prompt may have been truncated (prompt_eval_count {count} >= num_ctx {num_ctx} - max_tokens {max_tokens})",
            count = usage.input_tokens,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both HTTP styles report counts; recording them is what keeps an API
    /// user's bill from being a length heuristic.
    #[test]
    fn the_anthropic_envelope_is_measured() {
        let json = serde_json::json!({
            "content": [{"type": "text", "text": "OK"}],
            "usage": {"input_tokens": 1_200, "output_tokens": 48},
        });
        assert_eq!(
            anthropic_usage(&json).map(TokenUsage::total),
            Some(1_248),
            "{json}"
        );
    }

    #[test]
    fn the_openai_envelope_is_measured() {
        let json = serde_json::json!({
            "choices": [{"message": {"content": "OK"}}],
            "usage": {"prompt_tokens": 90, "completion_tokens": 10, "total_tokens": 100},
        });
        assert_eq!(openai_usage(&json).map(TokenUsage::total), Some(100));
    }

    /// A compatible server that omits the counts is estimated, not guessed at.
    #[test]
    fn an_envelope_without_counts_measures_nothing() {
        let json = serde_json::json!({"choices": [{"message": {"content": "OK"}}]});
        assert_eq!(openai_usage(&json), None);
        assert_eq!(anthropic_usage(&json), None);
    }

    fn schema() -> serde_json::Value {
        serde_json::json!({"type": "object", "additionalProperties": false})
    }

    #[test]
    fn ollama_native_body_has_format_and_num_ctx() {
        let user = [Message::user("user")];
        let body = ollama_body("qwen", "sys", &user, 4096, 24_576, Some(&schema()));
        assert_eq!(body["stream"], false);
        assert_eq!(body["format"], schema());
        assert_eq!(body["options"]["num_ctx"], 24_576);
        assert_eq!(body["options"]["temperature"], 0);
        assert_eq!(body["options"]["num_predict"], 4096);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "user");

        let plain = ollama_body("qwen", "sys", &user, 4096, 24_576, None);
        assert!(plain.get("format").is_none(), "{plain}");
    }

    #[test]
    fn ollama_base_strips_v1() {
        assert_eq!(
            ollama_native_base("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_native_base("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_native_base("http://localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_chat_url("http://localhost:11434/v1"),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn port_11434_selects_native_for_structured() {
        let base = Provider::Openai.default_api_base();
        assert_eq!(base, "http://localhost:11434/v1");
        let (plain, structured) = http_styles(&Provider::Openai, None, base).unwrap();
        assert_eq!(plain, ApiStyle::OpenAiCompat);
        assert_eq!(structured, ApiStyle::OllamaNative);

        let (plain, structured) =
            http_styles(&Provider::Openai, Some("ollama"), "http://example:8080/v1").unwrap();
        assert_eq!(plain, ApiStyle::OllamaNative);
        assert_eq!(structured, ApiStyle::OllamaNative);

        let (plain, structured) = http_styles(&Provider::Openai, Some("openai"), base).unwrap();
        assert_eq!(plain, ApiStyle::OpenAiCompat);
        assert_eq!(structured, ApiStyle::OpenAiCompat);
    }

    #[test]
    fn anthropic_body_has_output_config() {
        let user = [Message::user("user")];
        let body = anthropic_body("claude", "sys", &user, 100, Some(&schema()));
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"], schema());
        assert_eq!(body["system"], "sys");
        assert_eq!(body["max_tokens"], 100);

        let plain = anthropic_body("claude", "sys", &user, 100, None);
        assert!(plain.get("output_config").is_none(), "{plain}");
    }

    #[test]
    fn openai_body_has_response_format() {
        let user = [Message::user("user")];
        let body = openai_body("llama3.2", "sys", &user, 800, Some(&schema()));
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "extraction");
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema());
        assert_eq!(
            body["messages"],
            serde_json::json!([
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "user"},
            ])
        );
    }

    #[test]
    fn plain_openai_body_matches_the_pre_schema_shape() {
        let user = [Message::user("user")];
        let body = openai_body("llama3.2", "sys", &user, 800, None);
        assert_eq!(
            body,
            serde_json::json!({
                "model": "llama3.2",
                "max_tokens": 800,
                "messages": [
                    {"role": "system", "content": "sys"},
                    {"role": "user", "content": "user"},
                ],
            })
        );
    }

    /// The worked example is a real turn on every HTTP style. Ollama's options
    /// and schema field stay where a single-turn call puts them.
    #[test]
    fn chat_bodies_emit_the_turns_in_order() {
        let messages = [
            Message::user("example-user"),
            Message::assistant("example-assistant"),
            Message::user("real-user"),
        ];
        let expected = [
            ("user", "example-user"),
            ("assistant", "example-assistant"),
            ("user", "real-user"),
        ];

        let ollama = ollama_body("qwen", "sys", &messages, 4096, 24_576, Some(&schema()));
        assert_eq!(ollama["format"], schema());
        assert_eq!(ollama["stream"], false);
        assert_eq!(ollama["options"]["num_ctx"], 24_576);
        assert_eq!(ollama["options"]["temperature"], 0);
        assert_eq!(ollama["options"]["num_predict"], 4096);
        assert_eq!(ollama["messages"].as_array().map(Vec::len), Some(4));
        assert_eq!(ollama["messages"][0]["role"], "system");
        assert_eq!(ollama["messages"][0]["content"], "sys");
        assert_roles(&ollama["messages"], 1, &expected);

        let openai = openai_body("llama3.2", "sys", &messages, 800, Some(&schema()));
        assert_eq!(openai["messages"].as_array().map(Vec::len), Some(4));
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][0]["content"], "sys");
        assert_roles(&openai["messages"], 1, &expected);
        assert_eq!(openai["response_format"]["json_schema"]["schema"], schema());

        let anthropic = anthropic_body("claude", "sys", &messages, 100, Some(&schema()));
        assert_eq!(anthropic["system"], "sys");
        assert_eq!(anthropic["messages"].as_array().map(Vec::len), Some(3));
        assert_roles(&anthropic["messages"], 0, &expected);
        assert_eq!(anthropic["output_config"]["format"]["schema"], schema());
    }

    fn assert_roles(messages: &serde_json::Value, start: usize, expected: &[(&str, &str)]) {
        for (offset, (role, content)) in expected.iter().enumerate() {
            assert_eq!(messages[start + offset]["role"], *role);
            assert_eq!(messages[start + offset]["content"], *content);
        }
    }

    #[test]
    fn ollama_usage_is_measured() {
        let json = serde_json::json!({
            "message": {"role": "assistant", "content": "{}"},
            "prompt_eval_count": 5000,
            "eval_count": 12,
            "done_reason": "stop",
        });
        assert_eq!(
            ollama_usage(&json),
            Some(TokenUsage {
                input_tokens: 5000,
                output_tokens: 12,
            })
        );
        assert_eq!(
            ollama_usage(&serde_json::json!({"message": {"content": "{}"}})),
            None
        );
    }

    struct ScriptedReply {
        status: u16,
        body: String,
    }

    struct Captured {
        path: String,
        body: serde_json::Value,
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        let header_end = loop {
            let n = socket.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos;
            }
            if buf.len() > 1_048_576 {
                return None;
            }
        };
        let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let content_length = header.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })?;
        let mut body = buf.split_off(header_end + 4);
        while body.len() < content_length {
            let n = socket.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        if body.len() < content_length {
            return None;
        }
        body.truncate(content_length);
        let path = header
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        Some((path, body))
    }

    async fn scripted_server(
        replies: Vec<ScriptedReply>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Captured>>>) {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests_task = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&requests_task);
                let replies = Arc::clone(&replies);
                tokio::spawn(async move {
                    let Some((path, raw)) = read_request(&mut socket).await else {
                        return;
                    };
                    let body = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
                    requests
                        .lock()
                        .expect("requests")
                        .push(Captured { path, body });
                    let reply =
                        replies
                            .lock()
                            .expect("replies")
                            .pop_front()
                            .unwrap_or(ScriptedReply {
                                status: 500,
                                body: "{}".into(),
                            });
                    let reason = match reply.status {
                        200 => "OK",
                        400 => "Bad Request",
                        _ => "Error",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                        status = reply.status,
                        len = reply.body.len(),
                        body = reply.body,
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        tokio::task::yield_now().await;
        (format!("http://{addr}"), requests)
    }

    fn http_provider(
        api_base: &str,
        api_style: ApiStyle,
        structured_style: ApiStyle,
    ) -> HttpLlmProvider {
        HttpLlmProvider::for_tests(HttpConfig {
            api_key: "test".into(),
            model: "m".into(),
            api_base: api_base.to_string(),
            api_style,
            max_retries: 0,
            retry_delay_ms: 0,
            num_ctx: 16_384,
            structured_style,
        })
    }

    #[tokio::test]
    async fn openai_400_naming_response_format_retries_without_it() {
        let ok = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
        let (base, requests) = scripted_server(vec![
            ScriptedReply {
                status: 400,
                body: r#"{"error":{"message":"response_format is unsupported"}}"#.into(),
            },
            ScriptedReply {
                status: 200,
                body: ok.into(),
            },
            ScriptedReply {
                status: 200,
                body: ok.into(),
            },
        ])
        .await;
        let provider = http_provider(&base, ApiStyle::OpenAiCompat, ApiStyle::OpenAiCompat);
        let completion = provider
            .complete_structured("sys", "user", &schema(), 128)
            .await
            .unwrap();
        assert_eq!(completion.text, r#"{"ok":true}"#);
        {
            let recorded = requests.lock().unwrap();
            assert_eq!(
                recorded.len(),
                2,
                "{:?}",
                recorded.iter().map(|r| &r.path).collect::<Vec<_>>()
            );
            assert_eq!(recorded[0].path, "/chat/completions");
            assert_eq!(recorded[0].body["response_format"]["type"], "json_schema");
            assert_eq!(
                recorded[0].body["response_format"]["json_schema"]["schema"],
                schema()
            );
            assert!(recorded[1].body.get("response_format").is_none());
        }
        provider
            .complete_structured("sys", "user", &schema(), 128)
            .await
            .unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        assert!(recorded[2].body.get("response_format").is_none());
    }

    #[tokio::test]
    async fn openai_400_that_does_not_name_response_format_is_returned() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 400,
            body: r#"{"error":"model not found"}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, ApiStyle::OpenAiCompat, ApiStyle::OpenAiCompat);
        let err = provider
            .complete_structured("sys", "user", &schema(), 32)
            .await
            .expect_err("400");
        assert!(err.to_string().contains("API 400"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn plain_call_on_the_port_split_stays_on_chat_completions() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"choices":[{"message":{"content":"OK"}}]}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, ApiStyle::OpenAiCompat, ApiStyle::OllamaNative);
        let text = provider.complete("sys", "user", 32).await.unwrap();
        assert_eq!(text, "OK");
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded[0].path, "/chat/completions");
        assert!(recorded[0].body.get("response_format").is_none());
        assert!(recorded[0].body.get("format").is_none());
        assert_eq!(recorded[0].body["max_tokens"], 32);
    }

    #[test]
    fn short_prompt_keeps_the_configured_num_ctx() {
        assert_eq!(estimated_prompt_tokens(0), 0);
        assert_eq!(estimated_prompt_tokens(1), 1);
        assert_eq!(estimated_prompt_tokens(3), 1);
        assert_eq!(estimated_prompt_tokens(4), 2);
        assert_eq!(effective_num_ctx(16_384, 20, 128), 16_384);
        assert_eq!(effective_num_ctx(8_192, 20, 128), 8_192);
    }

    /// Structured native `num_ctx` is sized from [`prompt_chars`], which counts
    /// every turn, including the worked example.
    #[test]
    fn prompt_chars_counts_the_example_turns_in_num_ctx() {
        let system = "sys";
        let example_user = "example-user";
        let example_reply = "example-reply-0123456789abcdef";
        let real_user = "real";
        let messages = [
            Message::user(example_user),
            Message::assistant(example_reply),
            Message::user(real_user),
        ];
        let chars = prompt_chars(system, &messages);
        assert_eq!(
            chars,
            system.len() + example_user.len() + example_reply.len() + real_user.len()
        );
        let without_example = prompt_chars(system, &[Message::user(real_user)]);
        let floor = 8;
        let max_tokens = 1;
        assert_eq!(effective_num_ctx(floor, without_example, max_tokens), floor);
        let raised = effective_num_ctx(floor, chars, max_tokens);
        assert!(raised > floor, "{raised}");
        assert_eq!(
            raised,
            estimated_prompt_tokens(chars).saturating_add(max_tokens)
        );
    }

    #[test]
    fn a_full_v2_window_raises_num_ctx_within_the_cap() {
        let chars = 48_000 + crate::graph::v2::pass1::PASS1_SYSTEM.len();
        let effective = effective_num_ctx(16_384, chars, 4_096);
        let estimated = u32::try_from(chars.div_ceil(3)).unwrap();
        let expected = 16_384u32.max(estimated.saturating_add(4_096).min(32_768));
        assert_eq!(effective, expected);
        assert!(effective > 16_384, "{effective}");
        assert!(effective <= 32_768, "{effective}");
    }

    #[test]
    fn an_absurd_prompt_is_capped_and_a_higher_floor_is_kept() {
        let chars = 10_000_000;
        assert_eq!(effective_num_ctx(16_384, chars, 4_096), 32_768);
        assert_eq!(effective_num_ctx(40_000, chars, 4_096), 40_000);
    }

    #[test]
    fn prompt_truncation_warning_fires_at_the_reserved_boundary() {
        let num_ctx = 16_384;
        let max_tokens = 4_096;
        let limit = u64::from(num_ctx - max_tokens);
        assert!(!prompt_may_be_truncated(limit - 1, num_ctx, max_tokens));
        assert!(prompt_may_be_truncated(limit, num_ctx, max_tokens));
        assert!(prompt_may_be_truncated(limit + 50, num_ctx, max_tokens));
        assert!(prompt_may_be_truncated(0, 100, 4_096));
    }

    #[tokio::test]
    async fn ollama_native_structured_round_trip() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"message":{"role":"assistant","content":"{\"facts\":[]}"},"prompt_eval_count":120,"eval_count":4}"#.into(),
        }])
        .await;
        let provider = http_provider(
            &format!("{base}/v1"),
            ApiStyle::OpenAiCompat,
            ApiStyle::OllamaNative,
        );
        let completion = provider
            .complete_structured("sys", "user", &schema(), 64)
            .await
            .unwrap();
        assert_eq!(completion.text, r#"{"facts":[]}"#);
        assert_eq!(
            completion.usage,
            Some(TokenUsage {
                input_tokens: 120,
                output_tokens: 4,
            })
        );
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded[0].path, "/api/chat");
        assert_eq!(recorded[0].body["format"], schema());
        assert_eq!(recorded[0].body["options"]["num_ctx"], 16_384);
        assert_eq!(recorded[0].body["messages"][0]["role"], "system");
        assert_eq!(recorded[0].body["messages"][0]["content"], "sys");
        assert_eq!(recorded[0].body["messages"][1]["role"], "user");
        assert_eq!(recorded[0].body["messages"][1]["content"], "user");
    }

    /// The posted body, not the body builder, carries the four turns in order.
    #[tokio::test]
    async fn posted_chat_body_has_four_messages_in_order() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"message":{"role":"assistant","content":"{}"}}"#.into(),
        }])
        .await;
        let provider = http_provider(
            &format!("{base}/v1"),
            ApiStyle::OpenAiCompat,
            ApiStyle::OllamaNative,
        );
        let messages = [
            Message::user("example-user"),
            Message::assistant("example-reply"),
            Message::user("real-user"),
        ];
        provider
            .complete_structured_chat("sys", &messages, &schema(), 64)
            .await
            .unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].path, "/api/chat");
        let posted = &recorded[0].body["messages"];
        assert_eq!(posted.as_array().map(Vec::len), Some(4));
        assert_eq!(posted[0]["role"], "system");
        assert_eq!(posted[0]["content"], "sys");
        assert_eq!(posted[1]["role"], "user");
        assert_eq!(posted[1]["content"], "example-user");
        assert_eq!(posted[2]["role"], "assistant");
        assert_eq!(posted[2]["content"], "example-reply");
        assert_eq!(posted[3]["role"], "user");
        assert_eq!(posted[3]["content"], "real-user");
        assert_eq!(recorded[0].body["format"], schema());
    }

    #[tokio::test]
    async fn anthropic_structured_round_trip() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"content":[{"type":"text","text":"{\"ok\":true}"}],"usage":{"input_tokens":3,"output_tokens":2}}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, ApiStyle::Anthropic, ApiStyle::Anthropic);
        let completion = provider
            .complete_structured("sys", "hi", &schema(), 32)
            .await
            .unwrap();
        assert_eq!(completion.text, r#"{"ok":true}"#);
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded[0].path, "/");
        assert_eq!(
            recorded[0].body["output_config"]["format"]["type"],
            "json_schema"
        );
        assert_eq!(
            recorded[0].body["output_config"]["format"]["schema"],
            schema()
        );
    }

    #[tokio::test]
    async fn openai_422_naming_response_format_retries_without_it() {
        let ok = r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
        let (base, requests) = scripted_server(vec![
            ScriptedReply {
                status: 422,
                body: r#"{"error":{"message":"response_format is unsupported"}}"#.into(),
            },
            ScriptedReply {
                status: 200,
                body: ok.into(),
            },
        ])
        .await;
        let provider = http_provider(&base, ApiStyle::OpenAiCompat, ApiStyle::OpenAiCompat);
        let completion = provider
            .complete_structured("sys", "user", &schema(), 128)
            .await
            .unwrap();
        assert_eq!(completion.text, r#"{"ok":true}"#);
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].body["response_format"]["type"], "json_schema");
        assert!(recorded[1].body.get("response_format").is_none());
    }

    #[tokio::test]
    async fn openai_422_that_does_not_name_response_format_is_returned() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 422,
            body: r#"{"error":"model not found"}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, ApiStyle::OpenAiCompat, ApiStyle::OpenAiCompat);
        let err = provider
            .complete_structured("sys", "user", &schema(), 32)
            .await
            .expect_err("422");
        assert!(err.to_string().contains("API 422"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
