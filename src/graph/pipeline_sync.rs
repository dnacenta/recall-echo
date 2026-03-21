//! Pipeline sync engine — reconcile flat-file pipeline state with the graph.
//!
//! Strategy: flat files are the source of truth. On each sync:
//! 1. Parse all pipeline documents into PipelineEntry instances
//! 2. Query existing pipeline entities from the graph
//! 3. Diff: new entries, updated entries, removed entries
//! 4. Apply: create/update/archive entities, create relationships
//!
//! Fully idempotent — running twice on same state produces no changes.

use super::error::GraphError;
use super::pipeline;
use super::types::*;
use super::GraphMemory;

/// Sync pipeline documents into the graph.
pub async fn sync_pipeline(
    gm: &GraphMemory,
    docs: &PipelineDocuments,
) -> Result<PipelineSyncReport, GraphError> {
    let mut report = PipelineSyncReport::default();

    // 1. Parse all documents
    let (entries, relationships) = pipeline::parse_all_documents(docs);

    if entries.is_empty() {
        return Ok(report);
    }

    // 2. Get existing pipeline entities from graph
    let existing = get_pipeline_entities(gm).await?;

    // 3. Diff — match by (entity_type, normalized_title)
    let mut to_create: Vec<&PipelineEntry> = Vec::new();
    let mut to_update: Vec<(&PipelineEntry, Entity)> = Vec::new();
    let mut matched_ids: Vec<String> = Vec::new();

    for entry in &entries {
        let key = normalize_key(&entry.title);
        if let Some(existing_entity) = find_existing(&key, &entry.entity_type, &existing) {
            matched_ids.push(existing_entity.id_string());
            // Check if content actually changed
            if entity_needs_update(entry, &existing_entity) {
                to_update.push((entry, existing_entity));
            }
        } else {
            to_create.push(entry);
        }
    }

    // Entities in graph but not in parsed docs → archive
    let to_archive: Vec<&Entity> = existing
        .iter()
        .filter(|e| !matched_ids.contains(&e.id_string()))
        .collect();

    // 4. Apply changes

    // Create new entities
    for entry in &to_create {
        let extracted = pipeline::entry_to_entity(entry);
        let new_entity = NewEntity {
            name: extracted.name,
            entity_type: extracted.entity_type,
            abstract_text: extracted.abstract_text,
            overview: extracted.overview,
            content: extracted.content,
            attributes: extracted.attributes,
            source: Some(format!("pipeline:{}", entry.stage)),
        };

        match gm.add_entity(new_entity).await {
            Ok(_) => report.entities_created += 1,
            Err(e) => report.errors.push(format!("create {}: {}", entry.title, e)),
        }
    }

    // Update changed entities
    for (entry, existing_entity) in &to_update {
        let extracted = pipeline::entry_to_entity(entry);
        let updates = EntityUpdate {
            abstract_text: Some(extracted.abstract_text),
            overview: extracted.overview,
            content: None,
            attributes: extracted.attributes,
        };

        match gm
            .update_entity(&existing_entity.id_string(), updates)
            .await
        {
            Ok(_) => report.entities_updated += 1,
            Err(e) => report.errors.push(format!("update {}: {}", entry.title, e)),
        }
    }

    // Archive removed entities (set pipeline_status to "archived")
    for entity in &to_archive {
        let mut attrs = entity
            .attributes
            .clone()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        attrs.insert(
            "pipeline_status".into(),
            serde_json::Value::String("archived".into()),
        );

        let updates = EntityUpdate {
            attributes: Some(serde_json::Value::Object(attrs)),
            ..Default::default()
        };

        match gm.update_entity(&entity.id_string(), updates).await {
            Ok(_) => report.entities_archived += 1,
            Err(e) => report
                .errors
                .push(format!("archive {}: {}", entity.name, e)),
        }
    }

    // 5. Create relationships
    for rel in &relationships {
        // Check if both entities exist in the graph
        let source = gm.get_entity(&rel.source).await?;
        let target = gm.get_entity(&rel.target).await?;

        if source.is_some() && target.is_some() {
            // Check for existing relationship to avoid duplicates
            let existing_rels = gm
                .get_relationships(&rel.source, Direction::Both)
                .await
                .unwrap_or_default();

            let already_exists = existing_rels
                .iter()
                .any(|r| r.rel_type == rel.rel_type && r.valid_until.is_none());

            if already_exists {
                report.relationships_skipped += 1;
                continue;
            }

            let new_rel = NewRelationship {
                from_entity: rel.source.clone(),
                to_entity: rel.target.clone(),
                rel_type: rel.rel_type.clone(),
                description: rel.description.clone(),
                confidence: Some(1.0),
                source: Some("pipeline:sync".into()),
            };

            match gm.add_relationship(new_rel).await {
                Ok(_) => report.relationships_created += 1,
                Err(e) => report
                    .errors
                    .push(format!("rel {} -> {}: {}", rel.source, rel.target, e)),
            }
        }
    }

    Ok(report)
}

/// Get all entities that have a pipeline_stage attribute (i.e., pipeline entities).
async fn get_pipeline_entities(gm: &GraphMemory) -> Result<Vec<Entity>, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT * FROM entity WHERE attributes.pipeline_stage IS NOT NONE")
        .await?;

    let entities: Vec<Entity> = super::deserialize_take(&mut response, 0)?;
    Ok(entities)
}

/// Normalize a title for matching: lowercase, strip punctuation.
fn normalize_key(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Find an existing entity that matches a pipeline entry by normalized title and type.
fn find_existing(
    normalized_key: &str,
    entity_type: &EntityType,
    existing: &[Entity],
) -> Option<Entity> {
    let type_str = entity_type.to_string();
    existing
        .iter()
        .find(|e| {
            e.entity_type.to_string() == type_str && normalize_key(&e.name) == *normalized_key
        })
        .cloned()
}

/// Check if a pipeline entry has changed compared to the existing graph entity.
fn entity_needs_update(entry: &PipelineEntry, existing: &Entity) -> bool {
    // Compare pipeline_status
    if let Some(ref attrs) = existing.attributes {
        if let Some(status) = attrs.get("pipeline_status").and_then(|v| v.as_str()) {
            if status != entry.status {
                return true;
            }
        }
    }

    // Compare body content length (rough change detection)
    let new_entity = pipeline::entry_to_entity(entry);
    if let Some(ref new_overview) = new_entity.overview {
        if existing.overview != *new_overview {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_key_works() {
        assert_eq!(
            normalize_key("The External Observer Problem"),
            "the external observer problem"
        );
        assert_eq!(
            normalize_key("The philosophy→behavior gap"),
            "the philosophybehavior gap"
        );
    }

    #[test]
    fn entity_needs_update_detects_status_change() {
        let entry = PipelineEntry {
            title: "Test".into(),
            body: "body".into(),
            status: "graduated".into(),
            stage: "thoughts".into(),
            entity_type: EntityType::Thought,
            date: None,
            source_ref: None,
            destination: None,
            connected_to: vec![],
            sub_type: None,
        };

        let mut attrs = serde_json::Map::new();
        attrs.insert(
            "pipeline_status".into(),
            serde_json::Value::String("active".into()),
        );

        let existing = Entity {
            id: serde_json::Value::String("entity:test".into()),
            name: "Test".into(),
            entity_type: EntityType::Thought,
            abstract_text: "abstract".into(),
            overview: "body".into(),
            content: None,
            attributes: Some(serde_json::Value::Object(attrs)),
            embedding: None,
            mutable: true,
            access_count: 0,
            created_at: serde_json::Value::String("2026-01-01".into()),
            updated_at: serde_json::Value::String("2026-01-01".into()),
            source: None,
        };

        assert!(entity_needs_update(&entry, &existing));
    }
}
