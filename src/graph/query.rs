//! Hybrid query — combines semantic search, graph expansion, and episode search.
//!
//! Pipeline:
//! 1. **Semantic phase**: HNSW KNN with `limit * 2` to gather candidates
//! 2. **Graph phase**: 1-hop expansion from the top 3 candidates
//! 3. **Merge** by entity ID — corroborating an existing candidate or adding a
//!    new one
//! 4. **Episode search** (optional) — separate KNN on episodes
//!
//! # Scoring
//!
//! Both channels score through [`super::search::score_with_utility`]:
//!
//! ```text
//! score = w_semantic * similarity + w_hotness * hotness + w_utility * utility
//! ```
//!
//! The channel decides only where `similarity` comes from. A semantic
//! candidate measures it against the query vector; a graph candidate
//! *propagates* it — the parent's similarity discounted by the edge's
//! effective (decayed) confidence:
//!
//! ```text
//! similarity_graph = similarity_parent * effective_confidence
//! ```
//!
//! Hotness and utility are read off the neighbor itself, exactly as they are
//! for a semantic hit. Confidence therefore still orders neighbors — a decayed
//! edge ranks below a fresh one — without a graph candidate having to overcome
//! a base every semantic candidate gets for free.
//!
//! When an entity arrives on **both** channels the graph corroborates the
//! query, and its measured relevance is raised — bounded, and proportional to
//! how much the edge is believed:
//!
//! ```text
//! similarity = min(1.0, similarity_semantic
//!                       * (1 + corroboration_boost * effective_confidence))
//! ```
//!
//! The measurement stays the base: a direct reading of this entity against
//! this query outranks an estimate propagated from a neighbor, so
//! corroboration adds to it rather than replacing it. Two clamps keep it from
//! running away — the similarity ceiling of `1.0`, and a boost default cut to
//! the width of the similarity band it perturbs (see
//! [`GraphScoringConfig::corroboration_boost`]). An entity reachable from
//! several expanded parents is credited once, over its strongest path: the
//! parents are the top hits of a single query and are not independent
//! witnesses. Self-edges corroborate nothing and are skipped.

use std::collections::HashMap;

use surrealdb::Surreal;

use super::confidence;
use super::embed::Embedder;
use super::error::GraphError;
use super::search::{compute_hotness, score_with_utility};
use super::store::Db;
use super::types::*;
use crate::config::GraphScoringConfig;

/// Run a hybrid query: semantic search + graph expansion + optional episode search.
pub async fn query(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    scoring: &GraphScoringConfig,
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
        super::search::search_with_options(db, embedder, scoring, query_text, &semantic_options)
            .await?;

    // Collect into dedup map (id -> ScoredEntity)
    let mut entity_map: HashMap<String, ScoredEntity> = HashMap::new();
    for result in semantic_results {
        entity_map.insert(result.entity.id_string(), result);
    }

    // Phase 2: Graph expansion — 1-hop from the top semantic results
    if options.graph_depth > 0 {
        let parents = top_expansion_parents(&entity_map);
        let reached = collect_graph_candidates(db, &parents, options).await?;
        merge_graph_candidates(&mut entity_map, reached, scoring);
    }

    // Sort by score descending, truncate to limit
    let mut entities: Vec<ScoredEntity> = entity_map.into_values().collect();
    entities.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entities.truncate(limit);

    // The semantic phase already counted its own results. Counting only that
    // channel is what keeps the graph tail permanently cold: an entity that is
    // only ever reached over an edge can never accumulate the accesses that
    // feed hotness, so it can never rise. Entities corroborated by the graph
    // kept `MatchSource::Semantic` and are deliberately not counted twice.
    let expanded_ids: Vec<String> = entities
        .iter()
        .filter(|e| matches!(e.source, MatchSource::Graph { .. }))
        .map(|e| e.entity.id_string())
        .collect();
    super::crud::increment_access_counts(db, &expanded_ids).await?;

    // Phase 3: Episode search (optional)
    let episodes = if options.include_episodes {
        super::search::search_episodes(db, embedder, query_text, limit).await?
    } else {
        vec![]
    };

    Ok(QueryResult { entities, episodes })
}

/// How many semantic hits expansion runs from.
const EXPANSION_PARENTS: usize = 3;

/// A semantic hit that expansion runs from.
///
/// Snapshotted before any merging, so a parent that is itself corroborated
/// mid-merge cannot change what its own neighbors inherit.
struct ExpansionParent {
    id: String,
    name: String,
    similarity: f64,
}

/// A neighbor reached by expansion, over the strongest path that reached it.
struct GraphCandidate {
    entity: EntityDetail,
    parent: String,
    rel_type: String,
    effective_confidence: f64,
    /// The parent's similarity, discounted by `effective_confidence`.
    similarity: f64,
}

/// The top semantic hits, best score first.
///
/// Ties break on entity id: candidates arrive from a `HashMap`, and which of
/// two equally scored hits gets expanded must not depend on hash order.
fn top_expansion_parents(entity_map: &HashMap<String, ScoredEntity>) -> Vec<ExpansionParent> {
    let mut ranked: Vec<&ScoredEntity> = entity_map.values().collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.entity.id_string().cmp(&b.entity.id_string()))
    });
    ranked.truncate(EXPANSION_PARENTS);
    ranked
        .into_iter()
        .map(|hit| ExpansionParent {
            id: hit.entity.id_string(),
            name: hit.entity.name.clone(),
            similarity: hit.similarity,
        })
        .collect()
}

/// Walk one hop out of every parent, keeping each neighbor's strongest path.
async fn collect_graph_candidates(
    db: &Surreal<Db>,
    parents: &[ExpansionParent],
    options: &QueryOptions,
) -> Result<HashMap<String, GraphCandidate>, GraphError> {
    let mut reached: HashMap<String, GraphCandidate> = HashMap::new();

    for parent in parents {
        for (entity, rel_type, effective_confidence) in get_neighbor_details(db, &parent.id).await?
        {
            if let Some(ref et) = options.entity_type {
                if entity.entity_type.to_string() != *et {
                    continue;
                }
            }

            let id = entity.id_string();
            if id == parent.id {
                continue; // A self-edge is not a second witness.
            }

            let similarity = parent.similarity * effective_confidence;
            if reached
                .get(&id)
                .is_some_and(|best| best.similarity >= similarity)
            {
                continue;
            }

            reached.insert(
                id,
                GraphCandidate {
                    entity,
                    parent: parent.name.clone(),
                    rel_type,
                    effective_confidence,
                    similarity,
                },
            );
        }
    }

    Ok(reached)
}

/// Fold the expanded neighborhood into the semantic candidates: corroborate
/// what is already there, add what is not.
fn merge_graph_candidates(
    entity_map: &mut HashMap<String, ScoredEntity>,
    reached: HashMap<String, GraphCandidate>,
    scoring: &GraphScoringConfig,
) {
    let now = chrono::Utc::now();

    for (id, candidate) in reached {
        match entity_map.get_mut(&id) {
            Some(existing) => {
                let corroborated = corroborated_similarity(
                    scoring,
                    existing.similarity,
                    candidate.effective_confidence,
                );
                existing.similarity = corroborated;
                existing.score = score_entity(scoring, &existing.entity, corroborated, &now);
            }
            None => {
                let score = score_entity(scoring, &candidate.entity, candidate.similarity, &now);
                entity_map.insert(
                    id,
                    ScoredEntity {
                        entity: candidate.entity,
                        similarity: candidate.similarity,
                        score,
                        source: MatchSource::Graph {
                            parent: candidate.parent,
                            rel_type: candidate.rel_type,
                        },
                    },
                );
            }
        }
    }
}

/// Raise a measured relevance that the graph agrees with, bounded by the
/// similarity ceiling so corroboration can never outrank a perfect match.
fn corroborated_similarity(
    scoring: &GraphScoringConfig,
    similarity: f64,
    effective_confidence: f64,
) -> f64 {
    (similarity * (1.0 + scoring.corroboration_boost * effective_confidence)).min(1.0)
}

/// Score an entity from a relevance term plus its own hotness and utility.
fn score_entity(
    scoring: &GraphScoringConfig,
    entity: &EntityDetail,
    similarity: f64,
    now: &chrono::DateTime<chrono::Utc>,
) -> f64 {
    let hotness = compute_hotness(entity.access_count, &entity.updated_at_string(), now);
    score_with_utility(scoring, similarity, hotness, entity.utility_score)
}

/// Get 1-hop neighbors as L1 (EntityDetail) with the relationship type and effective confidence.
async fn get_neighbor_details(
    db: &Surreal<Db>,
    entity_id: &str,
) -> Result<Vec<(EntityDetail, String, f64)>, GraphError> {
    let now = chrono::Utc::now();

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
        // Apply temporal decay at read time
        let effective = confidence::effective_confidence(
            edge.confidence,
            edge.last_reinforced.as_ref(),
            &edge.valid_from,
            &now,
        );

        // Filter by effective confidence
        if effective < 0.1 {
            continue;
        }

        let tid = match &edge.target_id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        if let Some(detail) = super::crud::get_entity_detail(db, &tid).await? {
            results.push((detail, edge.rel_type, effective));
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

    // Find stale thoughts — connectivity-aware: active thoughts with no relationships
    // updated within staleness_days AND entity itself not updated within staleness_days.
    let mut stale_response = db
        .query(
            r#"SELECT id, name, entity_type, abstract, overview, attributes, access_count, updated_at, source
               FROM entity
               WHERE attributes.pipeline_stage = 'thoughts'
                 AND attributes.pipeline_status = 'active'
                 AND updated_at < time::now() - type::duration($threshold)
                 AND count(
                     SELECT * FROM relates_to
                     WHERE (in = $parent.id OR out = $parent.id)
                       AND valid_from > time::now() - type::duration($threshold)
                 ) = 0
               ORDER BY updated_at ASC"#,
        )
        .bind(("threshold", format!("{staleness_days}d")))
        .await?;

    let stale_thoughts: Vec<EntityDetail> = super::deserialize_take(&mut stale_response, 0)?;

    // Find stale questions — same connectivity-aware approach
    let mut stale_q_response = db
        .query(
            r#"SELECT id, name, entity_type, abstract, overview, attributes, access_count, updated_at, source
               FROM entity
               WHERE attributes.pipeline_stage = 'curiosity'
                 AND attributes.pipeline_status = 'active'
                 AND attributes.sub_type IS NONE
                 AND updated_at < time::now() - type::duration($threshold)
                 AND count(
                     SELECT * FROM relates_to
                     WHERE (in = $parent.id OR out = $parent.id)
                       AND valid_from > time::now() - type::duration($threshold)
                 ) = 0
               ORDER BY updated_at ASC"#,
        )
        .bind(("threshold", format!("{}d", staleness_days * 2)))
        .await?;

    let stale_questions: Vec<EntityDetail> = super::deserialize_take(&mut stale_q_response, 0)?;

    // Orphan detection — active pipeline entities with zero graph connections
    let mut orphan_response = db
        .query(
            r#"SELECT count() AS count FROM entity
               WHERE attributes.pipeline_stage IS NOT NONE
                 AND attributes.pipeline_status = 'active'
                 AND count(SELECT * FROM relates_to WHERE in = $parent.id OR out = $parent.id) = 0
               GROUP ALL"#,
        )
        .await?;

    let orphan_rows: Vec<CountRow> = super::deserialize_take(&mut orphan_response, 0)?;
    let orphan_count = orphan_rows.first().map(|r| r.count).unwrap_or(0);

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
        orphan_count,
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
        .ok_or_else(|| GraphError::NotFound(format!("entity: {entity_name}")))?;

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
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ");

    // Outgoing relationships
    let query_out = format!(
        r#"SELECT rel_type, out AS target_id
           FROM relates_to
           WHERE in = type::record($id) AND rel_type IN [{rel_types_str}] AND valid_until IS NONE"#
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
           WHERE out = type::record($id) AND rel_type IN [{rel_types_str}] AND valid_until IS NONE"#
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

fn lenient_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string, integer, or null")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_unit<E: de::Error>(self) -> Result<String, E> {
            Ok("unknown".to_string())
        }
        fn visit_none<E: de::Error>(self) -> Result<String, E> {
            Ok("unknown".to_string())
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
            Ok(v.to_string())
        }
    }
    deserializer.deserialize_any(Visitor)
}

#[derive(serde::Deserialize)]
struct StageStatusCount {
    #[serde(deserialize_with = "lenient_string")]
    stage: String,
    #[serde(deserialize_with = "lenient_string")]
    status: String,
    count: u64,
}

#[derive(serde::Deserialize)]
struct UpdatedAtRow {
    updated_at: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct CountRow {
    count: u64,
}
