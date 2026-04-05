//! Entity and relationship CRUD operations.

use surrealdb::Surreal;

use super::embed::Embedder;
use super::error::GraphError;
use super::store::Db;
use super::types::*;
use super::{deserialize_take, deserialize_take_opt};

/// Add a new entity to the graph. Embeds the abstract text for vector search.
pub async fn add_entity(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    entity: NewEntity,
) -> Result<Entity, GraphError> {
    let embedding = embedder.embed_single(&entity.abstract_text)?;
    let mutable = entity.entity_type.is_mutable();

    let mut response = db
        .query(
            r#"
            CREATE entity SET
                name = $name,
                entity_type = $entity_type,
                abstract = $abstract,
                overview = $overview,
                content = $content,
                attributes = $attributes,
                embedding = $embedding,
                mutable = $mutable,
                access_count = 0,
                created_at = time::now(),
                updated_at = time::now(),
                source = $source
            "#,
        )
        .bind(("name", entity.name))
        .bind(("entity_type", entity.entity_type.to_string()))
        .bind(("abstract", entity.abstract_text))
        .bind(("overview", entity.overview.unwrap_or_default()))
        .bind(("content", entity.content))
        .bind(("attributes", entity.attributes))
        .bind(("embedding", embedding))
        .bind(("mutable", mutable))
        .bind(("source", entity.source))
        .await?;

    let created: Option<Entity> = deserialize_take_opt(&mut response, 0)?;
    created
        .ok_or_else(|| GraphError::Db(surrealdb::Error::thrown("failed to create entity".into())))
}

/// Get an entity by name.
pub async fn get_entity_by_name(
    db: &Surreal<Db>,
    name: &str,
) -> Result<Option<Entity>, GraphError> {
    let mut response = db
        .query("SELECT * FROM entity WHERE name = $name LIMIT 1")
        .bind(("name", name.to_string()))
        .await?;

    deserialize_take_opt(&mut response, 0)
}

/// Get an entity by its record ID string (e.g. "entity:abc123").
pub async fn get_entity_by_id(db: &Surreal<Db>, id: &str) -> Result<Option<Entity>, GraphError> {
    let mut response = db
        .query("SELECT * FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?;

    deserialize_take_opt(&mut response, 0)
}

/// Update an entity's fields. Re-embeds if abstract text changed.
pub async fn update_entity(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    id: &str,
    updates: EntityUpdate,
) -> Result<Entity, GraphError> {
    let mut sets = vec!["updated_at = time::now()".to_string()];
    let mut bindings: Vec<(String, serde_json::Value)> = vec![];

    if let Some(ref abs) = updates.abstract_text {
        sets.push("abstract = $new_abstract".to_string());
        bindings.push((
            "new_abstract".to_string(),
            serde_json::Value::String(abs.clone()),
        ));

        let embedding = embedder.embed_single(abs)?;
        sets.push("embedding = $new_embedding".to_string());
        bindings.push(("new_embedding".to_string(), serde_json::json!(embedding)));
    }
    if let Some(ref ov) = updates.overview {
        sets.push("overview = $new_overview".to_string());
        bindings.push((
            "new_overview".to_string(),
            serde_json::Value::String(ov.clone()),
        ));
    }
    if let Some(ref ct) = updates.content {
        sets.push("content = $new_content".to_string());
        bindings.push((
            "new_content".to_string(),
            serde_json::Value::String(ct.clone()),
        ));
    }
    if let Some(ref attr) = updates.attributes {
        sets.push("attributes = $new_attributes".to_string());
        bindings.push(("new_attributes".to_string(), attr.clone()));
    }

    let query = format!(
        "UPDATE type::record($id) SET {} RETURN AFTER",
        sets.join(", ")
    );

    let id_owned = id.to_string();
    let mut q = db.query(&query).bind(("id", id_owned));
    for (k, v) in bindings {
        q = q.bind((k, v));
    }

    let mut response = q.await?;
    let updated: Vec<Entity> = deserialize_take(&mut response, 0)?;
    updated
        .into_iter()
        .next()
        .ok_or_else(|| GraphError::NotFound(id.to_string()))
}

/// Delete an entity and all its relationships.
pub async fn delete_entity(db: &Surreal<Db>, id: &str) -> Result<(), GraphError> {
    let id_owned = id.to_string();
    db.query(
        r#"
        DELETE FROM relates_to WHERE in = type::record($id) OR out = type::record($id);
        DELETE FROM type::record($id);
        "#,
    )
    .bind(("id", id_owned))
    .await?
    .check()?;

    Ok(())
}

/// List entities, optionally filtered by type.
pub async fn list_entities(
    db: &Surreal<Db>,
    entity_type: Option<&str>,
) -> Result<Vec<Entity>, GraphError> {
    let mut response = if let Some(et) = entity_type {
        db.query("SELECT * FROM entity WHERE entity_type = $et ORDER BY name")
            .bind(("et", et.to_string()))
            .await?
    } else {
        db.query("SELECT * FROM entity ORDER BY name").await?
    };

    deserialize_take(&mut response, 0)
}

/// Create a relationship between two entities (resolved by name).
pub async fn add_relationship(
    db: &Surreal<Db>,
    rel: NewRelationship,
) -> Result<Relationship, GraphError> {
    let from = get_entity_by_name(db, &rel.from_entity)
        .await?
        .ok_or_else(|| GraphError::NotFound(rel.from_entity.clone()))?;
    let to = get_entity_by_name(db, &rel.to_entity)
        .await?
        .ok_or_else(|| GraphError::NotFound(rel.to_entity.clone()))?;

    let from_id = from.id_string();
    let to_id = to.id_string();

    let mut response = db
        .query(
            r#"
            LET $from = type::record($from_id);
            LET $to = type::record($to_id);
            RELATE $from -> relates_to -> $to SET
                rel_type = $rel_type,
                description = $description,
                valid_from = time::now(),
                valid_until = NONE,
                confidence = $confidence,
                last_reinforced = time::now(),
                source = $source
            "#,
        )
        .bind(("from_id", from_id))
        .bind(("to_id", to_id))
        .bind(("rel_type", rel.rel_type))
        .bind(("description", rel.description))
        .bind(("confidence", rel.confidence.unwrap_or(1.0) as f64))
        .bind(("source", rel.source))
        .await?;

    // Index 2 because LET statements are at index 0 and 1
    let created: Option<Relationship> = deserialize_take_opt(&mut response, 2)?;
    created.ok_or_else(|| {
        GraphError::Db(surrealdb::Error::thrown(
            "failed to create relationship".into(),
        ))
    })
}

/// Get relationships for an entity.
pub async fn get_relationships(
    db: &Surreal<Db>,
    entity_name: &str,
    direction: Direction,
) -> Result<Vec<Relationship>, GraphError> {
    let entity = get_entity_by_name(db, entity_name)
        .await?
        .ok_or_else(|| GraphError::NotFound(entity_name.to_string()))?;

    let entity_id = entity.id_string();

    let query = match direction {
        Direction::Outgoing => "SELECT * FROM relates_to WHERE in = type::record($id)",
        Direction::Incoming => "SELECT * FROM relates_to WHERE out = type::record($id)",
        Direction::Both => {
            "SELECT * FROM relates_to WHERE in = type::record($id) OR out = type::record($id)"
        }
    };

    let mut response = db.query(query).bind(("id", entity_id)).await?;
    deserialize_take(&mut response, 0)
}

/// Update a relationship's confidence score.
pub async fn update_relationship_confidence(
    db: &Surreal<Db>,
    rel_id: &str,
    confidence: f64,
) -> Result<(), GraphError> {
    db.query("UPDATE type::record($id) SET confidence = $confidence")
        .bind(("id", rel_id.to_string()))
        .bind(("confidence", confidence))
        .await?
        .check()?;
    Ok(())
}

/// Reinforce a relationship: Bayesian update + reset last_reinforced timestamp.
///
/// Called when a relationship is corroborated by re-extraction. Updates confidence
/// via Bayesian posterior and resets the decay clock by setting `last_reinforced = now`.
pub async fn reinforce_relationship(
    db: &Surreal<Db>,
    rel_id: &str,
    new_confidence: f64,
) -> Result<(), GraphError> {
    db.query("UPDATE type::record($id) SET confidence = $confidence, last_reinforced = time::now()")
        .bind(("id", rel_id.to_string()))
        .bind(("confidence", new_confidence))
        .await?
        .check()?;
    Ok(())
}

/// Supersede an existing relationship: set valid_until on the old one, create a new one.
pub async fn supersede_relationship(
    db: &Surreal<Db>,
    old_id: &str,
    new: NewRelationship,
) -> Result<Relationship, GraphError> {
    let old_id_owned = old_id.to_string();
    db.query("UPDATE type::record($id) SET valid_until = time::now()")
        .bind(("id", old_id_owned))
        .await?
        .check()?;

    add_relationship(db, new).await
}

// ── Tiered queries ───────────────────────────────────────────────────

/// Get an entity summary (L0 — minimal, no embedding/content).
pub async fn get_entity_summary(
    db: &Surreal<Db>,
    id: &str,
) -> Result<Option<EntitySummary>, GraphError> {
    let mut response = db
        .query("SELECT id, name, entity_type, abstract FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?;

    deserialize_take_opt(&mut response, 0)
}

/// Get an entity detail (L1 — no embedding/content).
pub async fn get_entity_detail(
    db: &Surreal<Db>,
    id: &str,
) -> Result<Option<EntityDetail>, GraphError> {
    let mut response = db
        .query(
            r#"SELECT id, name, entity_type, abstract, overview, attributes,
                      access_count, updated_at, source
               FROM type::record($id)"#,
        )
        .bind(("id", id.to_string()))
        .await?;

    deserialize_take_opt(&mut response, 0)
}

/// Delete a single relationship by its record ID.
pub async fn delete_relationship(db: &Surreal<Db>, id: &str) -> Result<(), GraphError> {
    db.query("DELETE FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?
        .check()?;
    Ok(())
}

/// Get all relationships in the graph (for GC scanning).
pub async fn list_all_relationships(db: &Surreal<Db>) -> Result<Vec<Relationship>, GraphError> {
    let mut response = db.query("SELECT * FROM relates_to").await?;
    super::deserialize_take(&mut response, 0)
}

/// Count relationships for a given entity.
pub async fn count_relationships(db: &Surreal<Db>, entity_id: &str) -> Result<u64, GraphError> {
    let mut response = db
        .query(
            r#"SELECT count() AS count FROM relates_to
               WHERE in = type::record($id) OR out = type::record($id)
               GROUP ALL"#,
        )
        .bind(("id", entity_id.to_string()))
        .await?;

    #[derive(serde::Deserialize)]
    struct Row {
        count: u64,
    }

    let rows: Vec<Row> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.first().map(|r| r.count).unwrap_or(0))
}

/// Batch increment access counts for multiple entities.
pub async fn increment_access_counts(db: &Surreal<Db>, ids: &[String]) -> Result<(), GraphError> {
    if ids.is_empty() {
        return Ok(());
    }

    for id in ids {
        let _ = db
            .query("UPDATE type::record($id) SET access_count += 1")
            .bind(("id", id.clone()))
            .await;
    }

    Ok(())
}

// ── Episode CRUD ─────────────────────────────────────────────────────

/// Add a new episode to the graph. Embeds the abstract text for vector search.
pub async fn add_episode(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    episode: NewEpisode,
) -> Result<Episode, GraphError> {
    let embedding = embedder.embed_single(&episode.abstract_text)?;

    let mut response = db
        .query(
            r#"
            CREATE episode SET
                session_id = $session_id,
                timestamp = time::now(),
                abstract = $abstract,
                overview = $overview,
                content = $content,
                embedding = $embedding,
                log_number = $log_number
            "#,
        )
        .bind(("session_id", episode.session_id))
        .bind(("abstract", episode.abstract_text))
        .bind(("overview", episode.overview))
        .bind(("content", episode.content))
        .bind(("embedding", embedding))
        .bind(("log_number", episode.log_number.map(|n| n as i64)))
        .await?;

    let created: Option<Episode> = deserialize_take_opt(&mut response, 0)?;
    created
        .ok_or_else(|| GraphError::Db(surrealdb::Error::thrown("failed to create episode".into())))
}

/// Get episodes by session ID.
pub async fn get_episodes_by_session(
    db: &Surreal<Db>,
    session_id: &str,
) -> Result<Vec<Episode>, GraphError> {
    let mut response = db
        .query("SELECT * FROM episode WHERE session_id = $sid ORDER BY timestamp")
        .bind(("sid", session_id.to_string()))
        .await?;

    deserialize_take(&mut response, 0)
}

/// Mark all episodes with a given log_number as extracted.
pub async fn mark_episodes_extracted(db: &Surreal<Db>, log_number: u32) -> Result<(), GraphError> {
    db.query("UPDATE episode SET extracted = true WHERE log_number = $ln")
        .bind(("ln", log_number as i64))
        .await?
        .check()?;
    Ok(())
}

/// Get distinct log numbers of episodes that have NOT been extracted.
pub async fn get_unextracted_log_numbers(db: &Surreal<Db>) -> Result<Vec<i64>, GraphError> {
    let mut response = db
        .query("SELECT log_number FROM episode WHERE extracted = false AND log_number IS NOT NONE GROUP BY log_number ORDER BY log_number")
        .await?;

    #[derive(serde::Deserialize)]
    struct Row {
        log_number: i64,
    }

    let rows: Vec<Row> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.into_iter().map(|r| r.log_number).collect())
}

/// Get episode by log number.
pub async fn get_episode_by_log_number(
    db: &Surreal<Db>,
    log_number: u32,
) -> Result<Option<Episode>, GraphError> {
    let mut response = db
        .query("SELECT * FROM episode WHERE log_number = $ln LIMIT 1")
        .bind(("ln", log_number as i64))
        .await?;

    deserialize_take_opt(&mut response, 0)
}
