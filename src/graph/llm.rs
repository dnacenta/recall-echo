//! Minimal LLM provider trait for knowledge graph operations.
//!
//! recall-graph defines its own trait to stay independent of echo-system-types.
//! Callers implement this to bridge their actual LLM backend.

use super::error::GraphError;

/// Minimal LLM provider for extraction and deduplication.
///
/// Implementors bridge this to their actual LLM backend:
/// - recall-echo bridges to `echo_system_types::LmProvider`
/// - Standalone users can implement with any HTTP client
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a system prompt and user message, get back a text response.
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, GraphError>;
}
