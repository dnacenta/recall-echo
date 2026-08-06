// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inspection — what memory actually holds, for a person rather than a program.
//!
//! Nobody trusts a memory they cannot read. `graph status` counts rows and
//! `graph search` answers questions, but neither answers the question a human
//! actually has after a week of automatic ingestion: *what do you think you
//! know about me, and how sure are you?*
//!
//! Two shapes answer it.
//!
//! [`MemoryOverview`] is the unprompted one: the strongest things the graph
//! holds, grouped by entity type, next to an honest account of how well its
//! relationships are believed — including the coherence tally, which is the
//! one number that says "some of this confidence is me agreeing with myself".
//!
//! [`TopicReport`] is the prompted one. It runs the ordinary hybrid query — no
//! second retrieval path, no second set of scoring weights — and then attaches
//! the evidence behind each hit, which retrieval itself has no reason to carry.

use serde::{Deserialize, Serialize};
use surrealdb::Surreal;

use super::edge_view::{self, EdgeView, NameCache};
use super::embed::Embedder;
use super::error::GraphError;
use super::store::Db;
use super::types::{EntityDetail, GraphStats, MatchSource, QueryOptions};
use crate::config::GraphScoringConfig;

/// At or above this posterior mean, a relationship is held firmly.
pub const STRONG_CONFIDENCE: f64 = 0.8;
/// Below this posterior mean, a relationship is barely believed at all.
pub const DOUBTFUL_CONFIDENCE: f64 = 0.5;

/// Edges listed under "least certain" and "believed partly by repetition".
const HIGHLIGHT_EDGES: usize = 5;
/// Relationships shown per entity in a topic report, before they are summarised
/// as a count.
const MAX_EDGES_PER_ENTITY: usize = 8;
/// Graph expansion for a topic report — one hop, as the `graph query` CLI uses.
const TOPIC_GRAPH_DEPTH: u32 = 1;

/// What the graph holds, without being asked about anything in particular.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryOverview {
    /// Row counts, including the per-type breakdown.
    pub stats: GraphStats,
    /// The strongest entities of each type, most populous type first.
    pub groups: Vec<TypeGroup>,
    /// How firmly the live relationships are held, in aggregate.
    pub confidence: ConfidenceSummary,
    /// The least-believed live relationships — where memory is unsure.
    pub uncertain: Vec<EdgeView>,
    /// The relationships carrying the most self-authored corroboration —
    /// where memory may be agreeing with itself.
    pub self_reinforced: Vec<EdgeView>,
}

/// One entity type, and the strongest entities in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeGroup {
    pub entity_type: String,
    /// How many entities of this type exist, not how many are listed.
    pub count: u64,
    pub top: Vec<KnownEntity>,
}

/// One entity as an overview lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownEntity {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(default)]
    pub access_count: i64,
    #[serde(default)]
    pub utility_score: f64,
}

/// How well the live relationships are believed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfidenceSummary {
    /// At or above [`STRONG_CONFIDENCE`].
    pub strong: u64,
    /// Between [`DOUBTFUL_CONFIDENCE`] and [`STRONG_CONFIDENCE`].
    pub uncertain: u64,
    /// Below [`DOUBTFUL_CONFIDENCE`].
    pub doubtful: u64,
}

impl ConfidenceSummary {
    /// Live relationships counted.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.strong + self.uncertain + self.doubtful
    }

    /// Tally one relationship's posterior mean.
    fn record(&mut self, confidence: f64) {
        if confidence >= STRONG_CONFIDENCE {
            self.strong += 1;
        } else if confidence >= DOUBTFUL_CONFIDENCE {
            self.uncertain += 1;
        } else {
            self.doubtful += 1;
        }
    }
}

/// What the graph holds about one subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicReport {
    pub topic: String,
    pub entities: Vec<TopicEntity>,
}

/// One retrieved entity, with the claims it takes part in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicEntity {
    pub entity: EntityDetail,
    /// Retrieval score, unchanged from the hybrid query that produced it.
    pub score: f64,
    /// Whether it was matched directly or reached over a relationship.
    pub source: MatchSource,
    /// Its live relationships, strongest first.
    pub edges: Vec<EdgeView>,
    /// Relationships beyond the ones listed.
    #[serde(default)]
    pub edges_omitted: usize,
}

/// Summarise the whole graph, listing `per_type` entities of each type.
pub async fn overview(
    db: &Surreal<Db>,
    stats: GraphStats,
    per_type: usize,
) -> Result<MemoryOverview, GraphError> {
    let mut groups = Vec::with_capacity(stats.entity_type_counts.len());
    for (entity_type, count) in &stats.entity_type_counts {
        groups.push(TypeGroup {
            entity_type: entity_type.clone(),
            count: *count,
            top: strongest_of_type(db, entity_type, per_type).await?,
        });
    }
    groups.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.entity_type.cmp(&right.entity_type))
    });

    let mut cache = NameCache::new();
    let uncertain = edge_view::views(db, &mut cache, &least_certain(db).await?).await?;
    let self_reinforced =
        edge_view::views(db, &mut cache, &most_self_reinforced(db).await?).await?;

    Ok(MemoryOverview {
        stats,
        groups,
        confidence: confidence_summary(db).await?,
        uncertain,
        self_reinforced,
    })
}

/// Answer "what do you know about X" through the ordinary hybrid query, then
/// attach the evidence behind each hit.
pub async fn about(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    scoring: &GraphScoringConfig,
    topic: &str,
    limit: usize,
) -> Result<TopicReport, GraphError> {
    let options = QueryOptions {
        limit,
        entity_type: None,
        keyword: None,
        graph_depth: TOPIC_GRAPH_DEPTH,
        include_episodes: false,
    };
    let result = super::query::query(db, embedder, scoring, topic, &options).await?;

    let mut cache = NameCache::new();
    let mut entities = Vec::with_capacity(result.entities.len());
    for scored in result.entities {
        let all = edge_view::live_edges_of(db, &scored.entity.id_string()).await?;
        let edges_omitted = all.len().saturating_sub(MAX_EDGES_PER_ENTITY);
        let shown = &all[..all.len().min(MAX_EDGES_PER_ENTITY)];
        entities.push(TopicEntity {
            entity: scored.entity,
            score: scored.score,
            source: scored.source,
            edges: edge_view::views(db, &mut cache, shown).await?,
            edges_omitted,
        });
    }

    Ok(TopicReport {
        topic: topic.to_string(),
        entities,
    })
}

// ── Queries ──────────────────────────────────────────────────────────────

/// The entities of one type most worth showing first: the ones feedback has
/// found useful, then the ones retrieval keeps returning, then the freshest.
///
/// This is a display order, not a retrieval score — nothing here feeds ranking.
async fn strongest_of_type(
    db: &Surreal<Db>,
    entity_type: &str,
    limit: usize,
) -> Result<Vec<KnownEntity>, GraphError> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: serde_json::Value,
        name: String,
        entity_type: String,
        #[serde(rename = "abstract")]
        abstract_text: String,
        #[serde(default, deserialize_with = "super::util::count_or_zero")]
        access_count: i64,
        #[serde(default)]
        utility_score: Option<f64>,
    }

    // `updated_at` is projected because it is ordered on: SurrealDB requires
    // every ORDER BY idiom to appear in the selection.
    let query = format!(
        r#"SELECT id, name, entity_type, abstract, access_count, utility_score, updated_at
           FROM entity WHERE entity_type = $entity_type
           ORDER BY utility_score DESC, access_count DESC, updated_at DESC
           LIMIT {}"#,
        limit.clamp(1, 100)
    );
    let mut response = db
        .query(&query)
        .bind(("entity_type", entity_type.to_string()))
        .await?;

    let rows: Vec<Row> = super::deserialize_take(&mut response, 0)?;
    Ok(rows
        .into_iter()
        .map(|row| KnownEntity {
            id: edge_view::record_id(&row.id),
            name: row.name,
            entity_type: row.entity_type,
            abstract_text: row.abstract_text,
            access_count: row.access_count,
            utility_score: row.utility_score.unwrap_or(0.5),
        })
        .collect())
}

/// Band every live relationship by how firmly it is believed.
async fn confidence_summary(db: &Surreal<Db>) -> Result<ConfidenceSummary, GraphError> {
    let mut response = db
        .query("SELECT VALUE confidence FROM relates_to WHERE valid_until IS NONE")
        .await?;
    let confidences: Vec<f64> = super::deserialize_take(&mut response, 0)?;

    let mut summary = ConfidenceSummary::default();
    for confidence in confidences {
        summary.record(confidence);
    }
    Ok(summary)
}

/// The live relationships the graph believes least.
async fn least_certain(db: &Surreal<Db>) -> Result<Vec<super::types::Relationship>, GraphError> {
    let query = format!(
        r#"SELECT * FROM relates_to
           WHERE valid_until IS NONE AND confidence < {STRONG_CONFIDENCE}
           ORDER BY confidence ASC
           LIMIT {HIGHLIGHT_EDGES}"#
    );
    let mut response = db.query(&query).await?;
    super::deserialize_take(&mut response, 0)
}

/// The relationships carrying the most self-authored corroboration.
async fn most_self_reinforced(
    db: &Surreal<Db>,
) -> Result<Vec<super::types::Relationship>, GraphError> {
    let query = format!(
        r#"SELECT * FROM relates_to
           WHERE self_reinforcements IS NOT NONE AND self_reinforcements > 0
           ORDER BY self_reinforcements DESC
           LIMIT {HIGHLIGHT_EDGES}"#
    );
    let mut response = db.query(&query).await?;
    super::deserialize_take(&mut response, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_bands_split_at_the_documented_thresholds() {
        let mut summary = ConfidenceSummary::default();
        for confidence in [1.0, STRONG_CONFIDENCE, 0.79, DOUBTFUL_CONFIDENCE, 0.49, 0.0] {
            summary.record(confidence);
        }
        assert_eq!(summary.strong, 2);
        assert_eq!(summary.uncertain, 2);
        assert_eq!(summary.doubtful, 2);
        assert_eq!(summary.total(), 6);
    }

    #[test]
    fn an_empty_graph_has_nothing_to_be_sure_of() {
        let summary = ConfidenceSummary::default();
        assert_eq!(summary.total(), 0);
    }
}
