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
pub mod vigil_sync;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use embed::FastEmbedder;
use error::GraphError;
use store::Db;
#[allow(unused_imports)]
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
    embedder: FastEmbedder,
    path: PathBuf,
}

impl GraphMemory {
    /// Open or create a graph store at the given path.
    /// Path should be the `graph/` directory inside the memory directory.
    pub async fn open(path: &Path) -> Result<Self, GraphError> {
        std::fs::create_dir_all(path)?;

        let db = store::open(path).await?;
        store::init_schema(&db).await?;

        let models_dir = path.join("models");
        std::fs::create_dir_all(&models_dir)?;
        let embedder = FastEmbedder::new(&models_dir)?;

        Ok(Self {
            db,
            embedder,
            path: path.to_path_buf(),
        })
    }

    /// Path to the graph store.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Internal access to the database handle.
    #[allow(dead_code)]
    pub(crate) fn db(&self) -> &Surreal<Db> {
        &self.db
    }

    /// Internal access to the embedder.
    #[allow(dead_code)]
    pub(crate) fn embedder(&self) -> &FastEmbedder {
        &self.embedder
    }

    // --- Entity CRUD ---

    /// Add a new entity to the graph.
    pub async fn add_entity(&self, entity: NewEntity) -> Result<Entity, GraphError> {
        crud::add_entity(&self.db, &self.embedder, entity).await
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
        crud::update_entity(&self.db, &self.embedder, id, updates).await
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

    /// Update relationship confidence (Bayesian posterior).
    pub async fn update_relationship_confidence(
        &self,
        rel_id: &str,
        confidence: f64,
    ) -> Result<(), GraphError> {
        crud::update_relationship_confidence(&self.db, rel_id, confidence).await
    }

    // --- Episodes ---

    /// Add a new episode to the graph.
    pub async fn add_episode(&self, episode: NewEpisode) -> Result<Episode, GraphError> {
        crud::add_episode(&self.db, &self.embedder, episode).await
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
    pub async fn ingest_archive(
        &self,
        archive_text: &str,
        session_id: &str,
        log_number: Option<u32>,
        llm: Option<&dyn llm::LlmProvider>,
    ) -> Result<IngestionReport, GraphError> {
        ingest::ingest_archive(self, archive_text, session_id, log_number, llm).await
    }

    /// Run LLM extraction on an archive without creating episodes.
    pub async fn extract_from_archive(
        &self,
        archive_text: &str,
        session_id: &str,
        log_number: Option<u32>,
        llm: &dyn llm::LlmProvider,
    ) -> Result<IngestionReport, GraphError> {
        ingest::extract_from_archive(self, archive_text, session_id, log_number, llm).await
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
        search::search(&self.db, &self.embedder, query, limit).await
    }

    /// Search with options — L1 projections, type/keyword filters.
    pub async fn search_with_options(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<Vec<ScoredEntity>, GraphError> {
        search::search_with_options(&self.db, &self.embedder, query, options).await
    }

    /// Semantic search across episodes.
    pub async fn search_episodes(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EpisodeSearchResult>, GraphError> {
        search::search_episodes(&self.db, &self.embedder, query, limit).await
    }

    // --- Hybrid Query ---

    /// Hybrid query: semantic + graph expansion + optional episode search.
    pub async fn query(
        &self,
        query_text: &str,
        options: &QueryOptions,
    ) -> Result<QueryResult, GraphError> {
        query::query(&self.db, &self.embedder, query_text, options).await
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

async fn db_count(db: &Surreal<Db>, table: &str) -> Result<u64, GraphError> {
    let query = format!("SELECT count() AS count FROM {} GROUP ALL", table);
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
