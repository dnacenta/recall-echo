use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Node types in the knowledge graph.
/// Mutable types can be merged/updated. Immutable types are historical facts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Person,
    Project,
    Tool,
    Service,
    Preference,
    Decision,
    Event,
    Concept,
    Case,
    Pattern,
    Thread,
    Thought,
    Question,
    Observation,
    Policy,
    Measurement,
    Outcome,
}

impl EntityType {
    #[must_use]
    pub fn is_mutable(&self) -> bool {
        !matches!(
            self,
            Self::Decision
                | Self::Event
                | Self::Case
                | Self::Observation
                | Self::Measurement
                | Self::Outcome
        )
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        write!(f, "{s}")
    }
}

impl std::str::FromStr for EntityType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(s.to_string()))
            .map_err(|_| format!("unknown entity type: {s}"))
    }
}

/// Input for creating a new entity.
#[derive(Debug, Clone)]
pub struct NewEntity {
    pub name: String,
    pub entity_type: EntityType,
    pub abstract_text: String,
    pub overview: Option<String>,
    pub content: Option<String>,
    pub attributes: Option<serde_json::Value>,
    pub source: Option<String>,
}

/// A stored entity with all fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: serde_json::Value,
    pub name: String,
    pub entity_type: EntityType,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub overview: String,
    pub content: Option<String>,
    pub attributes: Option<serde_json::Value>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    #[serde(default = "default_true")]
    pub mutable: bool,
    #[serde(default)]
    pub access_count: i64,
    /// How useful this entity has been across sessions (0.0-1.0, default 0.5 neutral).
    #[serde(default = "default_utility_score")]
    pub utility_score: f64,
    /// Number of times utility_score has been updated via outcome feedback.
    #[serde(default)]
    pub utility_updates: i64,
    pub created_at: serde_json::Value,
    pub updated_at: serde_json::Value,
    pub source: Option<String>,
}

impl Entity {
    /// Get the record ID as a string (e.g. "entity:abc123").
    #[must_use]
    pub fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// Get the updated_at timestamp as a string.
    #[must_use]
    pub fn updated_at_string(&self) -> String {
        match &self.updated_at {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_utility_score() -> f64 {
    0.5
}

/// Fields that can be updated on an entity.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EntityUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Value>,
}

/// Input for creating a new relationship.
#[derive(Debug, Clone)]
pub struct NewRelationship {
    pub from_entity: String,
    pub to_entity: String,
    pub rel_type: String,
    pub description: Option<String>,
    pub confidence: Option<f32>,
    pub source: Option<String>,
}

/// A stored relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: serde_json::Value,
    #[serde(rename = "in")]
    pub from_id: serde_json::Value,
    #[serde(rename = "out")]
    pub to_id: serde_json::Value,
    pub rel_type: String,
    pub description: Option<String>,
    pub valid_from: serde_json::Value,
    pub valid_until: Option<serde_json::Value>,
    pub confidence: f64,
    /// When this relationship was last reinforced (Bayesian corroboration).
    /// Used by temporal decay: effective_confidence = confidence × 0.5^(days_since / half_life).
    #[serde(default)]
    pub last_reinforced: Option<serde_json::Value>,
    pub source: Option<String>,
}

impl Relationship {
    /// Get the record ID as a string.
    #[must_use]
    pub fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// Direction for relationship queries.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

// ── Tiered entity projections ────────────────────────────────────────

/// L0 — Minimal entity for traversal. No embedding, no content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySummary {
    pub id: serde_json::Value,
    pub name: String,
    pub entity_type: EntityType,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
}

impl EntitySummary {
    #[must_use]
    pub fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// L1 — Search result detail. Everything except content and embedding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityDetail {
    pub id: serde_json::Value,
    pub name: String,
    pub entity_type: EntityType,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub overview: String,
    pub attributes: Option<serde_json::Value>,
    #[serde(default)]
    pub access_count: i64,
    pub updated_at: serde_json::Value,
    pub source: Option<String>,
}

impl EntityDetail {
    #[must_use]
    pub fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    #[must_use]
    pub fn updated_at_string(&self) -> String {
        match &self.updated_at {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

// ── Search types ────────────────────────────────────────────────────

/// Options for entity search.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: usize,
    pub entity_type: Option<String>,
    pub keyword: Option<String>,
}

/// How an entity was found.
#[derive(Debug, Clone)]
pub enum MatchSource {
    /// Found via semantic similarity.
    Semantic,
    /// Found via graph expansion from a parent entity.
    Graph { parent: String, rel_type: String },
    /// Found via keyword filter match.
    Keyword,
}

/// A scored entity in search results.
#[derive(Debug, Clone)]
pub struct ScoredEntity {
    pub entity: EntityDetail,
    pub score: f64,
    pub source: MatchSource,
}

/// An episode search result.
#[derive(Debug, Clone)]
pub struct EpisodeSearchResult {
    pub episode: Episode,
    pub score: f64,
    pub distance: f64,
}

/// Options for hybrid query (semantic + graph expansion + episodes).
#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub limit: usize,
    pub entity_type: Option<String>,
    pub keyword: Option<String>,
    pub graph_depth: u32,
    pub include_episodes: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            entity_type: None,
            keyword: None,
            graph_depth: 1,
            include_episodes: false,
        }
    }
}

/// Result of a hybrid query.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub entities: Vec<ScoredEntity>,
    pub episodes: Vec<EpisodeSearchResult>,
}

/// A search result with scoring (legacy — wraps full Entity).
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub entity: Entity,
    pub score: f64,
    pub distance: f64,
}

/// A node in a traversal tree.
#[derive(Debug, Clone)]
pub struct TraversalNode {
    pub entity: EntitySummary,
    pub edges: Vec<TraversalEdge>,
}

/// An edge in a traversal tree.
#[derive(Debug, Clone)]
pub struct TraversalEdge {
    pub rel_type: String,
    pub direction: String,
    pub target: TraversalNode,
    pub valid_from: serde_json::Value,
    pub valid_until: Option<serde_json::Value>,
    pub confidence: f64,
}

/// A row from a relationship query (shared by traverse and query).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EdgeRow {
    pub rel_type: String,
    pub valid_from: serde_json::Value,
    pub valid_until: Option<serde_json::Value>,
    pub target_id: serde_json::Value,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub last_reinforced: Option<serde_json::Value>,
}

fn default_confidence() -> f64 {
    1.0
}

impl EdgeRow {
    #[must_use]
    pub fn target_id_string(&self) -> String {
        match &self.target_id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// Graph-level statistics.
#[derive(Debug, Clone)]
pub struct GraphStats {
    pub entity_count: u64,
    pub relationship_count: u64,
    pub episode_count: u64,
    pub entity_type_counts: HashMap<String, u64>,
}

// ── Ingestion types (Phase 2) ────────────────────────────────────────

/// Input for creating a new episode.
#[derive(Debug, Clone)]
pub struct NewEpisode {
    pub session_id: String,
    pub abstract_text: String,
    pub overview: Option<String>,
    pub content: Option<String>,
    pub log_number: Option<u32>,
}

/// A stored episode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: serde_json::Value,
    pub session_id: String,
    pub timestamp: serde_json::Value,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub overview: Option<String>,
    pub content: Option<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub log_number: Option<i64>,
}

impl Episode {
    #[must_use]
    pub fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// A candidate entity extracted by the LLM from a conversation chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub overview: Option<String>,
    pub content: Option<String>,
    pub attributes: Option<serde_json::Value>,
}

impl ExtractedEntity {
    /// Convert this extraction result into a `NewEntity` ready for storage.
    #[must_use]
    pub fn to_new_entity(&self, session_id: &str) -> NewEntity {
        NewEntity {
            name: self.name.clone(),
            entity_type: self.entity_type.clone(),
            abstract_text: self.abstract_text.clone(),
            overview: self.overview.clone(),
            content: self.content.clone(),
            attributes: self.attributes.clone(),
            source: Some(session_id.to_string()),
        }
    }
}

/// A candidate relationship extracted by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelationship {
    pub source: String,
    pub target: String,
    pub rel_type: String,
    pub description: Option<String>,
    #[serde(default)]
    pub confidence: Option<String>,
}

/// An extracted case (problem-solution pair).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedCase {
    pub problem: String,
    pub solution: String,
    pub context: Option<String>,
}

/// An extracted pattern (reusable process).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPattern {
    pub name: String,
    pub process: String,
    pub conditions: Option<String>,
}

/// An extracted preference (one per facet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPreference {
    pub facet: String,
    pub value: String,
    pub context: Option<String>,
}

/// Full extraction result from a single conversation chunk.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtractionResult {
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    #[serde(default)]
    pub relationships: Vec<ExtractedRelationship>,
    #[serde(default)]
    pub cases: Vec<ExtractedCase>,
    #[serde(default)]
    pub patterns: Vec<ExtractedPattern>,
    #[serde(default)]
    pub preferences: Vec<ExtractedPreference>,
}

/// LLM deduplication decision for a candidate entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupDecision {
    Skip,
    Create,
    Merge { target: String },
}

// ── Pipeline types ───────────────────────────────────────────────────

/// Canonical relationship types for the praxis pipeline.
pub mod pipeline_rels {
    pub const EVOLVED_FROM: &str = "EVOLVED_FROM";
    pub const CRYSTALLIZED_FROM: &str = "CRYSTALLIZED_FROM";
    pub const INFORMED_BY: &str = "INFORMED_BY";
    pub const EXPLORES: &str = "EXPLORES";
    pub const GRADUATED_TO: &str = "GRADUATED_TO";
    pub const ARCHIVED_FROM: &str = "ARCHIVED_FROM";
    pub const CONNECTED_TO: &str = "CONNECTED_TO";
    pub const PROMPTED_BY: &str = "PROMPTED_BY";
    pub const ANSWERED_BY: &str = "ANSWERED_BY";
}

/// Canonical relationship types for vigil-pulse data.
pub mod vigil_rels {
    pub const MEASURED_DURING: &str = "MEASURED_DURING";
    pub const RESULTED_IN: &str = "RESULTED_IN";
    pub const TRIGGERED_BY: &str = "TRIGGERED_BY";
}

/// Report from a vigil sync operation.
#[derive(Debug, Clone, Default)]
pub struct VigilSyncReport {
    pub measurements_created: u32,
    pub outcomes_created: u32,
    pub events_created: u32,
    pub relationships_created: u32,
    pub skipped: u32,
    pub errors: Vec<String>,
}

/// Contents of all 5 pipeline markdown files.
#[derive(Debug, Clone, Default)]
pub struct PipelineDocuments {
    pub learning: String,
    pub thoughts: String,
    pub curiosity: String,
    pub reflections: String,
    pub praxis: String,
}

/// Report from a pipeline sync operation.
#[derive(Debug, Clone, Default)]
pub struct PipelineSyncReport {
    pub entities_created: u32,
    pub entities_updated: u32,
    pub entities_archived: u32,
    pub relationships_created: u32,
    pub relationships_skipped: u32,
    pub errors: Vec<String>,
}

/// Pipeline health stats from the graph.
#[derive(Debug, Clone, Default)]
pub struct PipelineGraphStats {
    pub by_stage: HashMap<String, HashMap<String, u64>>,
    pub stale_thoughts: Vec<EntityDetail>,
    pub stale_questions: Vec<EntityDetail>,
    pub orphan_count: u64,
    pub total_entities: u64,
    pub last_movement: Option<String>,
}

/// A parsed pipeline entry from a markdown document.
#[derive(Debug, Clone)]
pub struct PipelineEntry {
    /// Title from ### heading (cleaned of dates and markers).
    pub title: String,
    /// Full content under the heading.
    pub body: String,
    /// Status: "active", "graduated", "dissolved", "explored", "retired".
    pub status: String,
    /// Stage: "learning", "thoughts", "curiosity", "reflections", "praxis".
    pub stage: String,
    /// Mapped entity type.
    pub entity_type: EntityType,
    /// Date from heading or metadata field.
    pub date: Option<String>,
    /// **Source:** field value.
    pub source_ref: Option<String>,
    /// **Destination:** field value.
    pub destination: Option<String>,
    /// Parsed "Connected to:" references.
    pub connected_to: Vec<String>,
    /// Sub-type for special sections: "theme", "pattern", "phronesis".
    pub sub_type: Option<String>,
}

/// Result of a full ingestion run.
#[derive(Debug, Clone, Default)]
pub struct IngestionReport {
    pub episodes_created: u32,
    pub entities_created: u32,
    pub entities_merged: u32,
    pub entities_skipped: u32,
    pub relationships_created: u32,
    pub relationships_skipped: u32,
    pub errors: Vec<String>,
    pub estimated_tokens: u64,
}
