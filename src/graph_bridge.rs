//! Bridge between recall-echo and recall-graph.
//!
//! Provides graph ingestion for archived conversations.
//! When pulse-null feature is enabled, also bridges LmProvider → LlmProvider.

/// Ingest a conversation archive into the knowledge graph.
///
/// Non-blocking: logs warnings on failure but never fails the caller.
/// Returns the ingestion report on success.
pub async fn ingest_into_graph(
    memory_dir: &std::path::Path,
    archive_content: &str,
    session_id: &str,
    log_number: Option<u32>,
) -> Result<crate::graph::types::IngestionReport, crate::error::RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(crate::error::RecallError::NotInitialized(
            "graph/ not initialized \u{2014} run `graph init` first".into(),
        ));
    }

    let gm = crate::graph::GraphMemory::open(&graph_dir).await?;

    // No LLM provider in standalone mode — episodes only, no entity extraction
    let report = gm
        .ingest_archive(archive_content, session_id, log_number, None)
        .await?;

    eprintln!(
        "recall-echo: graph ingested \u{2014} {} episodes, {} entities created, {} merged, {} skipped, {} relationships",
        report.episodes_created,
        report.entities_created,
        report.entities_merged,
        report.entities_skipped,
        report.relationships_created,
    );

    if !report.errors.is_empty() {
        eprintln!(
            "recall-echo: graph ingestion had {} warnings",
            report.errors.len()
        );
    }

    Ok(report)
}

/// Ingest with an LLM provider for entity extraction.
///
/// When pulse-null feature is enabled, this bridges the LmProvider
/// to recall-graph's LlmProvider for full entity/relationship extraction.
#[cfg(feature = "pulse-null")]
pub async fn ingest_into_graph_with_llm(
    memory_dir: &std::path::Path,
    archive_content: &str,
    session_id: &str,
    log_number: Option<u32>,
    provider: Option<&dyn pulse_system_types::llm::LmProvider>,
) -> Result<crate::graph::types::IngestionReport, crate::error::RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(crate::error::RecallError::NotInitialized(
            "graph/ not initialized \u{2014} run `graph init` first".into(),
        ));
    }

    let gm = crate::graph::GraphMemory::open(&graph_dir).await?;

    let bridge = provider.map(GraphLlmBridge::new);
    let llm_ref: Option<&dyn crate::graph::llm::LlmProvider> = bridge
        .as_ref()
        .map(|b| b as &dyn crate::graph::llm::LlmProvider);

    let report = gm
        .ingest_archive(archive_content, session_id, log_number, llm_ref)
        .await?;

    eprintln!(
        "recall-echo: graph ingested \u{2014} {} episodes, {} entities created, {} merged, {} skipped, {} relationships",
        report.episodes_created,
        report.entities_created,
        report.entities_merged,
        report.entities_skipped,
        report.relationships_created,
    );

    if !report.errors.is_empty() {
        eprintln!(
            "recall-echo: graph ingestion had {} warnings",
            report.errors.len()
        );
    }

    Ok(report)
}

/// Adapter that wraps an `pulse_system_types::LmProvider` to implement
/// `crate::graph::LlmProvider`.
#[cfg(feature = "pulse-null")]
pub struct GraphLlmBridge<'a> {
    provider: &'a dyn pulse_system_types::llm::LmProvider,
}

#[cfg(feature = "pulse-null")]
impl<'a> GraphLlmBridge<'a> {
    pub fn new(provider: &'a dyn pulse_system_types::llm::LmProvider) -> Self {
        Self { provider }
    }
}

#[cfg(feature = "pulse-null")]
#[async_trait::async_trait]
impl crate::graph::llm::LlmProvider for GraphLlmBridge<'_> {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, crate::graph::error::GraphError> {
        use pulse_system_types::llm::{Message, MessageContent, Role};

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(user_message.to_string()),
            source: None,
        }];

        let response = self
            .provider
            .invoke(system_prompt, &messages, max_tokens, None)
            .await
            .map_err(|e| crate::graph::error::GraphError::Llm(e.to_string()))?;

        Ok(response.text())
    }
}
