// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal LLM provider trait for knowledge graph operations.
//!
//! recall-graph defines its own trait to stay independent of pulse-system-types.
//! Callers implement this to bridge their actual LLM backend.

use serde::{Deserialize, Serialize};

use super::error::GraphError;

/// Tokens a provider *reported* for one call.
///
/// Measured, never inferred: this type is only ever built from numbers a
/// provider printed. Where a provider says nothing, the caller estimates and
/// says which it is doing — see [`crate::graph::types::IngestionReport`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt tokens, as the provider counted them.
    pub input_tokens: u64,
    /// Completion tokens, as the provider counted them.
    pub output_tokens: u64,
}

impl TokenUsage {
    /// Usage from a pair of counts, or `None` when neither was reported.
    ///
    /// A provider that reports one side and not the other still measured
    /// something, and half a real number beats a whole invented one.
    #[must_use]
    pub fn from_counts(input_tokens: Option<u64>, output_tokens: Option<u64>) -> Option<Self> {
        match (input_tokens, output_tokens) {
            (None, None) => None,
            (input, output) => Some(Self {
                input_tokens: input.unwrap_or(0),
                output_tokens: output.unwrap_or(0),
            }),
        }
    }

    /// Total tokens billed for the call.
    #[must_use]
    pub fn total(self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// One completion, and what it cost if the provider was willing to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The answer text — exactly what [`LlmProvider::complete`] returns.
    pub text: String,
    /// The provider's own token counts, when it reported any.
    pub usage: Option<TokenUsage>,
}

impl Completion {
    /// An answer from a provider that reported no usage.
    #[must_use]
    pub fn unmeasured(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            usage: None,
        }
    }

    /// An answer with the provider's token counts attached.
    #[must_use]
    pub fn measured(text: impl Into<String>, usage: Option<TokenUsage>) -> Self {
        Self {
            text: text.into(),
            usage,
        }
    }
}

/// One chat turn after the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `user` or `assistant`.
    pub role: String,
    /// The turn text.
    pub content: String,
}

impl Message {
    fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }

    /// A user turn.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    /// An assistant turn.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }
}

/// Last user turn. Earlier turns are a demonstration and are not folded in.
fn last_user_content(messages: &[Message]) -> &str {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.as_str())
        .unwrap_or("")
}

/// Minimal LLM provider for extraction and deduplication.
///
/// Implementors bridge this to their actual LLM backend:
/// - recall-echo bridges to `echo_system_types::LmProvider`
/// - Standalone users can implement with any HTTP client
#[allow(clippy::double_must_use, reason = "async-trait adds #[must_use]")]
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a system prompt and user message, get back a text response.
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError>;

    /// The same call, plus whatever the provider reported about its cost.
    ///
    /// Defaulted to [`LlmProvider::complete`] with no usage, so an existing
    /// implementor keeps working and simply goes on being estimated. Override
    /// it wherever the backend prints real numbers — codex's
    /// `turn.completed.usage`, grok's `usage`, the Anthropic and OpenAI
    /// envelopes.
    async fn complete_measured(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        let text = self
            .complete(system_prompt, user_message, max_tokens)
            .await?;
        Ok(Completion::unmeasured(text))
    }

    /// Ask for a reply whose text is JSON matching `schema`.
    ///
    /// The reply text is JSON that should match `schema`; callers still parse
    /// defensively. The default is [`LlmProvider::complete_measured`], which
    /// sends the prompt and does not constrain the backend, so an implementor
    /// that only knows [`LlmProvider::complete`] keeps compiling and answering.
    async fn complete_structured(
        &self,
        system_prompt: &str,
        user_message: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        let _ = schema;
        self.complete_measured(system_prompt, user_message, max_tokens)
            .await
    }

    /// JSON matching `schema`, with chat turns after the system prompt.
    ///
    /// The default keeps the last user turn and calls
    /// [`LlmProvider::complete_structured`], so a provider without chat sends
    /// the same request it sends today.
    async fn complete_structured_chat(
        &self,
        system_prompt: &str,
        messages: &[Message],
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, GraphError> {
        let user_message = last_user_content(messages);
        self.complete_structured(system_prompt, user_message, schema, max_tokens)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_that_reports_nothing_measures_nothing() {
        assert_eq!(TokenUsage::from_counts(None, None), None);
    }

    #[test]
    fn one_reported_side_is_still_a_measurement() {
        assert_eq!(
            TokenUsage::from_counts(None, Some(5)),
            Some(TokenUsage {
                input_tokens: 0,
                output_tokens: 5,
            })
        );
    }

    #[test]
    fn total_sums_both_sides() {
        let usage = TokenUsage::from_counts(Some(13_658), Some(5)).unwrap();
        assert_eq!(usage.total(), 13_663);
    }

    /// The default method is what an existing implementor runs. It must answer
    /// from `complete` and report no usage of its own.
    #[tokio::test]
    async fn only_complete_still_answers_complete_structured() {
        struct OnlyComplete;

        #[async_trait::async_trait]
        impl LlmProvider for OnlyComplete {
            async fn complete(
                &self,
                _system_prompt: &str,
                user_message: &str,
                _max_tokens: u32,
            ) -> Result<String, GraphError> {
                Ok(format!("echo:{user_message}"))
            }
        }

        let completion = OnlyComplete
            .complete_structured(
                "system",
                "hello",
                &serde_json::json!({"type": "object"}),
                32,
            )
            .await
            .unwrap();
        assert_eq!(completion.text, "echo:hello");
        assert_eq!(completion.usage, None);
    }
}
