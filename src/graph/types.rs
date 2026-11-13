// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::confidence::{EdgeEvidence, Evidence, Provenance};

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
    /// Omitted when absent: an entity crossing the daemon socket carries no
    /// embedding, and 384 floats of JSON text per entity is pure overhead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Posterior mean of the edge's Beta distribution — `alpha / (alpha + beta)`.
    /// Kept in sync with the counts on every write; this is what read paths score on.
    pub confidence: f64,
    /// Accumulated corroborating evidence (Beta α).
    /// `None` only on an edge the schema migration has not reached yet.
    #[serde(default)]
    pub alpha: Option<f64>,
    /// Accumulated contradicting evidence (Beta β).
    #[serde(default)]
    pub beta: Option<f64>,
    /// How many corroborations came from the agent itself — counted, never
    /// laundered into confidence. Populated from Phase 1 increment 2 onward.
    #[serde(default)]
    pub self_reinforcements: Option<i64>,
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

    /// The edge's accumulated evidence, falling back to the prior implied by
    /// its mean when the counts have not been backfilled yet.
    #[must_use]
    pub fn evidence(&self) -> Evidence {
        Evidence::from_stored(self.alpha, self.beta, self.confidence)
    }

    /// The edge's full evidence state — Beta counts plus coherence tally.
    /// This is what a confidence-moving observation is applied to.
    #[must_use]
    pub fn edge_evidence(&self) -> EdgeEvidence {
        EdgeEvidence::new(self.evidence(), self.self_reinforcements.unwrap_or(0))
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    #[serde(default = "default_utility_score")]
    pub utility_score: f64,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchSource {
    /// Found via semantic similarity.
    Semantic,
    /// Found via graph expansion from a parent entity.
    Graph { parent: String, rel_type: String },
    /// Found via keyword filter match.
    Keyword,
}

/// A scored entity in search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredEntity {
    pub entity: EntityDetail,
    /// The relevance term that went into `score`, in `[0, 1]`.
    ///
    /// For a semantic match this is cosine similarity against the query
    /// vector. For a graph-expanded match it is the parent's similarity
    /// discounted by the edge's effective confidence — the same quantity,
    /// estimated over the graph rather than measured against the query. Kept
    /// alongside `score` because the blend that produced `score` cannot be
    /// inverted (any weight may be zero).
    #[serde(default)]
    pub similarity: f64,
    pub score: f64,
    pub source: MatchSource,
}

/// An episode search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub entities: Vec<ScoredEntity>,
    pub episodes: Vec<EpisodeSearchResult>,
}

/// A search result with scoring (legacy — wraps full Entity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub entity: Entity,
    /// Blended retrieval score: semantic similarity plus hotness and utility.
    /// It answers *how worth returning is this*, not *how alike is this*.
    pub score: f64,
    /// Cosine distance between the query and the entity's embedding.
    pub distance: f64,
}

impl SearchResult {
    /// Raw cosine similarity between the query and this entity — the
    /// popularity-free half of [`SearchResult::score`].
    ///
    /// Callers asking "is this the same thing?" (deduplication) must use this;
    /// callers asking "is this worth returning?" (retrieval) want the score.
    #[must_use]
    pub fn similarity(&self) -> f64 {
        1.0 - self.distance
    }
}

/// A node in a traversal tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalNode {
    pub entity: EntitySummary,
    pub edges: Vec<TraversalEdge>,
}

/// An edge in a traversal tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphStats {
    pub entity_count: u64,
    pub relationship_count: u64,
    pub episode_count: u64,
    pub entity_type_counts: HashMap<String, u64>,
    /// Distinct log numbers the unextracted scan would process. Defaulted so
    /// a status response from an older daemon still deserializes.
    ///
    /// This field and `log_number_absent` are full episode-table scans, so
    /// `stats()` computes them **only** when the store has episodes and no
    /// entities — the state being diagnosed. On a healthy store they are
    /// always zero, not a measurement. `extracted_absent` is the exception;
    /// see its own note.
    #[serde(default)]
    pub unextracted_log_count: u64,
    /// Episodes with no `extracted` field at all. Schema version 2 gives
    /// every episode a value on open, so this is zero on a migrated store and
    /// a non-zero value means that migration did not land. These episodes are
    /// **not** pending work: the scan matches `extracted = false`, so it
    /// cannot see them at all, and only `graph migrate --force` will reach
    /// them. Unlike the other two fields this one is always computed — the
    /// `episode_extracted` index answers it with a count scan.
    #[serde(default)]
    pub extracted_absent: u64,
    /// Episodes with no `log_number` — not tied to an archive file, so the
    /// extraction scan cannot reach them by construction.
    /// Only computed in the zero-entity state; see `unextracted_log_count`.
    #[serde(default)]
    pub log_number_absent: u64,
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
    /// Omitted when absent — see [`Entity::embedding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    pub log_number: Option<i64>,
    /// Authorship class as stored. Absent on episodes written before
    /// provenance existed; read it through [`Episode::provenance`], which
    /// resolves absent and unrecognised values conservatively.
    #[serde(default)]
    pub provenance: Option<String>,
    /// How many times retrieval has returned this episode. Absent on episodes
    /// written before the counter existed — which reads as never retrieved.
    #[serde(default, deserialize_with = "super::util::count_or_zero")]
    pub access_count: i64,
}

impl Episode {
    #[must_use]
    pub fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// Who authored this episode. Legacy and unrecognised values resolve to
    /// [`Provenance::SelfGenerated`] — unlabelled text never earns full
    /// evidence weight.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        Provenance::from_stored(self.provenance.as_deref())
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

impl ExtractionResult {
    /// How many entries of every kind this result holds.
    #[must_use]
    pub fn element_count(&self) -> usize {
        self.entities.len()
            + self.relationships.len()
            + self.cases.len()
            + self.patterns.len()
            + self.preferences.len()
    }

    /// Move every entry of `other` onto the end of this result.
    pub fn append(&mut self, mut other: Self) {
        self.entities.append(&mut other.entities);
        self.relationships.append(&mut other.relationships);
        self.cases.append(&mut other.cases);
        self.patterns.append(&mut other.patterns);
        self.preferences.append(&mut other.preferences);
    }
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineDocuments {
    pub learning: String,
    pub thoughts: String,
    pub curiosity: String,
    pub reflections: String,
    pub praxis: String,
}

/// Report from a pipeline sync operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

/// What entity resolution did in one session.
///
/// Tier hits, whether the resolver ran, and the proposals it made that the
/// guard refused. Empty on a v1 ingest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveStats {
    pub mentions: u32,
    pub tier_name_kind: u32,
    pub tier_alias: u32,
    pub tier_name_any: u32,
    /// `1` when the resolver was called, `0` otherwise.
    pub resolver_called: u32,
    pub accepted: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedMatch>,
    /// Truncation and a resolver that could not be called. The leftovers
    /// were still written as new entities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ResolveStats {
    /// Nothing was resolved. v1 reports stay bare.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.mentions == 0
            && self.tier_name_kind == 0
            && self.tier_alias == 0
            && self.tier_name_any == 0
            && self.resolver_called == 0
            && self.accepted == 0
            && self.rejected.is_empty()
            && self.notes.is_empty()
    }
}

/// A resolver proposal the guard refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedMatch {
    pub mention: String,
    pub match_id: String,
}

/// One live fact closed while a session was written.
///
/// `reason` is `superseded`, `rejected`, `replaced`, or `promoted`.
/// `old_value` is the record that was closed. `new_value` is the fact that
/// closed it, empty when a rejection names no replacement. `key` is the
/// setting key that was closed, empty for an edge and for a slot that has
/// none, so two retirements of one value stay distinguishable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retirement {
    pub reason: String,
    pub subject: String,
    pub predicate: String,
    /// Absent in reports written before this field existed.
    #[serde(default)]
    pub key: String,
    pub old_value: String,
    pub new_value: String,
}

/// Monotonic durations of one pipeline v2 extraction.
///
/// `total_ms` wraps every stage, so it is at least [`StageTiming::pass1_ms`].
/// v1 leaves [`IngestionReport::timing`] empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTiming {
    /// The structured pass over the session windows.
    pub pass1_ms: u64,
    /// Entity resolution for the facts that pass kept.
    pub resolve_ms: u64,
    /// Edges, slots and retirements committed after resolution.
    pub write_ms: u64,
    /// From the start of the v2 extraction through the write.
    pub total_ms: u64,
    /// Known-row lookup plus reconcile. Absent when that switch is off, so a
    /// report from before this field, and a run that did not match, still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_ms: Option<u64>,
    /// Consolidation after the write. Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidate_ms: Option<u64>,
}

/// Counts from the post-pass-1 known-row matcher and the retirement guards.
///
/// Older bench JSON has no object here. A blank report is left out of new JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchReport {
    #[serde(default)]
    pub key_filled: u32,
    #[serde(default)]
    pub key_canonical: u32,
    #[serde(default)]
    pub edge_targets: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skipped: BTreeMap<String, u32>,
}

impl MatchReport {
    /// Nothing was filled, canonicalised, targeted or skipped.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.key_filled == 0
            && self.key_canonical == 0
            && self.edge_targets == 0
            && self.skipped.is_empty()
    }
}

/// How many candidate pairs the deterministic pre-filter settled.
///
/// Older bench JSON has no object here. A blank report is left out of new JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidatePrefilter {
    /// Same live value, same subject, same predicate: the later copy may close.
    #[serde(default)]
    pub dup: u32,
    /// A grounded replacement with one token partner and a departure cue.
    #[serde(default)]
    pub sup: u32,
    /// Left for the ordinary supersede path, or equal values that must not close.
    #[serde(default)]
    pub drop: u32,
}

/// Counts from the opt-in consolidation pass.
///
/// Older bench JSON has no object here. A blank report is left out of new JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateReport {
    #[serde(default)]
    pub anchors: u32,
    #[serde(default)]
    pub pairs: u32,
    #[serde(default)]
    pub overflow: u32,
    #[serde(default)]
    pub prefilter: ConsolidatePrefilter,
    #[serde(default)]
    pub judged: u32,
    #[serde(default)]
    pub cache_hits: u32,
    #[serde(default)]
    pub closes_sup: u32,
    #[serde(default)]
    pub closes_dup: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skips: BTreeMap<String, u32>,
    /// Judge verdicts a guard did not apply, keyed by reason. Empty, and
    /// omitted, when nothing was dropped. Absent on a report from before
    /// this field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dropped: BTreeMap<String, u32>,
    #[serde(default)]
    pub calls: u32,
    #[serde(default)]
    pub errors: u32,
    /// Times a sibling link made the cue true. Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_sibling: Option<u32>,
    /// Times a derived-from link made the cue true. Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_derived: Option<u32>,
    /// Times the trailing numeric quote made the cue true. Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_trailing: Option<u32>,
    /// Closes whose partner subject differs from the anchor, excluding closes
    /// kept by `consolidate_key_rename` (counted under `rename_*`).
    /// Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closes_component: Option<u32>,
    /// Closes kept by a same-subject key rename. Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_key: Option<u32>,
    /// Closes kept by a "becomes" replacement quote. Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_becomes: Option<u32>,
    /// Closes kept because a rule names an entity the writer rejected.
    /// Absent when that switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_entity_drop: Option<u32>,
    /// Closes kept because a unique `replaces` match supplied the cue.
    /// Absent when zero, including when the switch is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_cue: Option<u32>,
    /// Closes in [`Self::replaces_cue`] that needed the relaxed quote floor.
    /// Absent when zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_floor: Option<u32>,
    /// Pairs added by scoring `tokens(replaces)` that reached the judge.
    /// Absent when zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_via_replaces: Option<u32>,
}

/// Counts from the opt-in contradiction hold.
///
/// Older bench JSON has no object here. A blank report is left out of new JSON.
/// The hold runs only when consolidation is also on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldReport {
    #[serde(default)]
    pub holds_opened: u32,
    #[serde(default)]
    pub holds_confirmed: u32,
    #[serde(default)]
    pub holds_dropped: u32,
    #[serde(default)]
    pub holds_ambiguous: u32,
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub errors: u32,
}

impl HoldReport {
    /// Nothing was opened, resolved, left pending, or failed.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.holds_opened == 0
            && self.holds_confirmed == 0
            && self.holds_dropped == 0
            && self.holds_ambiguous == 0
            && self.errors == 0
    }
}

/// Counts from create-time derived-from links.
///
/// Older bench JSON has no object here. A blank report is left out of new JSON.
/// The counters are filled only when consolidation and the link switch are both on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinksReport {
    #[serde(default)]
    pub links_written: u32,
    #[serde(default)]
    pub premises_recorded: u32,
    #[serde(default)]
    pub fan_in_overflow: u32,
    /// Extra anchors added from a writer close of a linked premise.
    #[serde(default)]
    pub d_anchors_added: u32,
    /// Dependents past the four-anchor cap.
    #[serde(default)]
    pub d_anchor_overflow: u32,
}

impl LinksReport {
    /// No list was stored and no derived anchor was added or dropped.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.links_written == 0
            && self.premises_recorded == 0
            && self.fan_in_overflow == 0
            && self.d_anchors_added == 0
            && self.d_anchor_overflow == 0
    }
}

/// Counts for a component `part_of` the session project.
///
/// Older bench JSON has no object here. A blank report is left out of new JSON.
/// The counters are filled only when `ground_component_edges` is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentEdgesReport {
    #[serde(default)]
    pub component_edges_grounded: u32,
    #[serde(default)]
    pub component_edges_kept_object: u32,
    #[serde(default)]
    pub component_edge_technology: u32,
    #[serde(default)]
    pub component_edge_foreign: u32,
    /// Seeds whose incoming decided `part_of` page came back full.
    #[serde(default)]
    pub part_of_children_truncated: u32,
}

impl ComponentEdgesReport {
    /// Nothing was grounded, kept, or skipped.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.component_edges_grounded == 0
            && self.component_edges_kept_object == 0
            && self.component_edge_technology == 0
            && self.component_edge_foreign == 0
            && self.part_of_children_truncated == 0
    }

    pub(crate) fn add(&mut self, other: Self) {
        self.component_edges_grounded = self
            .component_edges_grounded
            .saturating_add(other.component_edges_grounded);
        self.component_edges_kept_object = self
            .component_edges_kept_object
            .saturating_add(other.component_edges_kept_object);
        self.component_edge_technology = self
            .component_edge_technology
            .saturating_add(other.component_edge_technology);
        self.component_edge_foreign = self
            .component_edge_foreign
            .saturating_add(other.component_edge_foreign);
        self.part_of_children_truncated = self
            .part_of_children_truncated
            .saturating_add(other.part_of_children_truncated);
    }
}

impl ConsolidateReport {
    /// The pass did not run, or it found nothing to count.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.pass_blank()
            && self.outcome_blank()
            && self.component_counts_blank()
            && self.rename_counts_blank()
            && self.replaces_counts_blank()
    }

    fn pass_blank(&self) -> bool {
        self.anchors == 0
            && self.pairs == 0
            && self.overflow == 0
            && self.prefilter == ConsolidatePrefilter::default()
            && self.judged == 0
            && self.cache_hits == 0
    }

    fn outcome_blank(&self) -> bool {
        self.closes_sup == 0
            && self.closes_dup == 0
            && self.skips.is_empty()
            && self.dropped.is_empty()
            && self.calls == 0
            && self.errors == 0
    }

    fn component_counts_blank(&self) -> bool {
        self.link_sibling.unwrap_or(0) == 0
            && self.link_derived.unwrap_or(0) == 0
            && self.quote_trailing.unwrap_or(0) == 0
            && self.closes_component.unwrap_or(0) == 0
    }

    fn rename_counts_blank(&self) -> bool {
        self.rename_key.unwrap_or(0) == 0
            && self.rename_becomes.unwrap_or(0) == 0
            && self.rename_entity_drop.unwrap_or(0) == 0
    }

    fn replaces_counts_blank(&self) -> bool {
        self.replaces_cue.unwrap_or(0) == 0
            && self.replaces_floor.unwrap_or(0) == 0
            && self.pair_via_replaces.unwrap_or(0) == 0
    }
}

/// Result of a full ingestion run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestionReport {
    pub episodes_created: u32,
    pub entities_created: u32,
    pub entities_merged: u32,
    pub entities_skipped: u32,
    pub relationships_created: u32,
    pub relationships_skipped: u32,
    pub errors: Vec<String>,
    /// Chunks the archive was split into for extraction.
    #[serde(default)]
    pub chunks_total: u32,
    /// Chunks whose extraction call failed (spawn, exit status, empty output,
    /// unparseable answer). Together with `chunks_total` this is what says
    /// "the provider is down" — graph yield cannot, because an archive that
    /// only restates known facts yields nothing and is still a success.
    #[serde(default)]
    pub chunks_failed: u32,
    /// Tokens *estimated* for the calls whose provider reported nothing —
    /// claude-code's prose output, a bridge with no counters, any custom CLI
    /// without `[llm.cli] usage_input_path`.
    pub estimated_tokens: u64,
    /// Tokens the providers actually reported, summed over the calls that
    /// reported them. Kept apart from the estimate so a displayed number never
    /// has to claim more precision than it has.
    /// Pass 1 only, so input plus output can exceed it when the resolver ran.
    #[serde(default)]
    pub measured_tokens: u64,
    /// Prompt tokens providers reported. `None` when no call reported usage.
    /// A provider reporting only one side records the other as 0, not null,
    /// matching [`crate::graph::llm::TokenUsage::from_counts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Completion tokens providers reported. `None` when no call reported usage.
    /// A provider reporting only one side records the other as 0, not null,
    /// matching [`crate::graph::llm::TokenUsage::from_counts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Record IDs of the entities this run created or merged into — the
    /// entities the session touched, and so the ones a session outcome
    /// applies to. Empty when the run had no LLM to extract with.
    #[serde(default)]
    pub entity_ids: Vec<String>,
    /// Candidates that reached the ambiguous band and cost one model call each.
    /// The dedup half of the ingest bill.
    #[serde(default)]
    pub dedup_llm_calls: u32,
    /// Candidates resolved by name match or by a similarity band — decided
    /// without a model call.
    #[serde(default)]
    pub dedup_fast_path: u32,
    /// Windows the second pipeline sent to the provider.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub windows: u32,
    /// Provider calls the second pipeline made, including a parse retry.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub llm_calls: u32,
    /// Facts, bugs and releases the model returned, before the schema table.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub candidates: u32,
    /// Candidates that survived the table and were handed to the writer.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub kept: u32,
    /// Literal facts appended as slots. A reinforced slot is not counted again.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub slots_written: u32,
    /// Candidates dropped, keyed by the reason.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub drops: HashMap<String, u32>,
    /// Entity resolution for this session. Blank on v1.
    #[serde(default, skip_serializing_if = "ResolveStats::is_blank")]
    pub resolve: ResolveStats,
    /// Facts this session closed, in the order it closed them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retirements: Vec<Retirement>,
    /// Retirements that could not be applied. The session still commits.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub retirements_skipped: u32,
    /// Why a row that would have closed was left live. Empty, and omitted,
    /// when nothing was skipped this way. Older bench JSON has no field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub retirement_skips: BTreeMap<String, u32>,
    /// Stage durations for pipeline v2. Absent on v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<StageTiming>,
    /// Known-row match counts. Blank, and omitted, when the switch is off.
    #[serde(default, skip_serializing_if = "MatchReport::is_blank")]
    pub known_match: MatchReport,
    /// Consolidation counts. Blank, and omitted, when the switch is off.
    #[serde(default, skip_serializing_if = "ConsolidateReport::is_blank")]
    pub consolidate: ConsolidateReport,
    /// Contradiction-hold counts. Blank, and omitted, when the hold is off.
    #[serde(default, skip_serializing_if = "HoldReport::is_blank")]
    pub hold: HoldReport,
    /// Derived-from counts. Blank, and omitted, when that switch is off.
    #[serde(default, skip_serializing_if = "LinksReport::is_blank")]
    pub links: LinksReport,
    /// Component `part_of` counts. Blank, and omitted, when that switch is off.
    #[serde(default, skip_serializing_if = "ComponentEdgesReport::is_blank")]
    pub component_edges: ComponentEdgesReport,
    /// Raw-turn values missing from the store. Omitted when the switch is off
    /// or the list is empty and nothing overflowed. These items are not stored.
    #[serde(default, skip_serializing_if = "RecoveredReport::is_blank")]
    pub recovered: RecoveredReport,
}

/// One live row that shares key tokens with a recovered value.
///
/// Shown so a missing value is not read as a stored fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredNearest {
    pub subject: String,
    pub key: String,
    pub value: String,
}

/// A candidate value from a text turn that is not a live stored value.
///
/// `quote` is the sentence, clipped to 200 characters. `nearest` is omitted
/// when no single live row is closer than the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredItem {
    pub turn: usize,
    pub role: String,
    pub pattern: String,
    pub key: String,
    pub value: String,
    pub quote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest: Option<RecoveredNearest>,
}

/// Missing raw-turn values for one session.
///
/// `redundant` counts hits whose value is already live. Those hits are not
/// listed. `overflow` counts hits past the per-turn or per-session cap.
/// A blank report is left out of JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredReport {
    #[serde(default)]
    pub items: Vec<RecoveredItem>,
    #[serde(default)]
    pub redundant: u32,
    #[serde(default)]
    pub overflow: u32,
}

impl RecoveredReport {
    /// No listed item and nothing past the cap.
    ///
    /// A redundant count alone is left out of JSON. The list is what was missing.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.items.is_empty() && self.overflow == 0
    }
}

fn u32_is_zero(value: &u32) -> bool {
    *value == 0
}

impl IngestionReport {
    /// Every token this run is believed to have spent, measured and estimated
    /// alike. The number to budget against; the two parts are what to display.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.measured_tokens + self.estimated_tokens
    }

    /// Every extraction call failed: the provider could not spawn, exited
    /// non-zero, answered nothing, or answered garbage — for *each* chunk.
    ///
    /// This is the run that must not be recorded as done. Marking it would
    /// make a broken provider look like an empty archive, and nothing would
    /// ever retry it. A partial failure (some chunks parsed) is a success
    /// with warnings for v1, and an archive with no chunks is simply empty.
    #[must_use]
    pub fn is_total_failure(&self) -> bool {
        self.chunks_total > 0 && self.chunks_failed == self.chunks_total
    }

    /// The session must stay pending.
    ///
    /// v1 stays on [`IngestionReport::is_total_failure`]: a partial chunk
    /// failure is still marked. v2 (`windows > 0`) leaves the session pending
    /// when any window failed, so that window can be extracted later.
    #[must_use]
    pub fn leaves_pending(&self) -> bool {
        self.is_total_failure() || (self.windows > 0 && self.chunks_failed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(total: u32, failed: u32) -> IngestionReport {
        IngestionReport {
            chunks_total: total,
            chunks_failed: failed,
            errors: (0..failed)
                .map(|i| format!("extraction chunk {i}: boom"))
                .collect(),
            ..IngestionReport::default()
        }
    }

    #[test]
    fn every_chunk_failing_is_a_total_failure() {
        assert!(chunks(1, 1).is_total_failure());
        assert!(chunks(7, 7).is_total_failure());
    }

    #[test]
    fn one_surviving_chunk_makes_it_a_partial_success() {
        assert!(!chunks(7, 6).is_total_failure());
        assert!(!chunks(2, 0).is_total_failure());
    }

    #[test]
    fn an_archive_that_restated_known_facts_is_not_a_failure() {
        // Every entity resolved as Skipped, one dedup call timed out: the
        // provider answered every chunk, so this is success with a warning.
        let report = IngestionReport {
            entities_skipped: 5,
            relationships_skipped: 2,
            errors: vec!["dedup 'x': timeout".into()],
            ..chunks(3, 0)
        };
        assert!(!report.is_total_failure());
    }

    #[test]
    fn an_archive_with_no_chunks_is_not_a_failure() {
        assert!(!chunks(0, 0).is_total_failure());
    }

    #[test]
    fn a_v1_report_carries_no_stage_timing() {
        assert!(IngestionReport::default().timing.is_none());
    }

    #[test]
    fn older_json_loads_without_match_fields_and_a_zero_duration_is_kept() {
        let timing: StageTiming =
            serde_json::from_str(r#"{"pass1_ms":1,"resolve_ms":2,"write_ms":3,"total_ms":6}"#)
                .unwrap();
        assert_eq!(timing.match_ms, None);
        let present = StageTiming {
            match_ms: Some(0),
            ..timing
        };
        let encoded = serde_json::to_value(present).unwrap();
        assert_eq!(encoded["match_ms"], 0);

        let value = serde_json::to_value(IngestionReport::default()).unwrap();
        assert!(value.get("known_match").is_none());
        assert!(value.get("consolidate").is_none());
        assert!(value["timing"].get("consolidate_ms").is_none());

        let mut filled = IngestionReport::default();
        filled.known_match.key_filled = 2;
        filled.known_match.skipped.insert("ambiguous".into(), 1);
        filled.timing = Some(present);
        let mut value = serde_json::to_value(&filled).unwrap();
        assert_eq!(value["known_match"]["key_filled"], 2);
        assert_eq!(value["known_match"]["skipped"]["ambiguous"], 1);
        assert_eq!(value["timing"]["match_ms"], 0);
        value.as_object_mut().unwrap().remove("known_match");
        value["timing"].as_object_mut().unwrap().remove("match_ms");
        let loaded: IngestionReport = serde_json::from_value(value).unwrap();
        assert!(loaded.known_match.is_blank());
        assert_eq!(loaded.timing.unwrap().match_ms, None);
        assert_eq!(loaded.entities_created, 0);
        assert!(loaded.retirement_skips.is_empty());
    }

    #[test]
    fn consolidate_drop_counters_are_omitted_when_empty() {
        let encoded = serde_json::to_value(ConsolidateReport::default()).unwrap();
        assert!(encoded.get("dropped").is_none());
        assert!(encoded.get("skips").is_none());
        let mut report = ConsolidateReport::default();
        report.dropped.insert("quote too short".into(), 1);
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["dropped"]["quote too short"], 1);
        assert!(!report.is_blank());
        assert!(ConsolidateReport::default().is_blank());
        let blank = serde_json::to_value(IngestionReport::default()).unwrap();
        assert!(blank.get("consolidate").is_none());
    }

    #[test]
    fn component_counters_are_omitted_until_the_switch_applies() {
        let encoded = serde_json::to_value(ConsolidateReport::default()).unwrap();
        assert!(encoded.get("link_sibling").is_none());
        assert!(encoded.get("link_derived").is_none());
        assert!(encoded.get("quote_trailing").is_none());
        assert!(encoded.get("closes_component").is_none());
        let mut report = ConsolidateReport {
            link_sibling: Some(0),
            link_derived: Some(1),
            quote_trailing: Some(3),
            closes_component: Some(2),
            ..ConsolidateReport::default()
        };
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["link_sibling"], 0);
        assert_eq!(encoded["link_derived"], 1);
        assert_eq!(encoded["quote_trailing"], 3);
        assert_eq!(encoded["closes_component"], 2);
        assert!(!report.is_blank());
        report.link_derived = Some(0);
        report.quote_trailing = Some(0);
        report.closes_component = Some(0);
        assert!(report.is_blank());
    }

    #[test]
    fn rename_counters_are_omitted_until_the_switch_applies() {
        let encoded = serde_json::to_value(ConsolidateReport::default()).unwrap();
        assert!(encoded.get("rename_key").is_none());
        assert!(encoded.get("rename_becomes").is_none());
        assert!(encoded.get("rename_entity_drop").is_none());
        let mut report = ConsolidateReport {
            rename_key: Some(0),
            rename_becomes: Some(0),
            rename_entity_drop: Some(0),
            ..ConsolidateReport::default()
        };
        assert!(report.is_blank());
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["rename_key"], 0);
        assert_eq!(encoded["rename_becomes"], 0);
        assert_eq!(encoded["rename_entity_drop"], 0);
        report.rename_key = Some(1);
        assert!(!report.is_blank());
        report.rename_key = Some(0);
        report.rename_becomes = Some(2);
        assert!(!report.is_blank());
        report.rename_becomes = Some(0);
        report.rename_entity_drop = Some(3);
        assert!(!report.is_blank());
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["rename_entity_drop"], 3);
    }

    #[test]
    fn replaces_counters_are_omitted_when_zero() {
        let encoded = serde_json::to_value(ConsolidateReport::default()).unwrap();
        assert!(encoded.get("replaces_cue").is_none());
        assert!(encoded.get("replaces_floor").is_none());
        assert!(encoded.get("pair_via_replaces").is_none());
        let mut report = ConsolidateReport {
            replaces_cue: Some(1),
            replaces_floor: Some(1),
            pair_via_replaces: Some(1),
            ..ConsolidateReport::default()
        };
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["replaces_cue"], 1);
        assert_eq!(encoded["replaces_floor"], 1);
        assert_eq!(encoded["pair_via_replaces"], 1);
        assert!(!report.is_blank());
        report.replaces_floor = None;
        report.replaces_cue = None;
        assert!(!report.is_blank());
        report.pair_via_replaces = None;
        assert!(report.is_blank());
        let encoded = serde_json::to_value(&report).unwrap();
        assert!(encoded.get("replaces_cue").is_none());
        assert!(encoded.get("replaces_floor").is_none());
        assert!(encoded.get("pair_via_replaces").is_none());
    }

    #[test]
    fn links_report_is_omitted_when_blank_and_older_json_loads() {
        let encoded = serde_json::to_value(IngestionReport::default()).unwrap();
        assert!(encoded.get("links").is_none());
        assert!(LinksReport::default().is_blank());

        let mut report = IngestionReport::default();
        report.links.links_written = 1;
        report.links.premises_recorded = 2;
        report.links.fan_in_overflow = 3;
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["links"]["links_written"], 1);
        assert_eq!(encoded["links"]["premises_recorded"], 2);
        assert_eq!(encoded["links"]["fan_in_overflow"], 3);
        assert_eq!(encoded["links"]["d_anchors_added"], 0);
        assert_eq!(encoded["links"]["d_anchor_overflow"], 0);
        report.links = LinksReport {
            d_anchors_added: 4,
            d_anchor_overflow: 1,
            ..LinksReport::default()
        };
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["links"]["d_anchors_added"], 4);
        assert_eq!(encoded["links"]["d_anchor_overflow"], 1);
        assert!(!report.links.is_blank());

        let mut value = encoded;
        value.as_object_mut().unwrap().remove("links");
        let loaded: IngestionReport = serde_json::from_value(value).unwrap();
        assert!(loaded.links.is_blank());
        assert_eq!(loaded.entities_created, 0);
    }

    #[test]
    fn component_edges_report_is_omitted_when_blank_and_older_json_loads() {
        let encoded = serde_json::to_value(IngestionReport::default()).unwrap();
        assert!(encoded.get("component_edges").is_none());
        assert!(ComponentEdgesReport::default().is_blank());

        let mut report = IngestionReport::default();
        report.component_edges.component_edges_grounded = 1;
        report.component_edges.component_edges_kept_object = 2;
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["component_edges"]["component_edges_grounded"], 1);
        assert_eq!(encoded["component_edges"]["component_edges_kept_object"], 2);
        assert_eq!(encoded["component_edges"]["component_edge_technology"], 0);
        assert_eq!(encoded["component_edges"]["component_edge_foreign"], 0);
        assert_eq!(encoded["component_edges"]["part_of_children_truncated"], 0);
        report.component_edges.add(ComponentEdgesReport {
            component_edge_technology: 3,
            component_edge_foreign: 4,
            ..ComponentEdgesReport::default()
        });
        assert_eq!(report.component_edges.component_edges_grounded, 1);
        assert_eq!(report.component_edges.component_edge_technology, 3);
        assert_eq!(report.component_edges.component_edge_foreign, 4);
        assert!(!report.component_edges.is_blank());

        let mut value = serde_json::to_value(IngestionReport::default()).unwrap();
        value.as_object_mut().unwrap().remove("component_edges");
        let loaded: IngestionReport = serde_json::from_value(value).unwrap();
        assert!(loaded.component_edges.is_blank());
    }

    #[test]
    fn recovered_report_is_omitted_when_blank_and_older_json_loads() {
        let encoded = serde_json::to_value(IngestionReport::default()).unwrap();
        assert!(encoded.get("recovered").is_none());
        assert!(RecoveredReport::default().is_blank());

        let mut report = IngestionReport::default();
        report.recovered.redundant = 3;
        assert!(report.recovered.is_blank());
        let encoded = serde_json::to_value(&report).unwrap();
        assert!(encoded.get("recovered").is_none(), "{encoded}");
        report.recovered = RecoveredReport::default();
        report.recovered.overflow = 2;
        assert!(!report.recovered.is_blank());
        report.recovered = RecoveredReport {
            items: vec![RecoveredItem {
                turn: 0,
                role: "user".into(),
                pattern: "is_now_value".into(),
                key: "jobs".into(),
                value: "4".into(),
                quote: "Default jobs is now 4.".into(),
                nearest: None,
            }],
            redundant: 0,
            overflow: 0,
        };
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["recovered"]["items"][0]["value"], "4");
        assert!(encoded["recovered"]["items"][0].get("nearest").is_none());

        let mut value = serde_json::to_value(IngestionReport::default()).unwrap();
        value.as_object_mut().unwrap().remove("recovered");
        let loaded: IngestionReport = serde_json::from_value(value).unwrap();
        assert!(loaded.recovered.is_blank());
    }

    #[test]
    fn retirement_skips_round_trip_and_older_json_still_loads() {
        let mut report = IngestionReport::default();
        report.retirement_skips.insert("enumeration".into(), 2);
        report.drops.insert("enumeration".into(), 1);
        let mut value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["retirement_skips"]["enumeration"], 2);
        assert_eq!(value["drops"]["enumeration"], 1);
        value.as_object_mut().unwrap().remove("retirement_skips");
        value.as_object_mut().unwrap().remove("drops");
        let loaded: IngestionReport = serde_json::from_value(value).unwrap();
        assert!(loaded.retirement_skips.is_empty());
        assert!(loaded.drops.is_empty());
        let encoded = serde_json::to_value(IngestionReport::default()).unwrap();
        assert!(encoded.get("retirement_skips").is_none());
    }

    #[test]
    fn a_failed_v2_window_leaves_the_session_pending() {
        let mut partial = chunks(2, 1);
        assert!(!partial.leaves_pending());
        partial.windows = 2;
        assert!(partial.leaves_pending());
        assert!(!partial.is_total_failure());

        let mut total = chunks(2, 2);
        total.windows = 2;
        assert!(total.leaves_pending());
        assert!(!chunks(3, 0).leaves_pending());
    }

    fn episode_with(embedding: Option<Vec<f32>>) -> Episode {
        Episode {
            id: serde_json::json!("episode:one"),
            session_id: "s1".into(),
            timestamp: serde_json::json!("2026-01-01T00:00:00Z"),
            abstract_text: "a".into(),
            overview: None,
            content: None,
            embedding,
            log_number: Some(1),
            provenance: None,
            access_count: 0,
        }
    }

    /// Embeddings are 384 floats each. They exist to be searched inside the
    /// store, never to be shipped to a caller that discards them.
    #[test]
    fn an_episode_without_an_embedding_carries_no_embedding_field() {
        let json = serde_json::to_value(episode_with(None)).unwrap();
        assert!(json.get("embedding").is_none(), "{json}");

        let json = serde_json::to_value(episode_with(Some(vec![0.5; 384]))).unwrap();
        assert!(json.get("embedding").is_some(), "{json}");
    }

    #[test]
    fn an_episode_round_trips_without_its_embedding() {
        let line = serde_json::to_string(&episode_with(None)).unwrap();
        let parsed: Episode = serde_json::from_str(&line).unwrap();
        assert!(parsed.embedding.is_none());
        assert_eq!(parsed.session_id, "s1");
    }

    #[test]
    fn pipeline_documents_survive_the_daemon_wire_format() {
        let docs = PipelineDocuments {
            learning: "# learning".into(),
            thoughts: "# thoughts".into(),
            curiosity: String::new(),
            reflections: "# reflections".into(),
            praxis: String::new(),
        };
        let line = serde_json::to_string(&docs).unwrap();
        assert_eq!(
            serde_json::from_str::<PipelineDocuments>(&line).unwrap(),
            docs
        );
    }
}
