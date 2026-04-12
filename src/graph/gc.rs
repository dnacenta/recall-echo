//! Garbage collection for the knowledge graph.
//!
//! Four-phase sweep: stale relationships → dead relationships → orphaned entities → delete.
//! Dry-run by default. Pipeline-linked entities are protected.

use chrono::{DateTime, Utc};
use surrealdb::Surreal;

use super::confidence;
use super::crud;
use super::error::GraphError;
use super::store::Db;
use super::types::{Entity, Relationship};

/// Configuration for garbage collection thresholds.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Days since valid_from before a low-confidence relationship is considered stale.
    pub stale_days: u64,
    /// Confidence threshold for stale relationships (below this = candidate).
    pub stale_confidence: f64,
    /// Confidence threshold for dead relationships (below this + age check = dead).
    pub dead_confidence: f64,
    /// Minimum age in days for dead relationship pruning.
    pub dead_min_age_days: u64,
    /// If true, only report — don't delete anything.
    pub dry_run: bool,
    /// If true, never GC entities linked to pipeline documents.
    pub protect_pipeline: bool,
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
        }
    }
}

/// A single GC action with reason.
#[derive(Debug, Clone)]
pub struct GcAction {
    pub target_id: String,
    pub target_name: String,
    pub kind: GcActionKind,
    pub reason: String,
}

/// What kind of thing is being collected.
#[derive(Debug, Clone, PartialEq)]
pub enum GcActionKind {
    StaleRelationship,
    DeadRelationship,
    OrphanedEntity,
}

impl std::fmt::Display for GcActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleRelationship => write!(f, "stale_relationship"),
            Self::DeadRelationship => write!(f, "dead_relationship"),
            Self::OrphanedEntity => write!(f, "orphaned_entity"),
        }
    }
}

/// Report from a GC run.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub entities_scanned: u64,
    pub relationships_scanned: u64,
    pub stale_relationships: u64,
    pub dead_relationships: u64,
    pub orphaned_entities: u64,
    pub total_removed: u64,
    pub dry_run: bool,
    pub actions: Vec<GcAction>,
    pub errors: Vec<String>,
}

/// Run garbage collection on the graph.
pub async fn run_gc(db: &Surreal<Db>, config: &GcConfig) -> Result<GcReport, GraphError> {
    let now = Utc::now();
    let mut report = GcReport {
        dry_run: config.dry_run,
        ..Default::default()
    };

    // Load all relationships and entities
    let all_rels = crud::list_all_relationships(db).await?;
    let all_entities = crud::list_entities(db, None).await?;
    report.relationships_scanned = all_rels.len() as u64;
    report.entities_scanned = all_entities.len() as u64;

    // Phase 1: Stale relationship decay
    let stale_ids = phase_stale_relationships(&all_rels, config, &now, &mut report);

    // Phase 2: Dead relationship pruning
    let dead_ids = phase_dead_relationships(&all_rels, config, &now, &stale_ids, &mut report);

    // Collect all relationship IDs to delete
    let mut rel_ids_to_delete: Vec<String> = Vec::new();
    rel_ids_to_delete.extend(stale_ids);
    rel_ids_to_delete.extend(dead_ids);

    // Phase 3: Orphaned entity removal (must account for relationships being removed)
    let orphan_ids =
        phase_orphaned_entities(db, &all_entities, config, &rel_ids_to_delete, &mut report).await?;

    // Phase 4: Execute deletions
    if !config.dry_run {
        for rel_id in &rel_ids_to_delete {
            if let Err(e) = crud::delete_relationship(db, rel_id).await {
                report
                    .errors
                    .push(format!("Failed to delete relationship {rel_id}: {e}"));
            } else {
                report.total_removed += 1;
            }
        }

        for entity_id in &orphan_ids {
            if let Err(e) = crud::delete_entity(db, entity_id).await {
                report
                    .errors
                    .push(format!("Failed to delete entity {entity_id}: {e}"));
            } else {
                report.total_removed += 1;
            }
        }
    } else {
        report.total_removed = rel_ids_to_delete.len() as u64 + orphan_ids.len() as u64;
    }

    Ok(report)
}

/// Phase 1: Find relationships older than stale_days with effective confidence below stale_confidence.
/// Uses temporal decay — effective confidence accounts for time since last reinforcement.
/// Only considers active relationships (valid_until is None).
fn phase_stale_relationships(
    rels: &[Relationship],
    config: &GcConfig,
    now: &DateTime<Utc>,
    report: &mut GcReport,
) -> Vec<String> {
    let mut stale_ids = Vec::new();

    for rel in rels {
        // Skip already-superseded relationships
        if rel.valid_until.is_some() {
            continue;
        }

        // Compute effective confidence with temporal decay
        let effective = confidence::effective_confidence(
            rel.confidence,
            rel.last_reinforced.as_ref(),
            &rel.valid_from,
            now,
        );

        // Check effective confidence threshold
        if effective >= config.stale_confidence {
            continue;
        }

        // Check age
        let age_days = match parse_datetime(&rel.valid_from) {
            Some(dt) => (*now - dt).num_days(),
            None => continue,
        };

        if age_days < config.stale_days as i64 {
            continue;
        }

        let id = rel.id_string();
        let description = rel.description.as_deref().unwrap_or("(no description)");
        report.actions.push(GcAction {
            target_id: id.clone(),
            target_name: format!(
                "{} --[{}]--> {}",
                value_to_short_id(&rel.from_id),
                rel.rel_type,
                value_to_short_id(&rel.to_id)
            ),
            kind: GcActionKind::StaleRelationship,
            reason: format!(
                "effective_confidence {:.2} (stored {:.2}) < {:.2}, age {} days > {}, desc: {}",
                effective,
                rel.confidence,
                config.stale_confidence,
                age_days,
                config.stale_days,
                description
            ),
        });
        stale_ids.push(id);
        report.stale_relationships += 1;
    }

    stale_ids
}

/// Phase 2: Find very low effective confidence relationships older than dead_min_age_days.
/// Uses temporal decay. Excludes relationships already caught in phase 1.
fn phase_dead_relationships(
    rels: &[Relationship],
    config: &GcConfig,
    now: &DateTime<Utc>,
    already_caught: &[String],
    report: &mut GcReport,
) -> Vec<String> {
    let mut dead_ids = Vec::new();

    for rel in rels {
        let id = rel.id_string();

        // Skip if already caught in phase 1
        if already_caught.contains(&id) {
            continue;
        }

        // Compute effective confidence with temporal decay
        let effective = confidence::effective_confidence(
            rel.confidence,
            rel.last_reinforced.as_ref(),
            &rel.valid_from,
            now,
        );

        // Check effective confidence threshold (lower bar than stale)
        if effective >= config.dead_confidence {
            continue;
        }

        // Check minimum age
        let age_days = match parse_datetime(&rel.valid_from) {
            Some(dt) => (*now - dt).num_days(),
            None => continue,
        };

        if age_days < config.dead_min_age_days as i64 {
            continue;
        }

        let description = rel.description.as_deref().unwrap_or("(no description)");
        report.actions.push(GcAction {
            target_id: id.clone(),
            target_name: format!(
                "{} --[{}]--> {}",
                value_to_short_id(&rel.from_id),
                rel.rel_type,
                value_to_short_id(&rel.to_id)
            ),
            kind: GcActionKind::DeadRelationship,
            reason: format!(
                "effective_confidence {:.2} (stored {:.2}) < {:.2}, age {} days > {}, desc: {}",
                effective,
                rel.confidence,
                config.dead_confidence,
                age_days,
                config.dead_min_age_days,
                description
            ),
        });
        dead_ids.push(id);
        report.dead_relationships += 1;
    }

    dead_ids
}

/// Phase 3: Find entities with zero relationships (accounting for pending deletions),
/// zero access_count, and no pipeline linkage.
async fn phase_orphaned_entities(
    db: &Surreal<Db>,
    entities: &[Entity],
    config: &GcConfig,
    pending_rel_deletions: &[String],
    report: &mut GcReport,
) -> Result<Vec<String>, GraphError> {
    let mut orphan_ids = Vec::new();

    for entity in entities {
        // Skip entities that have been accessed
        if entity.access_count > 0 {
            continue;
        }

        // Skip pipeline-linked entities if protection is on
        if config.protect_pipeline && is_pipeline_entity(entity) {
            continue;
        }

        // Count current relationships
        let entity_id = entity.id_string();
        let current_rels = crud::count_relationships(db, &entity_id).await?;

        // Count how many of those relationships are being deleted
        // (we need to check the actual relationship IDs touching this entity)
        let rels_being_deleted =
            count_pending_deletions_for_entity(db, &entity_id, pending_rel_deletions).await?;

        let remaining = current_rels.saturating_sub(rels_being_deleted);

        if remaining > 0 {
            continue;
        }

        report.actions.push(GcAction {
            target_id: entity_id.clone(),
            target_name: format!("{} ({})", entity.name, entity.entity_type),
            kind: GcActionKind::OrphanedEntity,
            reason: format!(
                "zero relationships after pruning, access_count={}",
                entity.access_count
            ),
        });
        orphan_ids.push(entity_id);
        report.orphaned_entities += 1;
    }

    Ok(orphan_ids)
}

/// Count how many of the pending relationship deletions affect a given entity.
async fn count_pending_deletions_for_entity(
    db: &Surreal<Db>,
    entity_id: &str,
    pending_deletions: &[String],
) -> Result<u64, GraphError> {
    if pending_deletions.is_empty() {
        return Ok(0);
    }

    // Get all relationships for this entity and check overlap with pending deletions
    let mut response = db
        .query(
            r#"SELECT id FROM relates_to
               WHERE in = type::record($id) OR out = type::record($id)"#,
        )
        .bind(("id", entity_id.to_string()))
        .await?;

    #[derive(serde::Deserialize)]
    struct IdRow {
        id: serde_json::Value,
    }

    let rows: Vec<IdRow> = super::deserialize_take(&mut response, 0)?;
    let count = rows
        .iter()
        .filter(|r| {
            let id_str = match &r.id {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            pending_deletions.contains(&id_str)
        })
        .count();

    Ok(count as u64)
}

/// Check if an entity is linked to a pipeline document.
fn is_pipeline_entity(entity: &Entity) -> bool {
    // Check source field
    if let Some(ref source) = entity.source {
        if source.starts_with("pipeline:") {
            return true;
        }
    }

    // Check attributes for pipeline_stage
    if let Some(ref attrs) = entity.attributes {
        if attrs.get("pipeline_stage").is_some() {
            return true;
        }
    }

    false
}

use super::util::parse_datetime;

/// Extract a short ID from a record ID value (e.g. "entity:abc" → "abc").
fn value_to_short_id(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.split(':').next_back().unwrap_or(s).to_string(),
        other => other.to_string(),
    }
}

/// Get stats-only report without computing deletion candidates.
/// Uses effective confidence (with temporal decay) for threshold counts.
pub async fn stats_only(db: &Surreal<Db>) -> Result<GcStatsReport, GraphError> {
    let now = Utc::now();
    let all_rels = crud::list_all_relationships(db).await?;
    let all_entities = crud::list_entities(db, None).await?;

    let pipeline_entities = all_entities
        .iter()
        .filter(|e| is_pipeline_entity(e))
        .count();

    let zero_access_entities = all_entities.iter().filter(|e| e.access_count == 0).count();

    let low_confidence_rels = all_rels
        .iter()
        .filter(|r| {
            confidence::effective_confidence(
                r.confidence,
                r.last_reinforced.as_ref(),
                &r.valid_from,
                &now,
            ) < 0.5
        })
        .count();

    let very_low_confidence_rels = all_rels
        .iter()
        .filter(|r| {
            confidence::effective_confidence(
                r.confidence,
                r.last_reinforced.as_ref(),
                &r.valid_from,
                &now,
            ) < 0.2
        })
        .count();

    let superseded_rels = all_rels.iter().filter(|r| r.valid_until.is_some()).count();

    Ok(GcStatsReport {
        total_entities: all_entities.len() as u64,
        total_relationships: all_rels.len() as u64,
        pipeline_entities: pipeline_entities as u64,
        zero_access_entities: zero_access_entities as u64,
        low_confidence_rels: low_confidence_rels as u64,
        very_low_confidence_rels: very_low_confidence_rels as u64,
        superseded_rels: superseded_rels as u64,
    })
}

/// Health stats without running GC.
#[derive(Debug, Clone)]
pub struct GcStatsReport {
    pub total_entities: u64,
    pub total_relationships: u64,
    pub pipeline_entities: u64,
    pub zero_access_entities: u64,
    /// Count of relationships with effective (decayed) confidence < 0.5
    pub low_confidence_rels: u64,
    /// Count of relationships with effective (decayed) confidence < 0.2
    pub very_low_confidence_rels: u64,
    pub superseded_rels: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_config_defaults() {
        let config = GcConfig::default();
        assert_eq!(config.stale_days, 30);
        assert_eq!(config.stale_confidence, 0.5);
        assert_eq!(config.dead_confidence, 0.2);
        assert_eq!(config.dead_min_age_days, 14);
        assert!(config.dry_run);
        assert!(config.protect_pipeline);
    }

    #[test]
    fn test_parse_datetime_iso() {
        let val = serde_json::Value::String("2024-01-15T10:30:00Z".to_string());
        let dt = parse_datetime(&val);
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_datetime_invalid() {
        let val = serde_json::Value::String("not-a-date".to_string());
        let dt = parse_datetime(&val);
        assert!(dt.is_none());
    }

    #[test]
    fn test_parse_datetime_non_string() {
        let val = serde_json::Value::Number(serde_json::Number::from(12345));
        let dt = parse_datetime(&val);
        assert!(dt.is_none());
    }

    #[test]
    fn test_value_to_short_id() {
        let val = serde_json::Value::String("entity:abc123".to_string());
        assert_eq!(value_to_short_id(&val), "abc123");
    }

    #[test]
    fn test_value_to_short_id_no_colon() {
        let val = serde_json::Value::String("abc123".to_string());
        assert_eq!(value_to_short_id(&val), "abc123");
    }

    #[test]
    fn test_is_pipeline_entity_by_source() {
        let entity = Entity {
            id: serde_json::Value::String("entity:test".to_string()),
            name: "Test".to_string(),
            entity_type: super::super::types::EntityType::Thread,
            abstract_text: "test".to_string(),
            overview: "test".to_string(),
            content: None,
            attributes: None,
            embedding: None,
            mutable: true,
            access_count: 0,
            utility_score: 0.5,
            utility_updates: 0,
            created_at: serde_json::Value::String("2024-01-01T00:00:00Z".to_string()),
            updated_at: serde_json::Value::String("2024-01-01T00:00:00Z".to_string()),
            source: Some("pipeline:learning".to_string()),
        };
        assert!(is_pipeline_entity(&entity));
    }

    #[test]
    fn test_is_pipeline_entity_by_attributes() {
        let entity = Entity {
            id: serde_json::Value::String("entity:test".to_string()),
            name: "Test".to_string(),
            entity_type: super::super::types::EntityType::Concept,
            abstract_text: "test".to_string(),
            overview: "test".to_string(),
            content: None,
            attributes: Some(serde_json::json!({"pipeline_stage": "thoughts"})),
            embedding: None,
            mutable: true,
            access_count: 0,
            utility_score: 0.5,
            utility_updates: 0,
            created_at: serde_json::Value::String("2024-01-01T00:00:00Z".to_string()),
            updated_at: serde_json::Value::String("2024-01-01T00:00:00Z".to_string()),
            source: None,
        };
        assert!(is_pipeline_entity(&entity));
    }

    #[test]
    fn test_is_not_pipeline_entity() {
        let entity = Entity {
            id: serde_json::Value::String("entity:test".to_string()),
            name: "Test".to_string(),
            entity_type: super::super::types::EntityType::Tool,
            abstract_text: "test".to_string(),
            overview: "test".to_string(),
            content: None,
            attributes: None,
            embedding: None,
            mutable: true,
            access_count: 0,
            utility_score: 0.5,
            utility_updates: 0,
            created_at: serde_json::Value::String("2024-01-01T00:00:00Z".to_string()),
            updated_at: serde_json::Value::String("2024-01-01T00:00:00Z".to_string()),
            source: Some("llm:ingest".to_string()),
        };
        assert!(!is_pipeline_entity(&entity));
    }

    #[test]
    fn test_phase_stale_relationships() {
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(45)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:abc".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: Some("test rel".to_string()),
            valid_from: serde_json::Value::String(old_date),
            valid_until: None,
            confidence: 0.3,
            last_reinforced: None,
            source: Some("ingest".to_string()),
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let stale = phase_stale_relationships(&rels, &config, &now, &mut report);

        assert_eq!(stale.len(), 1);
        assert_eq!(report.stale_relationships, 1);
    }

    #[test]
    fn test_phase_stale_skips_high_confidence() {
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(45)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:abc".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: None,
            valid_from: serde_json::Value::String(old_date),
            valid_until: None,
            confidence: 0.8,
            last_reinforced: None,
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let stale = phase_stale_relationships(&rels, &config, &now, &mut report);

        assert!(stale.is_empty());
    }

    #[test]
    fn test_phase_stale_skips_young() {
        let now = Utc::now();
        let recent_date = (now - chrono::Duration::days(5)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:abc".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: None,
            valid_from: serde_json::Value::String(recent_date),
            valid_until: None,
            confidence: 0.3,
            last_reinforced: None,
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let stale = phase_stale_relationships(&rels, &config, &now, &mut report);

        assert!(stale.is_empty());
    }

    #[test]
    fn test_phase_stale_skips_superseded() {
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(45)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:abc".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: None,
            valid_from: serde_json::Value::String(old_date.clone()),
            valid_until: Some(serde_json::Value::String(old_date)),
            confidence: 0.3,
            last_reinforced: None,
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let stale = phase_stale_relationships(&rels, &config, &now, &mut report);

        assert!(stale.is_empty());
    }

    #[test]
    fn test_phase_dead_relationships() {
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(20)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:dead1".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: None,
            valid_from: serde_json::Value::String(old_date),
            valid_until: None,
            confidence: 0.1,
            last_reinforced: None,
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let already_caught = vec![];
        let dead = phase_dead_relationships(&rels, &config, &now, &already_caught, &mut report);

        assert_eq!(dead.len(), 1);
        assert_eq!(report.dead_relationships, 1);
    }

    #[test]
    fn test_phase_dead_skips_already_caught() {
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(20)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:dead1".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: None,
            valid_from: serde_json::Value::String(old_date),
            valid_until: None,
            confidence: 0.1,
            last_reinforced: None,
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let already_caught = vec!["relates_to:dead1".to_string()];
        let dead = phase_dead_relationships(&rels, &config, &now, &already_caught, &mut report);

        assert!(dead.is_empty());
    }

    #[test]
    fn test_gc_action_kind_display() {
        assert_eq!(
            GcActionKind::StaleRelationship.to_string(),
            "stale_relationship"
        );
        assert_eq!(
            GcActionKind::DeadRelationship.to_string(),
            "dead_relationship"
        );
        assert_eq!(GcActionKind::OrphanedEntity.to_string(), "orphaned_entity");
    }

    #[test]
    fn test_phase_stale_decay_makes_high_stored_confidence_stale() {
        // A relationship with stored confidence 0.6 (above stale threshold 0.5)
        // but last reinforced 180 days ago — decay brings effective to ~0.15
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(180)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:decayed".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: Some("decayed rel".to_string()),
            valid_from: serde_json::Value::String(old_date),
            valid_until: None,
            confidence: 0.6,       // Above stale threshold!
            last_reinforced: None, // Never reinforced, so decays from valid_from
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let stale = phase_stale_relationships(&rels, &config, &now, &mut report);

        // Without decay: 0.6 >= 0.5, would NOT be caught
        // With decay: 0.6 * 0.5^(180/90) = 0.6 * 0.25 = 0.15 < 0.5, IS caught
        assert_eq!(
            stale.len(),
            1,
            "decayed relationship should be caught as stale"
        );
    }

    #[test]
    fn test_phase_stale_reinforced_prevents_decay() {
        // Same stored confidence 0.6, old valid_from, but recently reinforced
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(180)).to_rfc3339();
        let recent_reinforce = (now - chrono::Duration::days(5)).to_rfc3339();

        let rels = vec![Relationship {
            id: serde_json::Value::String("relates_to:reinforced".to_string()),
            from_id: serde_json::Value::String("entity:a".to_string()),
            to_id: serde_json::Value::String("entity:b".to_string()),
            rel_type: "CONNECTED_TO".to_string(),
            description: None,
            valid_from: serde_json::Value::String(old_date),
            valid_until: None,
            confidence: 0.6,
            last_reinforced: Some(serde_json::Value::String(recent_reinforce)),
            source: None,
        }];

        let config = GcConfig::default();
        let mut report = GcReport::default();
        let stale = phase_stale_relationships(&rels, &config, &now, &mut report);

        // Reinforced 5 days ago: effective ≈ 0.6 * 0.5^(5/90) ≈ 0.577 > 0.5
        assert!(
            stale.is_empty(),
            "recently reinforced relationship should NOT be stale"
        );
    }
}
