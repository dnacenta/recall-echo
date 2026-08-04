//! recall-graph — Knowledge graph with semantic search for AI memory systems.
//!
//! Provides a structured graph layer (Layer 0) underneath flat-file memory systems.
//! Used by recall-echo (pulse-null entities) and recall-claude (Claude Code users).

pub mod confidence;
pub mod crud;
pub mod dedup;
pub mod embed;
pub mod error;
pub mod extract;
pub mod gc;
pub mod ingest;
pub mod llm;
pub mod pipeline;
pub mod pipeline_sync;
pub mod query;
pub mod search;
pub mod store;
pub mod traverse;
pub mod types;
pub mod util;
pub mod utility;
pub mod vigil_sync;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use confidence::{Provenance, ProvenanceWeights};
use embed::{FastEmbedder, LazyEmbedder};
use error::GraphError;
pub use ingest::{IngestContext, ProvenancePolicy};
use store::Db;
pub use store::ServerConfig;
#[allow(unused_imports)] // Required in scope for SurrealValue derive macro expansion
use surrealdb::types::SurrealValue;
use surrealdb::Surreal;
use types::*;

/// Take serde_json::Value results from a SurrealDB response and deserialize to a Rust type.
/// This avoids needing SurrealValue derive on complex types.
pub(crate) fn deserialize_take<T: serde::de::DeserializeOwned>(
    response: &mut surrealdb::IndexedResults,
    index: usize,
) -> Result<Vec<T>, GraphError> {
    let values: Vec<serde_json::Value> = response.take(index)?;
    values
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(GraphError::from))
        .collect()
}

pub(crate) fn deserialize_take_opt<T: serde::de::DeserializeOwned>(
    response: &mut surrealdb::IndexedResults,
    index: usize,
) -> Result<Option<T>, GraphError> {
    let values: Vec<T> = deserialize_take(response, index)?;
    Ok(values.into_iter().next())
}

/// The main entry point for graph memory operations.
pub struct GraphMemory {
    db: Surreal<Db>,
    embedder: LazyEmbedder,
    path: PathBuf,
    scoring: crate::config::GraphScoringConfig,
    provenance: confidence::ProvenanceWeights,
}

impl GraphMemory {
    /// Open a graph store at the given path.
    ///
    /// The backend is chosen at runtime from the `[graph] mode` key of
    /// `.recall-echo.toml` in the parent directory (memory_dir):
    /// `embedded` (default) opens SurrealKV at `path/surreal/`; `server`
    /// connects to a SurrealDB server via the configured URL.
    /// The `path` is used for the FastEmbed models cache in both modes.
    pub async fn open(path: &Path) -> Result<Self, GraphError> {
        let memory_dir = path.parent().unwrap_or(path);
        let config = crate::config::load_from_dir(memory_dir);
        let mode = config
            .graph
            .as_ref()
            .map(|g| g.mode.clone())
            .unwrap_or_else(|| "embedded".to_string());

        match mode.as_str() {
            "server" => Self::open_server(path).await,
            _ => Self::open_embedded(path).await,
        }
    }

    /// Open the embedded SurrealKV store at `path/surreal/`.
    pub async fn open_embedded(path: &Path) -> Result<Self, GraphError> {
        std::fs::create_dir_all(path)?;

        let db = store::open(path).await?;
        store::init_schema(&db).await?;

        let models_dir = path.join("models");
        std::fs::create_dir_all(&models_dir)?;
        let embedder = LazyEmbedder::new(&models_dir);

        let graph_config = load_graph_section(path);

        Ok(Self {
            db,
            embedder,
            path: path.to_path_buf(),
            scoring: graph_config.scoring,
            provenance: graph_config.provenance,
        })
    }

    /// Connect to a SurrealDB server using `[graph]` settings from
    /// `.recall-echo.toml` in the parent directory (memory_dir).
    /// The `path` is still used for the FastEmbed models cache.
    pub async fn open_server(path: &Path) -> Result<Self, GraphError> {
        let memory_dir = path.parent().unwrap_or(path);
        let config = crate::config::load_from_dir(memory_dir);

        let graph_section = config.graph.unwrap_or_default();
        let password = if graph_section.password_file.is_empty() {
            String::new()
        } else {
            let pw_path = if graph_section.password_file.starts_with('/') {
                std::path::PathBuf::from(&graph_section.password_file)
            } else {
                // Relative to entity root (memory_dir's parent)
                let entity_root = memory_dir.parent().unwrap_or(memory_dir);
                entity_root.join(&graph_section.password_file)
            };
            std::fs::read_to_string(&pw_path)
                .map(|s| s.trim().to_string())
                .map_err(|e| {
                    GraphError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "failed to read graph password file {}: {e}",
                            pw_path.display()
                        ),
                    ))
                })?
        };

        let scoring = graph_section.scoring.clone();
        let provenance = graph_section.provenance;
        let server_config = store::ServerConfig {
            url: graph_section.url,
            username: graph_section.username,
            password,
            namespace: graph_section.namespace,
            database: graph_section.database,
        };

        let models_dir = path.join("models");
        let mut gm = Self::connect(&server_config, &models_dir).await?;
        gm.scoring = scoring;
        gm.provenance = provenance;
        Ok(gm)
    }

    /// Connect to a SurrealDB server over WebSocket with explicit config.
    pub async fn connect(
        config: &store::ServerConfig,
        models_dir: &Path,
    ) -> Result<Self, GraphError> {
        let db = store::connect(config).await?;
        store::init_schema(&db).await?;

        std::fs::create_dir_all(models_dir)?;
        let embedder = LazyEmbedder::new(models_dir);

        Ok(Self {
            db,
            embedder,
            path: models_dir.to_path_buf(),
            scoring: crate::config::GraphScoringConfig::default(),
            provenance: confidence::ProvenanceWeights::default(),
        })
    }

    /// Path to the graph store.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Evidence weights this store applies to observations, by provenance
    /// class (`[graph.provenance]`).
    #[must_use]
    pub fn provenance_weights(&self) -> &confidence::ProvenanceWeights {
        &self.provenance
    }

    /// Internal access to the database handle.
    #[allow(dead_code)]
    pub(crate) fn db(&self) -> &Surreal<Db> {
        &self.db
    }

    /// Internal access to the embedder (initializes it on first use).
    #[allow(dead_code)]
    pub(crate) fn embedder(&self) -> Result<&FastEmbedder, GraphError> {
        self.embedder.get()
    }

    // --- Entity CRUD ---

    /// Add a new entity to the graph.
    pub async fn add_entity(&self, entity: NewEntity) -> Result<Entity, GraphError> {
        crud::add_entity(&self.db, self.embedder.get()?, entity).await
    }

    /// Get an entity by name.
    pub async fn get_entity(&self, name: &str) -> Result<Option<Entity>, GraphError> {
        crud::get_entity_by_name(&self.db, name).await
    }

    /// Get an entity by its record ID.
    pub async fn get_entity_by_id(&self, id: &str) -> Result<Option<Entity>, GraphError> {
        crud::get_entity_by_id(&self.db, id).await
    }

    /// Update an entity's fields.
    pub async fn update_entity(
        &self,
        id: &str,
        updates: EntityUpdate,
    ) -> Result<Entity, GraphError> {
        crud::update_entity(&self.db, self.embedder.get()?, id, updates).await
    }

    /// Delete an entity and its relationships.
    pub async fn delete_entity(&self, id: &str) -> Result<(), GraphError> {
        crud::delete_entity(&self.db, id).await
    }

    /// List all entities, optionally filtered by type.
    pub async fn list_entities(
        &self,
        entity_type: Option<&str>,
    ) -> Result<Vec<Entity>, GraphError> {
        crud::list_entities(&self.db, entity_type).await
    }

    // --- Relationships ---

    /// Create a relationship between two named entities.
    pub async fn add_relationship(&self, rel: NewRelationship) -> Result<Relationship, GraphError> {
        crud::add_relationship(&self.db, rel).await
    }

    /// Get relationships for an entity.
    pub async fn get_relationships(
        &self,
        entity_name: &str,
        direction: Direction,
    ) -> Result<Vec<Relationship>, GraphError> {
        crud::get_relationships(&self.db, entity_name, direction).await
    }

    /// Supersede a relationship: close the old one, create a new one.
    pub async fn supersede_relationship(
        &self,
        old_id: &str,
        new: NewRelationship,
    ) -> Result<Relationship, GraphError> {
        crud::supersede_relationship(&self.db, old_id, new).await
    }

    /// Overwrite a relationship's confidence, resetting its evidence to the
    /// prior around the new mean.
    pub async fn update_relationship_confidence(
        &self,
        rel_id: &str,
        confidence: f64,
    ) -> Result<(), GraphError> {
        crud::update_relationship_confidence(&self.db, rel_id, confidence).await
    }

    /// Persist updated evidence for a relationship and reset its decay clock.
    ///
    /// Called when a relationship is corroborated: the new posterior mean is
    /// stored as `confidence`, the coherence tally is stored beside it, and
    /// `last_reinforced` is set to now, preventing temporal decay from eroding
    /// the edge.
    pub async fn reinforce_relationship(
        &self,
        rel_id: &str,
        evidence: confidence::EdgeEvidence,
    ) -> Result<(), GraphError> {
        crud::reinforce_relationship(&self.db, rel_id, evidence).await
    }

    // --- Episodes ---

    /// Add a new episode authored by the agent itself.
    ///
    /// The conservative default: a caller that cannot say where the text came
    /// from must not have it counted as independent evidence. Ingestion, which
    /// does know, uses [`GraphMemory::add_episode_from`].
    pub async fn add_episode(&self, episode: NewEpisode) -> Result<Episode, GraphError> {
        crud::add_episode(&self.db, self.embedder.get()?, episode).await
    }

    /// Add a new episode stamped with the class of whoever authored it.
    pub async fn add_episode_from(
        &self,
        episode: NewEpisode,
        provenance: Provenance,
    ) -> Result<Episode, GraphError> {
        crud::add_episode_from(&self.db, self.embedder.get()?, episode, provenance).await
    }

    /// Get episodes by session ID.
    pub async fn get_episodes_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<Episode>, GraphError> {
        crud::get_episodes_by_session(&self.db, session_id).await
    }

    /// Get episode by log number.
    pub async fn get_episode_by_log_number(
        &self,
        log_number: u32,
    ) -> Result<Option<Episode>, GraphError> {
        crud::get_episode_by_log_number(&self.db, log_number).await
    }

    // --- Ingestion ---

    /// Ingest a conversation archive into the knowledge graph.
    ///
    /// The [`IngestContext`] carries the provenance policy: conversation
    /// archives infer per chunk from turn roles, document ingestion forces a
    /// class.
    pub async fn ingest_archive(
        &self,
        archive_text: &str,
        context: &IngestContext,
        llm: Option<&dyn llm::LlmProvider>,
    ) -> Result<IngestionReport, GraphError> {
        ingest::ingest_archive(self, archive_text, context, llm).await
    }

    /// Run LLM extraction on an archive without creating episodes.
    pub async fn extract_from_archive(
        &self,
        archive_text: &str,
        context: &IngestContext,
        llm: &dyn llm::LlmProvider,
    ) -> Result<IngestionReport, GraphError> {
        ingest::extract_from_archive(self, archive_text, context, llm).await
    }

    /// Mark all episodes with a given log_number as extracted.
    pub async fn mark_extracted(&self, log_number: u32) -> Result<(), GraphError> {
        crud::mark_episodes_extracted(&self.db, log_number).await
    }

    /// Get log numbers of episodes that have NOT been extracted.
    pub async fn unextracted_log_numbers(&self) -> Result<Vec<i64>, GraphError> {
        crud::get_unextracted_log_numbers(&self.db).await
    }

    // --- Search ---

    /// Semantic search across entities (legacy — returns full Entity).
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, GraphError> {
        search::search(&self.db, self.embedder.get()?, &self.scoring, query, limit).await
    }

    /// Search with options — L1 projections, type/keyword filters.
    pub async fn search_with_options(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<Vec<ScoredEntity>, GraphError> {
        search::search_with_options(
            &self.db,
            self.embedder.get()?,
            &self.scoring,
            query,
            options,
        )
        .await
    }

    /// Semantic search across episodes.
    pub async fn search_episodes(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EpisodeSearchResult>, GraphError> {
        search::search_episodes(&self.db, self.embedder.get()?, query, limit).await
    }

    // --- Hybrid Query ---

    /// Hybrid query: semantic + graph expansion + optional episode search.
    pub async fn query(
        &self,
        query_text: &str,
        options: &QueryOptions,
    ) -> Result<QueryResult, GraphError> {
        query::query(
            &self.db,
            self.embedder.get()?,
            &self.scoring,
            query_text,
            options,
        )
        .await
    }

    // --- Traversal ---

    /// Traverse the graph from a named entity.
    pub async fn traverse(
        &self,
        entity_name: &str,
        depth: u32,
    ) -> Result<TraversalNode, GraphError> {
        traverse::traverse(&self.db, entity_name, depth).await
    }

    /// Traverse with type filter.
    pub async fn traverse_filtered(
        &self,
        entity_name: &str,
        depth: u32,
        type_filter: Option<&str>,
    ) -> Result<TraversalNode, GraphError> {
        traverse::traverse_filtered(&self.db, entity_name, depth, type_filter).await
    }

    // --- Pipeline ---

    /// Sync pipeline documents into the graph.
    pub async fn sync_pipeline(
        &self,
        docs: &PipelineDocuments,
    ) -> Result<PipelineSyncReport, GraphError> {
        pipeline_sync::sync_pipeline(self, docs).await
    }

    /// Get pipeline stats from the graph.
    pub async fn pipeline_stats(
        &self,
        staleness_days: u32,
    ) -> Result<PipelineGraphStats, GraphError> {
        query::pipeline_stats(&self.db, staleness_days).await
    }

    /// Get pipeline entities by stage and optional status.
    pub async fn pipeline_entities(
        &self,
        stage: &str,
        status: Option<&str>,
    ) -> Result<Vec<EntityDetail>, GraphError> {
        query::pipeline_entities(&self.db, stage, status).await
    }

    /// Trace pipeline flow for an entity.
    pub async fn pipeline_flow(
        &self,
        entity_name: &str,
    ) -> Result<Vec<(EntityDetail, String, EntityDetail)>, GraphError> {
        query::pipeline_flow(&self.db, entity_name).await
    }

    // --- Vigil Sync ---

    /// Sync vigil signal vectors into the graph as Measurement entities.
    pub async fn sync_vigil_signals(
        &self,
        signals_path: &std::path::Path,
    ) -> Result<VigilSyncReport, GraphError> {
        vigil_sync::sync_vigil_signals(self, signals_path).await
    }

    /// Sync outcome records into the graph as Outcome entities.
    pub async fn sync_outcomes(
        &self,
        outcomes_path: &std::path::Path,
    ) -> Result<VigilSyncReport, GraphError> {
        vigil_sync::sync_outcomes(self, outcomes_path).await
    }

    /// Sync both vigil signals and outcomes in one call.
    pub async fn sync_vigil(
        &self,
        signals_path: &std::path::Path,
        outcomes_path: &std::path::Path,
    ) -> Result<VigilSyncReport, GraphError> {
        vigil_sync::sync_vigil(self, signals_path, outcomes_path).await
    }

    /// Record outcome feedback: link retrieved entities to a session outcome and
    /// update their `utility_score` via EMA. `used_entity_ids` distinguishes the
    /// entities the response actually leaned on (full alpha) from retrieved-but-
    /// unused (muted alpha). Pass `None` to treat all retrieved as used.
    pub async fn record_outcome_feedback(
        &self,
        session_id: &str,
        outcome: utility::OutcomeKind,
        retrieved_entity_ids: &[String],
        used_entity_ids: Option<&[String]>,
    ) -> Result<utility::FeedbackReport, GraphError> {
        utility::record_outcome_feedback(
            &self.db,
            session_id,
            outcome,
            retrieved_entity_ids,
            used_entity_ids,
        )
        .await
    }

    /// Apply an outcome to every entity a session touched.
    ///
    /// Resolves the session's entities from the `contributed_to` records
    /// ingestion left behind (falling back to the entities the session
    /// authored), then records the outcome and moves their utility scores.
    /// The report says which entities moved and where they landed.
    pub async fn record_session_outcome(
        &self,
        session_id: &str,
        outcome: utility::OutcomeKind,
    ) -> Result<utility::FeedbackReport, GraphError> {
        let session = utility::session_entities(&self.db, session_id).await?;
        if session.is_empty() {
            return Ok(utility::FeedbackReport::default());
        }

        utility::record_outcome_feedback(
            &self.db,
            session_id,
            outcome,
            &session.retrieved,
            Some(&session.used),
        )
        .await
    }

    /// Record that a session touched these entities, without judging it.
    pub async fn record_session_use(
        &self,
        session_id: &str,
        entity_ids: &[String],
    ) -> Result<u32, GraphError> {
        utility::record_session_use(&self.db, session_id, entity_ids).await
    }

    // --- Garbage Collection ---

    /// Run garbage collection with the given config.
    pub async fn run_gc(&self, config: &gc::GcConfig) -> Result<gc::GcReport, GraphError> {
        gc::run_gc(&self.db, config).await
    }

    /// Get GC health stats without running collection.
    pub async fn gc_stats(&self) -> Result<gc::GcStatsReport, GraphError> {
        gc::stats_only(&self.db).await
    }

    /// Delete a single relationship by ID.
    pub async fn delete_relationship(&self, id: &str) -> Result<(), GraphError> {
        crud::delete_relationship(&self.db, id).await
    }

    // --- Stats ---

    /// Get graph statistics.
    pub async fn stats(&self) -> Result<GraphStats, GraphError> {
        let entity_count = db_count(&self.db, "entity").await?;
        let relationship_count = db_count(&self.db, "relates_to").await?;
        let episode_count = db_count(&self.db, "episode").await?;

        // Count by type
        let mut type_response = self
            .db
            .query("SELECT entity_type, count() AS count FROM entity GROUP BY entity_type")
            .await?;

        let type_rows: Vec<TypeCount> = type_response.take(0)?;
        let entity_type_counts: HashMap<String, u64> = type_rows
            .into_iter()
            .map(|r| (r.entity_type, r.count))
            .collect();

        Ok(GraphStats {
            entity_count,
            relationship_count,
            episode_count,
            entity_type_counts,
        })
    }
}

/// Load `[graph]` from `.recall-echo.toml` in the memory directory (the parent
/// of the graph store path). Returns defaults if the config file or the
/// section is absent, preserving legacy behavior.
fn load_graph_section(graph_path: &Path) -> crate::config::GraphSection {
    let memory_dir = graph_path.parent().unwrap_or(graph_path);
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
}

async fn db_count(db: &Surreal<Db>, table: &str) -> Result<u64, GraphError> {
    let query = format!("SELECT count() AS count FROM {table} GROUP ALL");
    let mut response = db.query(&query).await?;
    let rows: Vec<CountRow> = response.take(0)?;
    Ok(rows.first().map(|r| r.count).unwrap_or(0))
}

#[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
struct CountRow {
    count: u64,
}

#[derive(serde::Deserialize, surrealdb::types::SurrealValue)]
struct TypeCount {
    entity_type: String,
    count: u64,
}
