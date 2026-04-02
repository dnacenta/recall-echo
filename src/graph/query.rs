//! Hybrid query — combines semantic search, graph expansion, and episode search.
//!
//! Pipeline:
//! 1. **Semantic phase**: HNSW KNN with `limit * 2` to gather candidates
//! 2. **Graph phase**: 1-hop expansion from top-N results, scored as `parent_score * 0.5`
//! 3. **Merge + deduplicate** by entity ID, keeping highest score
//! 4. **Episode search** (optional) — separate KNN on episodes

use std::collections::HashMap;

use surrealdb::Surreal;

use super::confidence::{effective_confidence, DecayConfig};
use super::embed::Embedder;
use super::error::GraphError;
use super::store::Db;
use super::types::*;

/// Run a hybrid query: semantic search + graph expansion + optional episode search.
pub async fn query(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    query_text: &str,
    options: &QueryOptions,
) -> Result<QueryResult, GraphError> {
    let limit = if options.limit == 0 {
        10
    } else {
        options.limit
    };

    // Phase 1: Semantic search with 2x limit to get candidates
    let semantic_options = SearchOptions {
        limit: limit * 2,
        entity_type: options.entity_type.clone(),
        keyword: options.keyword.clone(),
    };
    let semantic_results =
        super::search::search_with_options(db, embedder, query_text, &semantic_options).await?;

    // Collect into dedup map (id -> ScoredEntity)
    let mut entity_map: HashMap<String, ScoredEntity> = HashMap::new();
    for result in semantic_results {
        entity_map.insert(result.entity.id_string(), result);
    }

    // Phase 2: Graph expansion — 1-hop from top-N semantic results
    if options.graph_depth > 0 {
        let top_n: Vec<(String, f64)> = {
            let mut entries: Vec<_> = entity_map
                .values()
                .map(|e| (e.entity.id_string(), e.score))
                .collect();
            entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            entries.truncate(3); // Expand from top 3
            entries
        };

        for (parent_id, parent_score) in &top_n {
            let parent_name = entity_map
                .get(parent_id)
                .map(|e| e.entity.name.clone())
                .unwrap_or_default();

            let neighbors = get_neighbor_details(db, parent_id).await?;

            for (neighbor, rel_type, confidence) in neighbors {
                let neighbor_id = neighbor.id_string();
                if entity_map.contains_key(&neighbor_id) {
                    continue; // Already in results
                }

                // Apply type filter
                if let Some(ref et) = options.entity_type {
                    if neighbor.entity_type.to_string() != *et {
                        continue;
                    }
                }

                let graph_score = parent_score * confidence;
                entity_map.insert(
                    neighbor_id,
                    ScoredEntity {
                        entity: neighbor,
                        score: graph_score,
                        source: MatchSource::Graph {
                            parent: parent_name.clone(),
                            rel_type,
                        },
                    },
                );
            }
        }
    }

    // Sort by score descending, truncate to limit
    let mut entities: Vec<ScoredEntity> = entity_map.into_values().collect();
    entities.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entities.truncate(limit);

    // Phase 3: Episode search (optional)
    let episodes = if options.include_episodes {
        super::search::search_episodes(db, embedder, query_text, limit).await?
    } else {
        vec![]
    };

    Ok(QueryResult { entities, episodes })
}

/// Get 1-hop neighbors as L1 (EntityDetail) with the relationship type and effective confidence.
/// Applies temporal decay to relationship confidence before returning.
async fn get_neighbor_details(
    db: &Surreal<Db>,
    entity_id: &str,
) -> Result<Vec<(EntityDetail, String, f64)>, GraphError> {
    let decay_config = DecayConfig::default();

    // Outgoing
    let mut response = db
        .query(
            r#"
            SELECT rel_type, confidence, last_reinforced, valid_from, out AS target_id
            FROM relates_to
            WHERE in = type::record($id) AND valid_until IS NONE
            "#,
        )
        .bind(("id", entity_id.to_string()))
        .await?;

    let outgoing: Vec<RelTarget> = super::deserialize_take(&mut response, 0)?;

    // Incoming
    let mut response = db
        .query(
            r#"
            SELECT rel_type, confidence, last_reinforced, valid_from, in AS target_id
            FROM relates_to
            WHERE out = type::record($id) AND valid_until IS NONE
            "#,
        )
        .bind(("id", entity_id.to_string()))
        .await?;

    let incoming: Vec<RelTarget> = super::deserialize_take(&mut response, 0)?;

    let mut results = Vec::new();
    let all_edges: Vec<_> = outgoing.into_iter().chain(incoming).collect();

    for edge in all_edges {
        // Compute effective (decayed) confidence
        let eff = effective_confidence(
            edge.confidence,
            edge.last_reinforced.as_ref(),
            &edge.valid_from,
            &decay_config,
        );

        // Filter by effective confidence
        if eff < 0.1 {
            continue;
        }

        let tid = match &edge.target_id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        if let Some(detail) = super::crud::get_entity_detail(db, &tid).await? {
            results.push((detail, edge.rel_type, eff));
        }
    }

    Ok(results)
}

fn default_rel_confidence() -> f64 {
    1.0
}

#[derive(serde::Deserialize)]
struct RelTarget {
    rel_type: String,
    target_id: serde_json::Value,
    #[serde(default = "default_rel_confidence")]
    confidence: f64,
    #[serde(default)]
    last_reinforced: Option<serde_json::Value>,
    #[serde(default)]
    valid_from: serde_json::Value,
}

// ── Pipeline queries ─────────────────────────────────────────────────

/// Get all pipeline entities for a given stage, optionally filtered by status.
pub async fn pipeline_entities(
    db: &Surreal<Db>,
    stage: &str,
    status: Option<&str>,
) -> Result<Vec<EntityDetail>, GraphError> {
    let query = match status {
        Some(_) => {
            r#"SELECT id, name, entity_type, abstract, overview, attributes, access_count, updated_at, source
               FROM entity
               WHERE attributes.pipeline_stage = $stage
                 AND attributes.pipeline_status = $status
               ORDER BY updated_at DESC"#
        }
        None => {
            r#"SELECT id, name, entity_type, abstract, overview, attributes, access_count, updated_at, source
               FROM entity
               WHERE attributes.pipeline_stage = $stage
               ORDER BY updated_at DESC"#
        }
    };

    let stage_owned = stage.to_string();
    let mut response = match status {
        Some(s) => {
            let status_owned = s.to_string();
            db.query(query)
                .bind(("stage", stage_owned))
                .bind(("status", status_owned))
                .await?
        }
        None => db.query(query).bind(("stage", stage_owned)).await?,
    };

    let entities: Vec<EntityDetail> = super::deserialize_take(&mut response, 0)?;
    Ok(entities)
}

/// Get pipeline stats: counts by (stage, status), stale entities.
pub async fn pipeline_stats(
    db: &Surreal<Db>,
    staleness_days: u32,
) -> Result<PipelineGraphStats, GraphError> {
    // Count by stage and status
    let mut response = db
        .query(
            r#"SELECT
                 attributes.pipeline_stage AS stage,
                 attributes.pipeline_status AS status,
                 count() AS count
               FROM entity
               WHERE attributes.pipeline_stage IS NOT NONE
               GROUP BY attributes.pipeline_stage, attributes.pipeline_status"#,
        )
        .await?;

    let rows: Vec<StageStatusCount> = super::deserialize_take(&mut response, 0)?;

    let mut by_stage: std::collections::HashMap<String, std::collections::HashMap<String, u64>> =
        std::collections::HashMap::new();
    let mut total = 0u64;

    for row in rows {
        total += row.count;
        by_stage
            .entry(row.stage)
            .or_default()
            .insert(row.status, row.count);
    }

    // Find stale thoughts (active, not updated in staleness_days)
    let mut stale_response = db
        .query(
            r#"SELECT id, name, entity_type, abstract, overview, attributes, access_count, updated_at, source
               FROM entity
               WHERE attributes.pipeline_stage = 'thoughts'
                 AND attributes.pipeline_status = 'active'
                 AND updated_at < time::now() - type::duration($threshold)
               ORDER BY updated_at ASC"#,
        )
        .bind(("threshold", format!("{}d", staleness_days)))
        .await?;

    let stale_thoughts: Vec<EntityDetail> = super::deserialize_take(&mut stale_response, 0)?;

    // Find stale questions
    let mut stale_q_response = db
        .query(
            r#"SELECT id, name, entity_type, abstract, overview, attributes, access_count, updated_at, source
               FROM entity
               WHERE attributes.pipeline_stage = 'curiosity'
                 AND attributes.pipeline_status = 'active'
                 AND attributes.sub_type IS NONE
                 AND updated_at < time::now() - type::duration($threshold)
               ORDER BY updated_at ASC"#,
        )
        .bind(("threshold", format!("{}d", staleness_days * 2)))
        .await?;

    let stale_questions: Vec<EntityDetail> = super::deserialize_take(&mut stale_q_response, 0)?;

    // Last movement (most recent graduated/dissolved/explored entity)
    let mut movement_response = db
        .query(
            r#"SELECT updated_at
               FROM entity
               WHERE attributes.pipeline_status IN ['graduated', 'dissolved', 'explored']
               ORDER BY updated_at DESC
               LIMIT 1"#,
        )
        .await?;

    let movement_rows: Vec<UpdatedAtRow> = super::deserialize_take(&mut movement_response, 0)?;
    let last_movement = movement_rows.first().map(|r| match &r.updated_at {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });

    Ok(PipelineGraphStats {
        by_stage,
        stale_thoughts,
        stale_questions,
        total_entities: total,
        last_movement,
    })
}

/// Trace the lineage of a pipeline entity through relationship chains.
pub async fn pipeline_flow(
    db: &Surreal<Db>,
    entity_name: &str,
) -> Result<Vec<(EntityDetail, String, EntityDetail)>, GraphError> {
    // Get the entity
    let entity = super::crud::get_entity_by_name(db, entity_name)
        .await?
        .ok_or_else(|| GraphError::NotFound(format!("entity: {}", entity_name)))?;

    let entity_id = entity.id_string();
    let mut chain = Vec::new();

    // Get all pipeline relationships (both directions)
    let pipeline_rel_types = [
        "EVOLVED_FROM",
        "CRYSTALLIZED_FROM",
        "INFORMED_BY",
        "GRADUATED_TO",
        "CONNECTED_TO",
        "EXPLORES",
        "ARCHIVED_FROM",
    ];
    let rel_types_str = pipeline_rel_types
        .iter()
        .map(|r| format!("'{}'", r))
        .collect::<Vec<_>>()
        .join(", ");

    // Outgoing relationships
    let query_out = format!(
        r#"SELECT rel_type, out AS target_id
           FROM relates_to
           WHERE in = type::record($id) AND rel_type IN [{}] AND valid_until IS NONE"#,
        rel_types_str
    );
    let mut response = db.query(&query_out).bind(("id", entity_id.clone())).await?;
    let outgoing: Vec<RelTarget> = super::deserialize_take(&mut response, 0)?;

    for edge in &outgoing {
        let tid = match &edge.target_id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(target) = super::crud::get_entity_detail(db, &tid).await? {
            let source_detail = super::crud::get_entity_detail(db, &entity_id)
                .await?
                .unwrap();
            chain.push((source_detail, edge.rel_type.clone(), target));
        }
    }

    // Incoming relationships
    let query_in = format!(
        r#"SELECT rel_type, in AS target_id
           FROM relates_to
           WHERE out = type::record($id) AND rel_type IN [{}] AND valid_until IS NONE"#,
        rel_types_str
    );
    let mut response = db.query(&query_in).bind(("id", entity_id.clone())).await?;
    let incoming: Vec<RelTarget> = super::deserialize_take(&mut response, 0)?;

    for edge in &incoming {
        let tid = match &edge.target_id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(source) = super::crud::get_entity_detail(db, &tid).await? {
            let target_detail = super::crud::get_entity_detail(db, &entity_id)
                .await?
                .unwrap();
            chain.push((source, edge.rel_type.clone(), target_detail));
        }
    }

    Ok(chain)
}

#[derive(serde::Deserialize)]
struct StageStatusCount {
    stage: String,
    status: String,
    count: u64,
}

#[derive(serde::Deserialize)]
struct UpdatedAtRow {
    updated_at: serde_json::Value,
}
