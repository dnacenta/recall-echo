//! Semantic search across entities and episodes using HNSW KNN + hotness scoring.

use surrealdb::Surreal;

use super::embed::Embedder;
use super::error::GraphError;
use super::store::Db;
use super::types::*;
use crate::config::GraphScoringConfig;

/// Semantic search across entities using HNSW KNN + utility-weighted scoring.
///
/// Scoring formula:
///
/// ```text
/// final_score = w_semantic * similarity
///             + w_hotness  * hotness
///             + w_utility  * utility_score
/// ```
///
/// Weights come from `scoring` (see [`GraphScoringConfig`]). Defaults
/// (`0.45` / `0.30` / `0.25`) preserve legacy behavior.
///
/// Where `hotness = sigmoid(ln(1 + access_count)) * exp(-lambda * days_since_update)`
/// and `utility_score` comes from outcome feedback.
pub async fn search(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    scoring: &GraphScoringConfig,
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

    let rows: Vec<EntityWithDistance> = super::deserialize_take(&mut response, 0)?;

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
            let utility = row.entity.utility_score;
            let score = score_with_utility(scoring, similarity, hotness, utility);

            SearchResult {
                entity: row.entity,
                score,
                distance: row.distance,
            }
        })
        .collect();

    // Batch increment access counts
    let ids: Vec<String> = results.iter().map(|r| r.entity.id_string()).collect();
    super::crud::increment_access_counts(db, &ids).await?;

    Ok(results)
}

/// Search with options — returns L1 projections (EntityDetail), supports type/keyword filters.
pub async fn search_with_options(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    scoring: &GraphScoringConfig,
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
                  access_count, utility_score, updated_at, source,
                  vector::distance::knn() AS distance
           FROM entity
           WHERE embedding <|{fetch_limit}, {ef}|> $query_vec
           ORDER BY distance"#,
    );

    let mut response = db.query(&sql).bind(("query_vec", query_embedding)).await?;

    let rows: Vec<DetailWithDistance> = super::deserialize_take(&mut response, 0)?;

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
            let utility = row.entity.utility_score;
            let score = score_with_utility(scoring, similarity, hotness, utility);

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
    super::crud::increment_access_counts(db, &ids).await?;

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

    let rows: Vec<EpisodeWithDistance> = super::deserialize_take(&mut response, 0)?;

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

// ── Utility-weighted scoring (Adaptive Learning v2 Phase 2) ────────

/// Compute final score with utility weighting.
///
/// Linear combination of similarity, hotness, and utility using weights
/// from `scoring`. Defaults (0.45 / 0.30 / 0.25) preserve legacy behavior
/// so deployments without a `[graph.scoring]` TOML section see no change.
fn score_with_utility(
    scoring: &GraphScoringConfig,
    similarity: f64,
    hotness: f64,
    utility: f64,
) -> f64 {
    scoring.weight_semantic * similarity
        + scoring.weight_hotness * hotness
        + scoring.weight_utility * utility
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

#[cfg(test)]
mod tests {
    use super::*;

    const SIMILARITY: f64 = 0.8;
    const HOTNESS: f64 = 0.4;
    const UTILITY: f64 = 0.6;

    /// The default config must produce the exact same value as the pre-v3.9.0
    /// hard-coded formula (`0.45 * similarity + 0.30 * hotness + 0.25 * utility`).
    /// Anyone running with no `[graph.scoring]` section in `.recall-echo.toml`
    /// must see identical scoring output.
    #[test]
    fn default_config_matches_legacy_hardcoded_formula() {
        let scoring = GraphScoringConfig::default();
        let legacy = 0.45 * SIMILARITY + 0.30 * HOTNESS + 0.25 * UTILITY;
        let actual = score_with_utility(&scoring, SIMILARITY, HOTNESS, UTILITY);
        assert!((actual - legacy).abs() < f64::EPSILON);
    }

    #[test]
    fn raising_weight_utility_raises_score_for_high_utility_entity() {
        let low = GraphScoringConfig {
            weight_semantic: 0.45,
            weight_hotness: 0.30,
            weight_utility: 0.25,
        };
        let high = GraphScoringConfig {
            weight_semantic: 0.25,
            weight_hotness: 0.25,
            weight_utility: 0.5,
        };
        let high_utility = 0.9;
        let low_score = score_with_utility(&low, SIMILARITY, HOTNESS, high_utility);
        let high_score = score_with_utility(&high, SIMILARITY, HOTNESS, high_utility);
        assert!(
            high_score > low_score,
            "boosting weight_utility should raise the score for a high-utility entity \
             (low={low_score}, high={high_score})"
        );
    }

    #[test]
    fn zero_weights_produce_zero_score() {
        let scoring = GraphScoringConfig {
            weight_semantic: 0.0,
            weight_hotness: 0.0,
            weight_utility: 0.0,
        };
        let score = score_with_utility(&scoring, SIMILARITY, HOTNESS, UTILITY);
        assert!((score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn custom_weights_compute_linear_combination() {
        let scoring = GraphScoringConfig {
            weight_semantic: 0.5,
            weight_hotness: 0.2,
            weight_utility: 0.3,
        };
        let expected = 0.5 * SIMILARITY + 0.2 * HOTNESS + 0.3 * UTILITY;
        let actual = score_with_utility(&scoring, SIMILARITY, HOTNESS, UTILITY);
        assert!((actual - expected).abs() < f64::EPSILON);
    }
}
