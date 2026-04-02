//! Graph garbage collection — prunes stale relationships and orphaned entities.
//!
//! Four-phase sweep:
//! 1. Stale relationship decay — old + low confidence
//! 2. Dead relationship pruning — very low confidence, never reinforced
//! 3. Orphaned entity removal — no relationships, no access, no pipeline link
//! 4. Delete — execute removals (dry-run by default)

use std::fmt;

use surrealdb::Surreal;

use super::confidence::{effective_confidence, DecayConfig};
use super::error::GraphError;
use super::store::Db;

/// Configuration for the garbage collector.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Days since `valid_from` before a low-confidence relationship is considered stale.
    pub stale_days: u64,
    /// Confidence threshold for stale relationship decay (below this = candidate).
    /// Applied to *effective* (decayed) confidence, not stored.
    pub stale_confidence: f64,
    /// Confidence threshold for dead relationship pruning.
    /// Applied to *effective* (decayed) confidence, not stored.
    pub dead_confidence: f64,
    /// Minimum age in days for dead relationship pruning.
    pub dead_min_age_days: u64,
    /// If true, report what would be deleted without actually deleting.
    pub dry_run: bool,
    /// If true, never GC entities linked to pipeline documents.
    pub protect_pipeline: bool,
    /// Half-life in days for temporal decay calculation.
    /// Used to compute effective confidence before comparing against thresholds.
    pub half_life_days: f64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            stale_days: 30,
            stale_confidence: 0.5,
            dead_confidence: 0.2,
            dead_min_age_days: 14,
            dry_run: true,
            protect_pipeline: true,
            half_life_days: 90.0,
        }
    }
}

/// Statistics from a GC run.
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    /// Total entities scanned.
    pub entities_scanned: u64,
    /// Total relationships scanned.
    pub relationships_scanned: u64,
    /// Relationships removed (or would be removed) by stale decay.
    pub stale_relationships: u64,
    /// Relationships removed (or would be removed) by dead pruning.
    pub dead_relationships: u64,
    /// Entities removed (or would be removed) as orphans.
    pub orphaned_entities: u64,
    /// Entities protected by pipeline linkage.
    pub pipeline_protected: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Details of each removal candidate (for reporting).
    pub details: Vec<GcDetail>,
}

impl GcStats {
    pub fn total_removed(&self) -> u64 {
        self.stale_relationships + self.dead_relationships + self.orphaned_entities
    }
}

impl fmt::Display for GcStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = if self.dry_run {
            "DRY RUN"
        } else {
            "EXECUTED"
        };
        writeln!(f, "GC Report ({mode})")?;
        writeln!(
            f,
            "  Scanned: {} entities, {} relationships",
            self.entities_scanned, self.relationships_scanned
        )?;
        writeln!(
            f,
            "  Stale relationships:  {}",
            self.stale_relationships
        )?;
        writeln!(f, "  Dead relationships:   {}", self.dead_relationships)?;
        writeln!(f, "  Orphaned entities:    {}", self.orphaned_entities)?;
        if self.pipeline_protected > 0 {
            writeln!(
                f,
                "  Pipeline-protected:   {}",
                self.pipeline_protected
            )?;
        }
        writeln!(
            f,
            "  Total removals:       {}",
            self.total_removed()
        )?;
        Ok(())
    }
}

/// Detail about a single GC removal candidate.
#[derive(Debug, Clone)]
pub struct GcDetail {
    pub id: String,
    pub name: Option<String>,
    pub kind: GcRemovalKind,
    pub reason: String,
}

/// What kind of removal this is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GcRemovalKind {
    StaleRelationship,
    DeadRelationship,
    OrphanedEntity,
}

impl fmt::Display for GcRemovalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleRelationship => write!(f, "stale_rel"),
            Self::DeadRelationship => write!(f, "dead_rel"),
            Self::OrphanedEntity => write!(f, "orphan"),
        }
    }
}

/// Row types for GC queries.
#[derive(Debug, serde::Deserialize)]
struct RelCandidate {
    id: serde_json::Value,
    confidence: f64,
    #[serde(default)]
    last_reinforced: Option<serde_json::Value>,
    #[serde(default)]
    valid_from: Option<serde_json::Value>,
    #[serde(default)]
    rel_type: Option<String>,
    #[serde(default)]
    from_name: Option<String>,
    #[serde(default)]
    to_name: Option<String>,
}

impl RelCandidate {
    fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// Compute effective (decayed) confidence using temporal decay.
    fn effective_confidence(&self, decay_config: &DecayConfig) -> f64 {
        let fallback = serde_json::Value::Null;
        let vf = self.valid_from.as_ref().unwrap_or(&fallback);
        effective_confidence(
            self.confidence,
            self.last_reinforced.as_ref(),
            vf,
            decay_config,
        )
    }

    fn description_with_decay(&self, eff: f64) -> String {
        let from = self.from_name.as_deref().unwrap_or("?");
        let to = self.to_name.as_deref().unwrap_or("?");
        let rt = self.rel_type.as_deref().unwrap_or("?");
        if (eff - self.confidence).abs() > 0.001 {
            format!(
                "{from} —[{rt}]→ {to} (stored: {:.3}, effective: {:.3})",
                self.confidence, eff
            )
        } else {
            format!("{from} —[{rt}]→ {to} (conf: {:.3})", self.confidence)
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct OrphanCandidate {
    id: serde_json::Value,
    name: String,
    entity_type: String,
    #[serde(default)]
    access_count: i64,
    #[serde(default)]
    attributes: Option<serde_json::Value>,
}

impl OrphanCandidate {
    fn id_string(&self) -> String {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    fn is_pipeline_linked(&self) -> bool {
        if let Some(attrs) = &self.attributes {
            attrs.get("pipeline_stage").is_some()
        } else {
            false
        }
    }
}

/// Run garbage collection on the knowledge graph.
pub async fn run_gc(db: &Surreal<Db>, config: &GcConfig) -> Result<GcStats, GraphError> {
    let mut stats = GcStats {
        dry_run: config.dry_run,
        ..Default::default()
    };

    // Count totals for reporting
    stats.entities_scanned = count_table(db, "entity").await?;
    stats.relationships_scanned = count_table(db, "relates_to").await?;

    let decay_config = DecayConfig {
        half_life_days: config.half_life_days,
        ..DecayConfig::default()
    };

    // Phase 1: Stale relationship decay (uses effective confidence)
    let stale_rels = find_stale_relationships(db, config).await?;
    for rel in &stale_rels {
        let eff = rel.effective_confidence(&decay_config);
        stats.details.push(GcDetail {
            id: rel.id_string(),
            name: None,
            kind: GcRemovalKind::StaleRelationship,
            reason: format!(
                "stale: {} (>{} days, eff_conf < {})",
                rel.description_with_decay(eff),
                config.stale_days,
                config.stale_confidence
            ),
        });
    }
    stats.stale_relationships = stale_rels.len() as u64;

    // Phase 2: Dead relationship pruning (uses effective confidence)
    let dead_rels = find_dead_relationships(db, config).await?;
    for rel in &dead_rels {
        // Don't double-count if already caught by stale phase
        let already_stale = stale_rels.iter().any(|s| s.id_string() == rel.id_string());
        if !already_stale {
            let eff = rel.effective_confidence(&decay_config);
            stats.details.push(GcDetail {
                id: rel.id_string(),
                name: None,
                kind: GcRemovalKind::DeadRelationship,
                reason: format!(
                    "dead: {} (>{} days, eff_conf < {})",
                    rel.description_with_decay(eff),
                    config.dead_min_age_days,
                    config.dead_confidence
                ),
            });
            stats.dead_relationships += 1;
        }
    }

    // Collect all relationship IDs to delete (deduplicated)
    let mut rel_ids_to_delete: Vec<String> = stale_rels.iter().map(|r| r.id_string()).collect();
    for rel in &dead_rels {
        let id = rel.id_string();
        if !rel_ids_to_delete.contains(&id) {
            rel_ids_to_delete.push(id);
        }
    }

    // Phase 3: Orphaned entity removal (after relationship pruning)
    // We need to find entities that WOULD be orphaned after deleting the candidate relationships
    let orphans = find_orphaned_entities(db, config, &rel_ids_to_delete).await?;
    for orphan in &orphans {
        if config.protect_pipeline && orphan.is_pipeline_linked() {
            stats.pipeline_protected += 1;
        } else {
            stats.details.push(GcDetail {
                id: orphan.id_string(),
                name: Some(orphan.name.clone()),
                kind: GcRemovalKind::OrphanedEntity,
                reason: format!(
                    "orphan: {} ({}, access_count={})",
                    orphan.name, orphan.entity_type, orphan.access_count
                ),
            });
            stats.orphaned_entities += 1;
        }
    }

    // Phase 4: Execute deletions (unless dry run)
    if !config.dry_run {
        // Delete relationships first
        for rel_id in &rel_ids_to_delete {
            delete_relationship(db, rel_id).await?;
        }

        // Delete orphaned entities
        for orphan in &orphans {
            if config.protect_pipeline && orphan.is_pipeline_linked() {
                continue;
            }
            delete_entity_and_rels(db, &orphan.id_string()).await?;
        }
    }

    Ok(stats)
}

/// Phase 1: Find relationships older than `stale_days` with *effective* confidence below `stale_confidence`.
/// Uses temporal decay to compute effective confidence from stored + last_reinforced.
async fn find_stale_relationships(
    db: &Surreal<Db>,
    config: &GcConfig,
) -> Result<Vec<RelCandidate>, GraphError> {
    // Fetch all non-superseded relationships older than stale_days.
    // We filter by effective confidence in Rust after applying decay.
    let query = r#"
        SELECT
            id,
            confidence,
            last_reinforced,
            valid_from,
            rel_type,
            in.name AS from_name,
            out.name AS to_name
        FROM relates_to
        WHERE valid_from < time::now() - type::duration($age)
            AND (valid_until IS NONE OR valid_until IS NULL)
    "#;

    let age_str = format!("{}d", config.stale_days);
    let decay_config = DecayConfig {
        half_life_days: config.half_life_days,
        ..DecayConfig::default()
    };

    let mut response = db
        .query(query)
        .bind(("age", age_str))
        .await?;

    let all_rows: Vec<RelCandidate> = super::deserialize_take(&mut response, 0)?;

    // Filter by effective confidence < threshold
    let rows = all_rows
        .into_iter()
        .filter(|r| r.effective_confidence(&decay_config) < config.stale_confidence)
        .collect();

    Ok(rows)
}

/// Phase 2: Find relationships with very low *effective* confidence, older than `dead_min_age_days`.
/// These are low-quality extractions that were never reinforced by further evidence.
async fn find_dead_relationships(
    db: &Surreal<Db>,
    config: &GcConfig,
) -> Result<Vec<RelCandidate>, GraphError> {
    let query = r#"
        SELECT
            id,
            confidence,
            last_reinforced,
            valid_from,
            rel_type,
            in.name AS from_name,
            out.name AS to_name
        FROM relates_to
        WHERE valid_from < time::now() - type::duration($age)
    "#;

    let age_str = format!("{}d", config.dead_min_age_days);
    let decay_config = DecayConfig {
        half_life_days: config.half_life_days,
        ..DecayConfig::default()
    };

    let mut response = db
        .query(query)
        .bind(("age", age_str))
        .await?;

    let all_rows: Vec<RelCandidate> = super::deserialize_take(&mut response, 0)?;

    // Filter by effective confidence < threshold
    let rows = all_rows
        .into_iter()
        .filter(|r| r.effective_confidence(&decay_config) < config.dead_confidence)
        .collect();

    Ok(rows)
}

/// Phase 3: Find entities with zero relationships (accounting for pending deletions),
/// zero access count, and not linked to pipeline documents.
async fn find_orphaned_entities(
    db: &Surreal<Db>,
    _config: &GcConfig,
    pending_rel_deletions: &[String],
) -> Result<Vec<OrphanCandidate>, GraphError> {
    // First, find entities that currently have zero relationships
    let query = r#"
        SELECT id, name, entity_type, access_count, attributes
        FROM entity
        WHERE access_count <= 0
            AND (
                SELECT count() FROM relates_to
                WHERE in = $parent.id OR out = $parent.id
            )[0].count <= 0
    "#;

    let mut zero_rel_entities: Vec<OrphanCandidate> =
        match db.query(query).await {
            Ok(mut response) => super::deserialize_take(&mut response, 0).unwrap_or_default(),
            Err(_) => {
                // Fallback: simpler approach if subquery doesn't work
                find_orphans_fallback(db, pending_rel_deletions).await?
            }
        };

    // Also check entities that WOULD become orphaned after pending deletions
    if !pending_rel_deletions.is_empty() {
        let would_be_orphans = find_would_be_orphans(db, pending_rel_deletions).await?;
        for candidate in would_be_orphans {
            if !zero_rel_entities.iter().any(|e| e.id_string() == candidate.id_string()) {
                zero_rel_entities.push(candidate);
            }
        }
    }

    Ok(zero_rel_entities)
}

/// Find entities that would become orphaned after deleting the specified relationships.
async fn find_would_be_orphans(
    db: &Surreal<Db>,
    pending_rel_deletions: &[String],
) -> Result<Vec<OrphanCandidate>, GraphError> {
    let mut candidates = Vec::new();

    // Collect all entity IDs involved in pending deletions
    for rel_id in pending_rel_deletions {
        let query = r#"
            SELECT in AS from_id, out AS to_id FROM type::record($id)
        "#;

        #[derive(Debug, serde::Deserialize)]
        struct RelEndpoints {
            from_id: serde_json::Value,
            to_id: serde_json::Value,
        }

        let mut response = db
            .query(query)
            .bind(("id", rel_id.clone()))
            .await?;

        let endpoints: Vec<RelEndpoints> = super::deserialize_take(&mut response, 0)?;
        for ep in endpoints {
            let from_str = match &ep.from_id {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let to_str = match &ep.to_id {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };

            for entity_id in [from_str, to_str] {
                // Check if this entity would have zero remaining relationships
                // after removing all pending deletions
                let count_query = r#"
                    SELECT count() AS count FROM relates_to
                    WHERE (in = type::record($eid) OR out = type::record($eid))
                        AND id NOT IN $pending
                    GROUP ALL
                "#;

                #[derive(Debug, serde::Deserialize)]
                struct CountRow {
                    count: u64,
                }

                let pending_json: Vec<serde_json::Value> = pending_rel_deletions
                    .iter()
                    .map(|id| serde_json::Value::String(id.clone()))
                    .collect();

                let mut count_resp = db
                    .query(count_query)
                    .bind(("eid", entity_id.clone()))
                    .bind(("pending", pending_json))
                    .await?;

                let rows: Vec<CountRow> =
                    super::deserialize_take(&mut count_resp, 0).unwrap_or_default();
                let remaining = rows.first().map(|r| r.count).unwrap_or(0);

                if remaining == 0 {
                    // This entity would be orphaned — check if it qualifies
                    let entity_query = r#"
                        SELECT id, name, entity_type, access_count, attributes
                        FROM type::record($id)
                        WHERE access_count <= 0
                    "#;

                    let mut entity_resp = db
                        .query(entity_query)
                        .bind(("id", entity_id))
                        .await?;

                    let entity_candidates: Vec<OrphanCandidate> =
                        super::deserialize_take(&mut entity_resp, 0).unwrap_or_default();

                    for c in entity_candidates {
                        if !candidates.iter().any(|e: &OrphanCandidate| e.id_string() == c.id_string()) {
                            candidates.push(c);
                        }
                    }
                }
            }
        }
    }

    Ok(candidates)
}

/// Fallback orphan detection when subquery approach fails.
async fn find_orphans_fallback(
    db: &Surreal<Db>,
    _pending_rel_deletions: &[String],
) -> Result<Vec<OrphanCandidate>, GraphError> {
    // Get all entities with zero access
    let query = r#"
        SELECT id, name, entity_type, access_count, attributes
        FROM entity
        WHERE access_count <= 0
    "#;

    let mut response = db.query(query).await?;
    let all_zero_access: Vec<OrphanCandidate> = super::deserialize_take(&mut response, 0)?;

    let mut orphans = Vec::new();
    for entity in all_zero_access {
        let eid = entity.id_string();
        let count_query = r#"
            SELECT count() AS count FROM relates_to
            WHERE in = type::record($id) OR out = type::record($id)
            GROUP ALL
        "#;

        #[derive(Debug, serde::Deserialize)]
        struct CountRow {
            count: u64,
        }

        let mut count_resp = db
            .query(count_query)
            .bind(("id", eid))
            .await?;

        let rows: Vec<CountRow> = super::deserialize_take(&mut count_resp, 0).unwrap_or_default();
        let rel_count = rows.first().map(|r| r.count).unwrap_or(0);

        if rel_count == 0 {
            orphans.push(entity);
        }
    }

    Ok(orphans)
}

/// Delete a single relationship by ID.
async fn delete_relationship(db: &Surreal<Db>, id: &str) -> Result<(), GraphError> {
    db.query("DELETE FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?
        .check()?;
    Ok(())
}

/// Delete an entity and all its relationships.
async fn delete_entity_and_rels(db: &Surreal<Db>, id: &str) -> Result<(), GraphError> {
    db.query(
        r#"
        DELETE FROM relates_to WHERE in = type::record($id) OR out = type::record($id);
        DELETE FROM type::record($id);
        "#,
    )
    .bind(("id", id.to_string()))
    .await?
    .check()?;
    Ok(())
}

/// Count rows in a table.
async fn count_table(db: &Surreal<Db>, table: &str) -> Result<u64, GraphError> {
    #[derive(Debug, serde::Deserialize)]
    struct CountRow {
        count: u64,
    }

    let query = format!("SELECT count() AS count FROM {} GROUP ALL", table);
    let mut response = db.query(&query).await?;
    let rows: Vec<CountRow> = super::deserialize_take(&mut response, 0).unwrap_or_default();
    Ok(rows.first().map(|r| r.count).unwrap_or(0))
}
