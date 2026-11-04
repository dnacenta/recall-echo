// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LLM providers implementing crate::graph::LlmProvider.
//!
//! Two families:
//! - **HTTP** — Anthropic (`x-api-key`) or any OpenAI-compatible endpoint.
//!   Billed per token. The OpenAI-compatible one is opt-in: `api_base` and
//!   `OPENAI_API_KEY` are both required, and there is no default endpoint.
//! - **Agent CLI** — spawns the tool the user already subscribes to
//!   (`claude`, `gemini`, `grok`, `codex`, or anything described in
//!   `[llm.cli]`). No API key, no per-token billing. See
//!   [`crate::cli_provider`]. The default provider is one of these.
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
    if let Some(err) = config::legacy_llm_refusal(memory_dir) {
        return Err(err);
    }
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
    if let Some(err) = config::legacy_llm_refusal(memory_dir) {
        return Err(err);
    }
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

/// Resolved configuration for an HTTP LLM provider.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
    /// [`Provider::Anthropic`] or [`Provider::Openai`]. Anything else is a CLI.
    pub provider: Provider,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
}

impl HttpConfig {
    /// Build from a config LlmSection (for Anthropic/OpenAI providers only).
    pub fn from_config_section(
        llm: &config::LlmSection,
    ) -> Result<Self, crate::error::RecallError> {
        if !matches!(llm.provider, Provider::Anthropic | Provider::Openai) {
            return Err(crate::error::RecallError::Config(format!(
                "provider {} spawns a CLI — use create_provider()",
                llm.provider
            )));
        }
        let model = llm.resolved_model().to_string();
        let api_base = llm.resolved_api_base().to_string();
        if llm.provider == Provider::Openai && api_base.is_empty() {
            return Err(crate::error::RecallError::Config(
                "llm.api_base is required for the openai provider".into(),
            ));
        }
        if llm.provider == Provider::Openai && model.is_empty() {
            return Err(crate::error::RecallError::Config(
                "llm.model is required for the openai provider".into(),
            ));
        }
        let api_key = api_key_for(&llm.provider)?;

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
            provider: llm.provider.clone(),
            max_retries,
            retry_delay_ms,
        })
    }
}

/// `RECALL_LLM_API_KEY` overrides the provider key when it is non-empty.
/// OpenAI-compatible HTTP does not invent a key when `OPENAI_API_KEY` is absent.
fn api_key_for(provider: &Provider) -> Result<String, crate::error::RecallError> {
    if let Some(key) = non_empty_env("RECALL_LLM_API_KEY") {
        return Ok(key);
    }
    let (name, key) = match provider {
        Provider::Anthropic => ("ANTHROPIC_API_KEY", non_empty_env("ANTHROPIC_API_KEY")),
        Provider::Openai => ("OPENAI_API_KEY", non_empty_env("OPENAI_API_KEY")),
        other => {
            return Err(crate::error::RecallError::Config(format!(
                "provider {other} spawns a CLI — use create_provider()"
            )))
        }
    };
    key.ok_or_else(|| {
        crate::error::RecallError::Config(format!(
            "No API key found. Set {name} in your environment."
        ))
    })
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
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
        let schema = match requested {
            Requested::Text => None,
            Requested::Structured(schema) => Some(schema),
        };
        match self.config.provider {
            Provider::Anthropic => {
                self.complete_anthropic(system_prompt, messages, max_tokens, schema)
                    .await
            }
            Provider::Openai => {
                self.complete_openai(system_prompt, messages, max_tokens, schema)
                    .await
            }
            _ => Err(GraphError::Llm(format!(
                "provider {} is not an HTTP provider",
                self.config.provider
            ))),
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

/// Anthropic's counts: `usage.input_tokens` / `usage.output_tokens`.
fn anthropic_usage(json: &serde_json::Value) -> Option<TokenUsage> {
    TokenUsage::from_counts(
        json["usage"]["input_tokens"].as_u64(),
        json["usage"]["output_tokens"].as_u64(),
    )
}

/// The OpenAI-compatible counts. A server that omits them is estimated.
fn openai_usage(json: &serde_json::Value) -> Option<TokenUsage> {
    TokenUsage::from_counts(
        json["usage"]["prompt_tokens"].as_u64(),
        json["usage"]["completion_tokens"].as_u64(),
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
        let body = openai_body("m", "sys", &user, 800, Some(&schema()));
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
        let body = openai_body("m", "sys", &user, 800, None);
        assert_eq!(
            body,
            serde_json::json!({
                "model": "m",
                "max_tokens": 800,
                "messages": [
                    {"role": "system", "content": "sys"},
                    {"role": "user", "content": "user"},
                ],
            })
        );
    }

    /// The worked example is a real turn on both HTTP styles.
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

        let openai = openai_body("m", "sys", &messages, 800, Some(&schema()));
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

    fn http_provider(api_base: &str, provider: Provider) -> HttpLlmProvider {
        HttpLlmProvider::for_tests(HttpConfig {
            api_key: "test".into(),
            model: "m".into(),
            api_base: api_base.to_string(),
            provider,
            max_retries: 0,
            retry_delay_ms: 0,
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
        let provider = http_provider(&base, Provider::Openai);
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
        let provider = http_provider(&base, Provider::Openai);
        let err = provider
            .complete_structured("sys", "user", &schema(), 32)
            .await
            .expect_err("400");
        assert!(err.to_string().contains("API 400"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_plain_openai_call_posts_chat_completions() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"choices":[{"message":{"content":"OK"}}]}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, Provider::Openai);
        let text = provider.complete("sys", "user", 32).await.unwrap();
        assert_eq!(text, "OK");
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded[0].path, "/chat/completions");
        assert!(recorded[0].body.get("response_format").is_none());
        assert_eq!(recorded[0].body["max_tokens"], 32);
    }

    /// The posted body, not the body builder, carries the four turns in order.
    #[tokio::test]
    async fn posted_chat_body_has_four_messages_in_order() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"choices":[{"message":{"content":"{}"}}]}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, Provider::Openai);
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
        assert_eq!(recorded[0].path, "/chat/completions");
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
        assert_eq!(
            recorded[0].body["response_format"]["json_schema"]["schema"],
            schema()
        );
    }

    #[tokio::test]
    async fn anthropic_structured_round_trip() {
        let (base, requests) = scripted_server(vec![ScriptedReply {
            status: 200,
            body: r#"{"content":[{"type":"text","text":"{\"ok\":true}"}],"usage":{"input_tokens":3,"output_tokens":2}}"#.into(),
        }])
        .await;
        let provider = http_provider(&base, Provider::Anthropic);
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
        let provider = http_provider(&base, Provider::Openai);
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
        let provider = http_provider(&base, Provider::Openai);
        let err = provider
            .complete_structured("sys", "user", &schema(), 32)
            .await
            .expect_err("422");
        assert!(err.to_string().contains("API 422"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    /// Restores the keys this test clears. Dropped while the env lock is held.
    struct KeyRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Drop for KeyRestore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn openai_section(api_base: &str, model: &str) -> config::LlmSection {
        config::LlmSection {
            provider: Provider::Openai,
            api_base: api_base.into(),
            model: model.into(),
            ..config::LlmSection::default()
        }
    }

    #[test]
    fn openai_without_a_model_is_refused() {
        let err = HttpConfig::from_config_section(&openai_section("https://example.test/v1", ""))
            .unwrap_err();
        assert!(err.to_string().contains("llm.model"), "{err}");
        assert!(Provider::Openai.default_model().is_empty());
    }

    #[test]
    fn openai_requires_an_endpoint_and_an_api_key() {
        let _lock = crate::test_env::lock();
        let saved: Vec<_> = ["OPENAI_API_KEY", "RECALL_LLM_API_KEY"]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        let _restore = KeyRestore(saved);
        let missing_base = HttpConfig::from_config_section(&openai_section("", "m")).unwrap_err();
        assert!(
            missing_base.to_string().contains("api_base"),
            "{missing_base}"
        );
        assert!(Provider::Openai.default_api_base().is_empty());
        let missing_key =
            HttpConfig::from_config_section(&openai_section("https://example.test/v1", "m"))
                .unwrap_err();
        assert!(
            missing_key.to_string().contains("OPENAI_API_KEY"),
            "{missing_key}"
        );
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        let built =
            HttpConfig::from_config_section(&openai_section("https://example.test/v1", "m"))
                .unwrap();
        assert_eq!(built.api_key, "sk-test");
        assert_eq!(built.model, "m");
    }
}
