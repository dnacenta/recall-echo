// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Entity and relationship CRUD operations.

use surrealdb::Surreal;

use super::confidence::{EdgeEvidence, Evidence, Observation, Provenance};
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

    // A new edge starts at the prior: its mean is the requested confidence,
    // its concentration is PRIOR_CONCENTRATION. The mean is stored as
    // requested rather than re-derived, so creation is bit-for-bit unchanged.
    let confidence = rel.confidence.unwrap_or(1.0) as f64;
    let evidence = Evidence::from_prior(confidence);

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
                alpha = $alpha,
                beta = $beta,
                self_reinforcements = 0,
                last_reinforced = time::now(),
                source = $source
            "#,
        )
        .bind(("from_id", from_id))
        .bind(("to_id", to_id))
        .bind(("rel_type", rel.rel_type))
        .bind(("description", rel.description))
        .bind(("confidence", confidence))
        .bind(("alpha", evidence.alpha()))
        .bind(("beta", evidence.beta()))
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

/// Overwrite a relationship's confidence, discarding its accumulated evidence.
///
/// This is an assertion of a mean, not an observation: the edge is reset to the
/// prior concentration around the new value, so the stored counts keep matching
/// the stored mean. Use [`reinforce_relationship`] to *add* evidence.
pub async fn update_relationship_confidence(
    db: &Surreal<Db>,
    rel_id: &str,
    confidence: f64,
) -> Result<(), GraphError> {
    let evidence = Evidence::from_prior(confidence);
    db.query("UPDATE type::record($id) SET confidence = $confidence, alpha = $alpha, beta = $beta")
        .bind(("id", rel_id.to_string()))
        .bind(("confidence", confidence))
        .bind(("alpha", evidence.alpha()))
        .bind(("beta", evidence.beta()))
        .await?
        .check()?;
    Ok(())
}

/// Persist updated evidence for a relationship, moving its decay clock only if
/// the observation earned it.
///
/// The caller loads the edge's [`EdgeEvidence`], records an [`Observation`]
/// with its provenance, and hands both here — the same value that moved the
/// counts also decides the anchor, so the two can never disagree. Corroboration
/// sets `last_reinforced = now` because the belief was just seen again;
/// contradiction leaves the anchor alone, which keeps the effect of "this is
/// wrong" monotone: the posterior mean falls, and every day of decay already
/// applied to the edge stays applied.
///
/// The counts are written whole rather than incremented in SurrealQL: the
/// caller has already read them, and a full write keeps mean and counts from
/// ever disagreeing.
pub async fn record_observation(
    db: &Surreal<Db>,
    rel_id: &str,
    evidence: EdgeEvidence,
    observation: Observation,
) -> Result<(), GraphError> {
    let counts = evidence.evidence();
    let anchor = if observation.renews_decay_anchor() {
        ",\n               last_reinforced = time::now()"
    } else {
        ""
    };
    db.query(format!(
        r#"UPDATE type::record($id) SET
               confidence = $confidence,
               alpha = $alpha,
               beta = $beta,
               self_reinforcements = $self_reinforcements{anchor}"#
    ))
    .bind(("id", rel_id.to_string()))
    .bind(("confidence", counts.mean()))
    .bind(("alpha", counts.alpha()))
    .bind(("beta", counts.beta()))
    .bind(("self_reinforcements", evidence.self_reinforcements()))
    .await?
    .check()?;
    Ok(())
}

/// Persist corroborating evidence for a relationship and reset its decay clock.
///
/// [`record_observation`] under [`Observation::Corroborating`], named for the
/// one thing it may be used for.
pub async fn reinforce_relationship(
    db: &Surreal<Db>,
    rel_id: &str,
    evidence: EdgeEvidence,
) -> Result<(), GraphError> {
    record_observation(db, rel_id, evidence, Observation::Corroborating).await
}

/// Persist contradicting evidence for a relationship **without** restarting its
/// decay clock.
///
/// [`record_observation`] under [`Observation::Contradicting`]. Reaching for
/// [`reinforce_relationship`] here instead would let a correction *raise* an
/// edge's effective confidence: a stale edge stored at 0.6 and decayed to 0.3
/// would come back at 0.545 undecayed, more visible after being denied than
/// before it.
pub async fn contradict_relationship(
    db: &Surreal<Db>,
    rel_id: &str,
    evidence: EdgeEvidence,
) -> Result<(), GraphError> {
    record_observation(db, rel_id, evidence, Observation::Contradicting).await
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

/// Add a new episode authored by the agent itself.
///
/// The conservative default for callers that cannot say where the text came
/// from: unattributed text must never be counted as independent evidence.
pub async fn add_episode(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    episode: NewEpisode,
) -> Result<Episode, GraphError> {
    add_episode_from(db, embedder, episode, Provenance::SelfGenerated).await
}

/// Add a new episode stamped with who authored it. Embeds the abstract text
/// for vector search.
///
/// Provenance is an argument rather than a field of [`NewEpisode`] because it
/// is a property of the *ingestion context*, not of the text: the same chunk
/// is external when read out of a document and self-authored when the agent
/// wrote it.
pub async fn add_episode_from(
    db: &Surreal<Db>,
    embedder: &dyn Embedder,
    episode: NewEpisode,
    provenance: Provenance,
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
                log_number = $log_number,
                provenance = $provenance
            "#,
        )
        .bind(("session_id", episode.session_id))
        .bind(("abstract", episode.abstract_text))
        .bind(("overview", episode.overview))
        .bind(("content", episode.content))
        .bind(("embedding", embedding))
        .bind(("log_number", episode.log_number.map(|n| n as i64)))
        .bind(("provenance", provenance.as_str().to_string()))
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

/// Delete a single episode by its record ID.
pub async fn delete_episode(db: &Surreal<Db>, id: &str) -> Result<(), GraphError> {
    db.query("DELETE FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?
        .check()?;
    Ok(())
}

/// Batch increment episode retrieval counts.
///
/// Coalesces the absent case: episodes written before the counter existed
/// read as NONE, and `NONE + 1` is not a count.
pub async fn increment_episode_access_counts(
    db: &Surreal<Db>,
    ids: &[String],
) -> Result<(), GraphError> {
    for id in ids {
        let _ = db
            .query("UPDATE type::record($id) SET access_count = (access_count ?? 0) + 1")
            .bind(("id", id.clone()))
            .await;
    }

    Ok(())
}

/// Mark every episode of one archive extracted — run once per archive by
/// both the daemon and `graph extract`, so on a backlog drain it is paid as
/// many times as there are archives. Served by the `episode_log` index.
const MARK_EXTRACTED: &str = "UPDATE episode SET extracted = true WHERE log_number = $ln";

/// Mark all episodes with a given log_number as extracted.
pub async fn mark_episodes_extracted(db: &Surreal<Db>, log_number: u32) -> Result<(), GraphError> {
    db.query(MARK_EXTRACTED)
        .bind(("ln", log_number as i64))
        .await?
        .check()?;
    Ok(())
}

/// The rows the extraction scan is looking for, as a literal fragment shared
/// by the scan and its count so the two can never disagree.
macro_rules! unextracted_source {
    () => {
        "FROM episode WHERE extracted = false AND log_number IS NOT NONE"
    };
}

/// The extraction scan, as one statement so the plan test explains exactly
/// what the daemon runs.
///
/// `extracted = false` is a plain field comparison, which the planner serves
/// from the `episode_extracted` index. The background worker runs this once
/// per poll interval — fixed per daemon at `(idle_after_secs / 4)` clamped to
/// 100ms–30s, so 30s at the default — for as long as the machine stays quiet,
/// which is exactly when there is least to find. A full table scan here is a
/// permanent tax. `log_number IS NOT NONE` cannot be indexed and does not
/// need to be: it filters the *output* of the index scan.
///
/// The index makes this cheaper, not free, and not O(pending): SurrealKV
/// keeps superseded entries in the `= false` range until compaction, so an
/// episode that has been extracted still costs the scan something until then
/// — measured ~15× below the unindexed predicate at 40k episodes, growing
/// with each extraction cycle and partly reclaimed on reopen. Truly bounding
/// it by the pending set would take a separate table keyed by log number with
/// rows deleted on extraction; out of scope here.
///
/// This predicate is only correct because schema version 2 backfilled every
/// absent `extracted` to `false` (`store::backfill_episode_extracted`) and
/// that backfill fails the open rather than leaving a row behind. An episode
/// with no value at all is invisible here — which is what
/// [`crate::graph::store::count_absent_extracted`] exists to catch.
const UNEXTRACTED_SCAN: &str = concat!(
    "SELECT log_number ",
    unextracted_source!(),
    " GROUP BY log_number ORDER BY log_number"
);

/// How many distinct archives the scan would process, counted by the store
/// rather than by collecting every log number into a `Vec` to call `.len()`
/// on it. Same grouped source as [`UNEXTRACTED_SCAN`], one integer back.
const UNEXTRACTED_COUNT: &str = concat!(
    "SELECT count() AS count FROM (SELECT log_number ",
    unextracted_source!(),
    " GROUP BY log_number) GROUP ALL"
);

/// Get distinct log numbers of episodes that have NOT been extracted.
pub async fn get_unextracted_log_numbers(db: &Surreal<Db>) -> Result<Vec<i64>, GraphError> {
    let mut response = db.query(UNEXTRACTED_SCAN).await?;

    #[derive(serde::Deserialize)]
    struct Row {
        log_number: i64,
    }

    let rows: Vec<Row> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.into_iter().map(|r| r.log_number).collect())
}

/// How many distinct archives are pending extraction.
pub async fn count_unextracted_logs(db: &Surreal<Db>) -> Result<u64, GraphError> {
    #[derive(serde::Deserialize)]
    struct CountRow {
        count: u64,
    }

    let mut response = db.query(UNEXTRACTED_COUNT).await?;
    let rows: Vec<CountRow> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.first().map(|r| r.count).unwrap_or(0))
}

/// Count episodes missing the `extracted` flag and missing a `log_number`,
/// in one pass over the table — `count(expr)` counts truthy values, so both
/// diagnostics share the scan. Returns `(extracted_absent, log_number_absent)`.
///
/// `extracted_absent` is zero on every store schema version 2 has opened, by
/// design: the migration gives every episode a value. It is kept because it
/// is the assertion that the migration landed — the extraction scan matches
/// `extracted = false`, so an episode that still has no value is not merely
/// slow to find, it can never be found, and nothing else would say so.
pub async fn episode_absent_field_counts(db: &Surreal<Db>) -> Result<(u64, u64), GraphError> {
    #[derive(serde::Deserialize)]
    struct AbsentCounts {
        extracted_absent: u64,
        log_number_absent: u64,
    }

    let mut response = db
        .query(
            "SELECT count(extracted IS NONE) AS extracted_absent, \
             count(log_number IS NONE) AS log_number_absent \
             FROM episode GROUP ALL",
        )
        .await?;
    let rows: Vec<AbsentCounts> = super::deserialize_take(&mut response, 0)?;
    Ok(rows
        .first()
        .map(|r| (r.extracted_absent, r.log_number_absent))
        .unwrap_or((0, 0)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::store;

    /// A legacy episode row — written before the `extracted` field existed —
    /// must still be found by the unextracted scan. Replays the real-world
    /// sequence: old schema, insert, schema upgrade, scan. Since RE-44 the
    /// upgrade backfills the absent value, so the scan sees the row through
    /// the `episode_extracted` index rather than through a `??`.
    #[tokio::test]
    async fn scan_finds_episodes_with_absent_extracted_field() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = store::open(dir.path()).await.expect("open store");

        // The episode table as it existed before `extracted` was defined.
        db.query(
            r#"
            DEFINE TABLE episode SCHEMAFULL;
            DEFINE FIELD session_id ON episode TYPE string;
            DEFINE FIELD timestamp  ON episode TYPE datetime DEFAULT time::now();
            DEFINE FIELD abstract   ON episode TYPE string;
            DEFINE FIELD log_number ON episode TYPE option<int>;
            "#,
        )
        .await
        .expect("legacy schema")
        .check()
        .expect("legacy schema check");

        db.query("CREATE episode SET session_id = 'legacy', abstract = 'old row', log_number = 7")
            .await
            .expect("legacy insert")
            .check()
            .expect("legacy insert check");

        // Upgrade to the current schema. `IF NOT EXISTS` adds `extracted`
        // with its DEFAULT — which applies at creation, not retroactively —
        // and the version-2 migration backfills the row that predates it.
        let migration = store::init_schema(&db).await.expect("schema upgrade");
        assert_eq!(migration.episodes_backfilled, 1, "{migration:?}");

        db.query("CREATE episode SET session_id = 'modern', abstract = 'new row', log_number = 9")
            .await
            .expect("modern insert")
            .check()
            .expect("modern insert check");

        // A modern row with no log_number — not tied to any archive, so the
        // scan can never reach it; only the diagnostics can see it.
        db.query("CREATE episode SET session_id = 'orphan', abstract = 'no archive'")
            .await
            .expect("orphan insert")
            .check()
            .expect("orphan insert check");

        let logs = get_unextracted_log_numbers(&db).await.expect("scan");
        assert_eq!(
            logs,
            vec![7, 9],
            "legacy and modern rows must both be visible"
        );

        // The diagnostics run against the same store. `extracted_absent` is
        // zero by design after RE-44: the migration gave the legacy row a
        // value, and any non-zero count would mean it had not. The orphan row
        // is still the one missing `log_number`.
        let (extracted_absent, log_number_absent) =
            episode_absent_field_counts(&db).await.expect("diagnostics");
        assert_eq!(extracted_absent, 0, "the migration reached every episode");
        assert_eq!(log_number_absent, 1);

        // Marking extracted removes a log from the scan either way.
        mark_episodes_extracted(&db, 7).await.expect("mark");
        let logs = get_unextracted_log_numbers(&db).await.expect("rescan");
        assert_eq!(logs, vec![9]);
    }

    /// The whole point of RE-44: the statement the daemon polls must be
    /// served by the `episode_extracted` index. Nothing else in the suite
    /// can tell a correct index scan from a correct full-table scan, so a
    /// future edit to the predicate would silently reinstate the tax this
    /// issue removed.
    /// `EXPLAIN FULL` of `sql`, as a JSON tree.
    async fn plan_of(db: &Surreal<Db>, sql: &str) -> serde_json::Value {
        let mut response = db
            .query(format!("{sql} EXPLAIN FULL"))
            .bind(("ln", 1i64))
            .await
            .expect("explain");
        let rows: Vec<serde_json::Value> =
            super::super::deserialize_take(&mut response, 0).expect("explain rows");
        serde_json::Value::Array(rows)
    }

    async fn store_with_one_episode(dir: &std::path::Path) -> Surreal<Db> {
        let db = store::open(dir).await.expect("open store");
        store::init_schema(&db).await.expect("schema");
        db.query("CREATE episode SET session_id = 's', abstract = 'a', log_number = 1")
            .await
            .expect("insert")
            .check()
            .expect("insert check");
        db
    }

    #[tokio::test]
    async fn the_extraction_scan_is_served_by_the_index() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = store_with_one_episode(dir.path()).await;

        let plan = plan_of(&db, UNEXTRACTED_SCAN).await;
        assert!(
            plan_uses_index(&plan, "episode_extracted"),
            "the extraction scan must read the episode_extracted index \
             (SurrealDB 3.2.4), not scan the table. Plan: {plan:#}"
        );

        // The count the status path runs shares the scan's source, so it must
        // reach the same index.
        let plan = plan_of(&db, UNEXTRACTED_COUNT).await;
        assert!(
            plan_uses_index(&plan, "episode_extracted"),
            "the pending count must use the same index (SurrealDB 3.2.4). \
             Plan: {plan:#}"
        );
    }

    /// Marking one archive extracted runs once per archive — on a backlog
    /// drain, as many times as there are archives — so it has to be served by
    /// the `episode_log` index rather than scanning the episode table.
    #[tokio::test]
    async fn marking_an_archive_extracted_is_served_by_the_log_index() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = store_with_one_episode(dir.path()).await;

        let plan = plan_of(&db, MARK_EXTRACTED).await;
        assert!(
            plan_uses_index(&plan, "episode_log"),
            "marking must read the episode_log index (SurrealDB 3.2.4), not \
             scan the table. Plan: {plan:#}"
        );
    }

    /// The count and the scan must always agree about which archives are
    /// pending — they share a source fragment precisely so they cannot drift.
    #[tokio::test]
    async fn the_pending_count_agrees_with_the_pending_scan() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = store::open(dir.path()).await.expect("open store");
        store::init_schema(&db).await.expect("schema");

        assert_eq!(count_unextracted_logs(&db).await.expect("count"), 0);

        for log_number in [3, 3, 4, 9] {
            db.query("CREATE episode SET session_id = 's', abstract = 'a', log_number = $ln")
                .bind(("ln", log_number as i64))
                .await
                .expect("insert")
                .check()
                .expect("insert check");
        }
        mark_episodes_extracted(&db, 4).await.expect("mark");

        let scanned = get_unextracted_log_numbers(&db).await.expect("scan");
        assert_eq!(scanned, vec![3, 9], "distinct, extracted excluded");
        assert_eq!(
            count_unextracted_logs(&db).await.expect("count"),
            scanned.len() as u64,
            "the count is the length of the scan, computed by the store"
        );
    }

    /// Walk an `EXPLAIN FULL` plan tree for a step that *reads* a named
    /// index. Structural rather than a substring match on serialized JSON, so
    /// an index named in a predicate string — or a plan whose fields move —
    /// cannot pass for a plan that actually uses it.
    ///
    /// SurrealDB 3.2.4 prints two plan shapes: `SELECT` gives an operator
    /// tree (`{"operator": "IndexScan", "attributes": {"index": …}}`) and
    /// `UPDATE` gives the older flat form (`{"operation": "Iterate Index",
    /// "detail": {"plan": {"index": …}}}`). Both count; a table scan in
    /// either shape does not.
    fn plan_uses_index(node: &serde_json::Value, index: &str) -> bool {
        match node {
            serde_json::Value::Array(items) => items.iter().any(|i| plan_uses_index(i, index)),
            serde_json::Value::Object(map) => {
                let named = |value: Option<&serde_json::Value>| {
                    value.and_then(|v| v.as_str()) == Some(index)
                };
                let operator_tree = matches!(
                    map.get("operator").and_then(|o| o.as_str()),
                    Some("IndexScan" | "IndexCountScan")
                ) && named(map.get("attributes").and_then(|a| a.get("index")));
                let iterate_index = map.get("operation").and_then(|o| o.as_str())
                    == Some("Iterate Index")
                    && named(
                        map.get("detail")
                            .and_then(|d| d.get("plan"))
                            .and_then(|p| p.get("index")),
                    );

                operator_tree
                    || iterate_index
                    || map
                        .get("children")
                        .is_some_and(|c| plan_uses_index(c, index))
            }
            _ => false,
        }
    }

    #[test]
    fn plan_walker_finds_both_shapes_and_nothing_else() {
        // SELECT: nested operator tree.
        let select = serde_json::json!([{
            "operator": "Sort",
            "children": [{
                "operator": "IndexScan",
                "attributes": { "index": "episode_extracted", "access": "= false" }
            }]
        }]);
        assert!(plan_uses_index(&select, "episode_extracted"));
        assert!(!plan_uses_index(&select, "episode_time"));

        // UPDATE: flat legacy form.
        let update = serde_json::json!([{
            "operation": "Iterate Index",
            "detail": { "table": "episode", "plan": { "index": "episode_log", "operator": "=" } }
        }]);
        assert!(plan_uses_index(&update, "episode_log"));
        assert!(!plan_uses_index(&update, "episode_extracted"));

        // A table scan that merely mentions the index is not a use of it.
        let table_scan = serde_json::json!([
            { "operator": "TableScan", "attributes": { "predicate": "episode_extracted = false" } },
            { "operation": "Iterate Table", "detail": { "table": "episode" } }
        ]);
        assert!(!plan_uses_index(&table_scan, "episode_extracted"));
    }
}
