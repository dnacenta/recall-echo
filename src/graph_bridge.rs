// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

    // Hot path (SessionEnd hook): goes through the serve daemon so a
    // concurrent session never collides on the embedded store lock.
    // No LLM provider in standalone mode — episodes only, no entity extraction.
    // Provenance is left to per-chunk turn-role inference: this is a
    // conversation archive, the one place where authorship is visible.
    let request = crate::serve::Request::IngestArchive(crate::serve::IngestArchiveArgs {
        content: archive_content.to_string(),
        session_id: session_id.to_string(),
        log_number,
        provenance: None,
    });
    let report: crate::graph::types::IngestionReport =
        serde_json::from_value(crate::serve_client::execute(memory_dir, &request).await?)?;

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

/// Sync the pipeline documents into the knowledge graph.
///
/// Pipeline sync needs no LLM provider, so it runs as an ordinary daemon
/// request. That matters on the SessionEnd hook path: ingest and sync then
/// share one warm daemon instead of the sync stopping the daemon the ingest
/// just started and reloading the embedding model in-process.
pub async fn sync_pipeline_into_graph(
    memory_dir: &std::path::Path,
    docs: crate::graph::types::PipelineDocuments,
) -> Result<crate::graph::types::PipelineSyncReport, crate::error::RecallError> {
    let request = crate::serve::Request::SyncPipeline(crate::serve::SyncPipelineArgs { docs });
    Ok(serde_json::from_value(
        crate::serve_client::execute(memory_dir, &request).await?,
    )?)
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

    // LLM-backed extraction cannot cross the socket (the provider lives in
    // this process), so this path takes the store exclusively instead.
    let context = crate::graph::IngestContext::new(session_id, log_number);
    let report = crate::serve_client::exclusive(memory_dir, |gm| async move {
        let bridge = provider.map(GraphLlmBridge::new);
        let llm_ref: Option<&dyn crate::graph::llm::LlmProvider> = bridge
            .as_ref()
            .map(|b| b as &dyn crate::graph::llm::LlmProvider);

        Ok(gm
            .ingest_archive(archive_content, &context, llm_ref)
            .await?)
    })
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
