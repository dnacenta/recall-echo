//! Semantic search across entities and episodes using HNSW KNN + hotness scoring.

use surrealdb::Surreal;

use crate::embed::Embedder;
use crate::error::GraphError;
use crate::store::Db;
use crate::types::*;

/// Semantic search across entities using HNSW KNN + hotness scoring.
///
/// This is the simple search — returns full `Entity` objects for backwards
/// compatibility. For L1 projections and filters, use `search_with_options`.
///
/// Hotness formula:
///   hotness = sigmoid(ln(1 + access_count)) * exp(-λ * days_since_update)
///   final_score = α * semantic_similarity + (1 - α) * hotness
///
/// Where λ = ln(2)/7 (7-day half-life), α = 0.7
pub async fn search(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, GraphError> {
    let query_embedding = embedder.embed_single(query)?;

    // HNSW KNN — uses the entity_vector index (HNSW DIMENSION 384 DIST COSINE)
    // KNN operator requires literal integers (not bind params)
    let ef = (limit * 4).max(40);
    let sql = format!(
        r#"SELECT *,
                vector::distance::knn() AS distance
            FROM entity
            WHERE embedding <|{limit}, {ef}|> $query_vec
            ORDER BY distance"#,
    );

    let mut response = db.query(&sql).bind(("query_vec", query_embedding)).await?;

    let rows: Vec<EntityWithDistance> = crate::deserialize_take(&mut response, 0)?;

    let now = chrono::Utc::now();
    let results: Vec<SearchResult> = rows
        .into_iter()
        .map(|row| {
            let similarity = 1.0 - row.distance;
            let hotness = compute_hotness(
                row.entity.access_count,
                &row.entity.updated_at_string(),
                &now,
            );
            let score = 0.7 * similarity + 0.3 * hotness;

            SearchResult {
                entity: row.entity,
                score,
                distance: row.distance,
            }
        })
        .collect();

    // Batch increment access counts
    let ids: Vec<String> = results.iter().map(|r| r.entity.id_string()).collect();
    crate::crud::increment_access_counts(db, &ids).await?;

    Ok(results)
}

/// Search with options — returns L1 projections (EntityDetail), supports type/keyword filters.
pub async fn search_with_options(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    query_text: &str,
    options: &SearchOptions,
) -> Result<Vec<ScoredEntity>, GraphError> {
    let query_embedding = embedder.embed_single(query_text)?;
    let limit = if options.limit == 0 {
        10
    } else {
        options.limit
    };

    let has_filters = options.entity_type.is_some() || options.keyword.is_some();

    // KNN doesn't support post-filter AND clauses, so fetch more and filter in Rust
    let fetch_limit = if has_filters { limit * 4 } else { limit };
    let ef = (fetch_limit * 4).max(40);
    let sql = format!(
        r#"SELECT id, name, entity_type, abstract, overview, attributes,
                  access_count, updated_at, source,
                  vector::distance::knn() AS distance
           FROM entity
           WHERE embedding <|{fetch_limit}, {ef}|> $query_vec
           ORDER BY distance"#,
    );

    let mut response = db.query(&sql).bind(("query_vec", query_embedding)).await?;

    let rows: Vec<DetailWithDistance> = crate::deserialize_take(&mut response, 0)?;

    let now = chrono::Utc::now();
    let mut results: Vec<ScoredEntity> = rows
        .into_iter()
        .filter(|row| {
            // Apply type filter
            if let Some(ref et) = options.entity_type {
                if row.entity.entity_type.to_string() != *et {
                    return false;
                }
            }
            // Apply keyword filter
            if let Some(ref kw) = options.keyword {
                let kw_lower = kw.to_lowercase();
                let name_match = row.entity.name.to_lowercase().contains(&kw_lower);
                let abs_match = row.entity.abstract_text.to_lowercase().contains(&kw_lower);
                if !name_match && !abs_match {
                    return false;
                }
            }
            true
        })
        .map(|row| {
            let similarity = 1.0 - row.distance;
            let hotness = compute_hotness(
                row.entity.access_count,
                &row.entity.updated_at_string(),
                &now,
            );
            let score = 0.7 * similarity + 0.3 * hotness;

            ScoredEntity {
                entity: row.entity,
                score,
                source: MatchSource::Semantic,
            }
        })
        .collect();

    results.truncate(limit);

    // Batch increment access counts
    let ids: Vec<String> = results.iter().map(|r| r.entity.id_string()).collect();
    crate::crud::increment_access_counts(db, &ids).await?;

    Ok(results)
}

/// Semantic search across episodes using HNSW KNN.
pub async fn search_episodes(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    query_text: &str,
    limit: usize,
) -> Result<Vec<EpisodeSearchResult>, GraphError> {
    let query_embedding = embedder.embed_single(query_text)?;

    let ef = (limit * 4).max(40);
    let sql = format!(
        r#"SELECT *,
                vector::distance::knn() AS distance
            FROM episode
            WHERE embedding <|{limit}, {ef}|> $query_vec
            ORDER BY distance"#,
    );

    let mut response = db.query(&sql).bind(("query_vec", query_embedding)).await?;

    let rows: Vec<EpisodeWithDistance> = crate::deserialize_take(&mut response, 0)?;

    let results = rows
        .into_iter()
        .map(|row| {
            let similarity = 1.0 - row.distance;
            EpisodeSearchResult {
                episode: row.episode,
                score: similarity,
                distance: row.distance,
            }
        })
        .collect();

    Ok(results)
}

// ── Deserialization helpers ──────────────────────────────────────────

#[derive(serde::Deserialize)]
struct EntityWithDistance {
    #[serde(flatten)]
    entity: Entity,
    distance: f64,
}

#[derive(serde::Deserialize)]
struct DetailWithDistance {
    #[serde(flatten)]
    entity: EntityDetail,
    distance: f64,
}

#[derive(serde::Deserialize)]
struct EpisodeWithDistance {
    #[serde(flatten)]
    episode: Episode,
    distance: f64,
}

// ── Hotness scoring ─────────────────────────────────────────────────

pub(crate) fn compute_hotness(
    access_count: i64,
    updated_at: &str,
    now: &chrono::DateTime<chrono::Utc>,
) -> f64 {
    let days_since = chrono::DateTime::parse_from_rfc3339(updated_at)
        .map(|dt| (*now - dt.with_timezone(&chrono::Utc)).num_hours() as f64 / 24.0)
        .unwrap_or(30.0);

    let lambda = (2.0_f64).ln() / 7.0; // 7-day half-life
    let activity = sigmoid((1.0 + access_count as f64).ln());
    let recency = (-lambda * days_since).exp();

    activity * recency
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}
