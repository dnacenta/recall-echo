//! Ingestion orchestrator — chunk → episode → extract → dedup → relationships.

use std::collections::HashMap;

use futures::stream::{self, StreamExt};

use super::confidence::{bayesian_update, ExtractionContext};
use super::crud;
use super::dedup::{self, ResolvedEntity};
use super::error::GraphError;
use super::extract;
use super::llm::LlmProvider;
use super::types::*;
use super::GraphMemory;

/// Maximum number of concurrent LLM calls during extraction and dedup.
const LLM_CONCURRENCY: usize = 10;

/// Ingest a conversation archive into the knowledge graph.
///
/// Flow:
/// 1. Chunk the conversation text
/// 2. Create an Episode for each chunk (always, even without LLM)
/// 3. If LLM provided: extract entities/relationships, dedup, store
/// 4. Return a report of what was created/merged/skipped
pub async fn ingest_archive(
    gm: &GraphMemory,
    archive_text: &str,
    session_id: &str,
    log_number: Option<u32>,
    llm: Option<&dyn LlmProvider>,
) -> Result<IngestionReport, GraphError> {
    let mut report = IngestionReport::default();

    let chunks = extract::chunk_conversation(archive_text, 500);
    if chunks.is_empty() {
        return Ok(report);
    }

    // Create episodes for each chunk
    for (i, chunk) in chunks.iter().enumerate() {
        let abstract_text = build_episode_abstract(chunk);
        let episode = NewEpisode {
            session_id: session_id.to_string(),
            abstract_text,
            overview: None,
            content: Some(chunk.clone()),
            log_number,
        };

        match gm.add_episode(episode).await {
            Ok(_) => report.episodes_created += 1,
            Err(e) => {
                report.errors.push(format!("episode chunk {i}: {e}"));
            }
        }
    }

    // If LLM provided, run extraction on all chunks
    if let Some(llm) = llm {
        process_extraction(gm, &chunks, session_id, log_number, llm, &mut report).await?;
    }

    Ok(report)
}

/// Run LLM extraction on an archive text without creating episodes.
///
/// Use this when episodes already exist (e.g., backfill extraction on
/// previously-ingested archives).
pub async fn extract_from_archive(
    gm: &GraphMemory,
    archive_text: &str,
    session_id: &str,
    log_number: Option<u32>,
    llm: &dyn LlmProvider,
) -> Result<IngestionReport, GraphError> {
    let mut report = IngestionReport::default();

    let chunks = extract::chunk_conversation(archive_text, 500);
    if chunks.is_empty() {
        return Ok(report);
    }

    process_extraction(gm, &chunks, session_id, log_number, llm, &mut report).await?;

    Ok(report)
}

/// Shared extraction logic — parallel extraction, sequential dedup.
///
/// Four phases:
/// 1. Extract all chunks in parallel (up to LLM_CONCURRENCY)
/// 2. Local pre-dedup: merge same-name entities from different chunks
/// 3. Dedup sequentially against the DB (each call sees prior results)
/// 4. Create relationships sequentially (fast, no LLM)
async fn process_extraction(
    gm: &GraphMemory,
    chunks: &[String],
    session_id: &str,
    log_number: Option<u32>,
    llm: &dyn LlmProvider,
    report: &mut IngestionReport,
) -> Result<(), GraphError> {
    // Phase 1: Extract all chunks in parallel
    let extraction_results: Vec<(usize, Result<ExtractionResult, GraphError>)> =
        stream::iter(chunks.iter().enumerate())
            .map(|(i, chunk)| async move {
                let result = extract::extract_from_chunk(llm, chunk, session_id, log_number).await;
                (i, result)
            })
            .buffer_unordered(LLM_CONCURRENCY)
            .collect()
            .await;

    // Collect entities and relationships from successful extractions
    let mut all_entities: Vec<ExtractedEntity> = Vec::new();
    let mut all_relationships: Vec<ExtractedRelationship> = Vec::new();

    for (i, result) in extraction_results {
        match result {
            Ok(extraction) => {
                all_entities.extend(extract::flatten_extraction(&extraction));
                all_relationships.extend(extraction.relationships);
                // Estimate ~2500 tokens per extracted chunk (system prompt + chunk input + output)
                report.estimated_tokens += 2500;
            }
            Err(e) => {
                report.errors.push(format!("extraction chunk {i}: {e}"));
            }
        }
    }

    // Phase 2: Local pre-dedup — merge same-name entities before hitting the DB
    let deduplicated = local_merge_entities(all_entities);

    // Phase 3: Dedup sequentially — each resolve_entity sees the full DB state
    let mut name_map: HashMap<String, String> = HashMap::new();

    for candidate in &deduplicated {
        // Estimate ~600 tokens per dedup call (vector search + LLM decision)
        report.estimated_tokens += 600;
        match dedup::resolve_entity(gm, llm, candidate, session_id).await {
            Ok(ResolvedEntity::Created(entity)) => {
                name_map.insert(candidate.name.clone(), entity.name.clone());
                report.entities_created += 1;
            }
            Ok(ResolvedEntity::Merged(entity)) => {
                name_map.insert(candidate.name.clone(), entity.name.clone());
                report.entities_merged += 1;
            }
            Ok(ResolvedEntity::Skipped) => {
                name_map.insert(candidate.name.clone(), candidate.name.clone());
                report.entities_skipped += 1;
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("dedup '{}': {}", candidate.name, e));
            }
        }
    }

    // Phase 4: Create relationships or Bayesian-update existing ones
    for rel in &all_relationships {
        let from_name = name_map.get(&rel.source).unwrap_or(&rel.source);
        let to_name = name_map.get(&rel.target).unwrap_or(&rel.target);

        // Check if a relationship of the same type already exists
        if let Some(existing) =
            find_existing_relationship(gm, from_name, to_name, &rel.rel_type).await
        {
            // Re-extraction is corroborating evidence — Bayesian update + reset decay clock
            let updated = bayesian_update(existing.confidence, true);
            if let Err(e) =
                crud::reinforce_relationship(gm.db(), &existing.id_string(), updated).await
            {
                report
                    .errors
                    .push(format!("confidence update {from_name} -> {to_name}: {e}"));
            }
            report.relationships_skipped += 1;
            continue;
        }

        // Parse extraction context from LLM output, default to Inferred
        let context: ExtractionContext = rel
            .confidence
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(ExtractionContext::Inferred);

        let new_rel = NewRelationship {
            from_entity: from_name.clone(),
            to_entity: to_name.clone(),
            rel_type: rel.rel_type.clone(),
            description: rel.description.clone(),
            confidence: Some(context.prior() as f32),
            source: Some(session_id.to_string()),
        };

        match gm.add_relationship(new_rel).await {
            Ok(_) => report.relationships_created += 1,
            Err(e) => {
                report
                    .errors
                    .push(format!("relationship {from_name} -> {to_name}: {e}"));
            }
        }
    }

    Ok(())
}

/// Merge extracted entities that share the same name (case-insensitive).
///
/// When multiple chunks extract the same entity, combine their data:
/// - Keep the longest abstract_text
/// - Concatenate overviews
/// - Concatenate content
/// - Deep-merge attributes (later wins on conflict)
/// - First occurrence's entity_type wins
fn local_merge_entities(entities: Vec<ExtractedEntity>) -> Vec<ExtractedEntity> {
    let mut seen: HashMap<String, ExtractedEntity> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for entity in entities {
        let key = entity.name.to_lowercase();
        if let Some(existing) = seen.get_mut(&key) {
            // Keep longer abstract
            if entity.abstract_text.len() > existing.abstract_text.len() {
                existing.abstract_text = entity.abstract_text;
            }
            // Concatenate overviews
            if let Some(new_overview) = entity.overview {
                existing.overview = Some(match &existing.overview {
                    Some(o) => format!("{o}\n\n{new_overview}"),
                    None => new_overview,
                });
            }
            // Concatenate content
            if let Some(new_content) = entity.content {
                existing.content = Some(match &existing.content {
                    Some(c) => format!("{c}\n\n{new_content}"),
                    None => new_content,
                });
            }
            // Merge attributes
            if let Some(new_attrs) = entity.attributes {
                existing.attributes = Some(match &existing.attributes {
                    Some(a) => merge_json(a, &new_attrs),
                    None => new_attrs,
                });
            }
        } else {
            order.push(key.clone());
            seen.insert(key, entity);
        }
    }

    // Preserve insertion order
    order.into_iter().filter_map(|k| seen.remove(&k)).collect()
}

use super::util::merge_json_objects as merge_json;

/// Build a short abstract for an episode chunk.
fn build_episode_abstract(chunk: &str) -> String {
    let chars: String = chunk.chars().take(200).collect();
    if chars.len() < chunk.len() {
        format!("{}...", chars.trim())
    } else {
        chars.trim().to_string()
    }
}

/// Find an existing relationship of the same type between two entities.
/// Returns the full Relationship if found (for Bayesian update).
async fn find_existing_relationship(
    gm: &GraphMemory,
    from_name: &str,
    to_name: &str,
    rel_type: &str,
) -> Option<Relationship> {
    let rels = gm
        .get_relationships(from_name, Direction::Outgoing)
        .await
        .ok()?;
    let to_entity = gm.get_entity(to_name).await.ok()??;
    let to_id = to_entity.id_string();

    rels.into_iter().find(|r| {
        r.rel_type == rel_type && {
            let out_id = match &r.to_id {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out_id == to_id
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_abstract_truncates() {
        let long = "x".repeat(500);
        let abs = build_episode_abstract(&long);
        assert!(abs.len() < 210);
        assert!(abs.ends_with("..."));
    }

    #[test]
    fn episode_abstract_short_unchanged() {
        let short = "Hello world";
        let abs = build_episode_abstract(short);
        assert_eq!(abs, "Hello world");
    }
}
