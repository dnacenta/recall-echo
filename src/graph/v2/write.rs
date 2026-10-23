// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Writes for the second pipeline: exact name-and-kind resolution, edges,
//! slots, and reinforcement of an identical live fact.
//!
//! Nothing here retires or contradicts an edge. A repeated fact adds evidence
//! and corroborates. That keeps the prior recovered from the edge's first β:
//! the creating session never moves β.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::super::confidence::{EdgeEvidence, Evidence, ExtractionContext, Provenance};
use super::super::crud;
use super::super::error::GraphError;
use super::super::store::Db;
use super::super::types::{Entity, EntityUpdate, NewEntity};
use super::super::{deserialize_take, GraphMemory};
use super::input::TurnRole;
use super::schema::{normalise, Fact, Kind};
use surrealdb::Surreal;

/// Shown when v2 is asked to write into a store it did not build.
pub(crate) const V2_REFUSAL: &str = "this graph was built by pipeline v1; re-extract into a fresh store with: recall-echo graph extract --pipeline v2 --rebuild";

const STAMP_V2: &str = "v2";

/// What one batch of facts did to the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WriteStats {
    pub entities_created: u32,
    pub edges: u32,
    pub slots: u32,
    pub reinforced: u32,
}

/// Refuse a store this pipeline must not write, otherwise stamp it `v2`.
///
/// A stamp of `v2` proceeds. An empty store with no stamp proceeds and is
/// stamped. Anything else — a `v1` stamp, or entities and no stamp — refuses
/// before a provider is called.
pub(crate) async fn ensure_v2_store(gm: &GraphMemory) -> Result<(), GraphError> {
    match read_stamp(gm.db()).await? {
        Some(stamp) if stamp == STAMP_V2 => Ok(()),
        None if entity_count(gm.db()).await? == 0 => write_stamp(gm.db(), STAMP_V2).await,
        _ => Err(GraphError::Extraction(V2_REFUSAL.to_string())),
    }
}

/// The pipeline stamp on `meta:pipeline`, if one was written.
pub(crate) async fn read_stamp(db: &Surreal<Db>) -> Result<Option<String>, GraphError> {
    let mut response = db.query("SELECT pipeline FROM meta:pipeline").await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(rows.into_iter().find_map(pipeline_field))
}

fn pipeline_field(row: Value) -> Option<String> {
    let text = row.get("pipeline")?.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

async fn write_stamp(db: &Surreal<Db>, stamp: &str) -> Result<(), GraphError> {
    db.query("UPSERT meta:pipeline SET pipeline = $pipeline")
        .bind(("pipeline", stamp.to_string()))
        .await?
        .check()?;
    Ok(())
}

async fn entity_count(db: &Surreal<Db>) -> Result<u64, GraphError> {
    let mut response = db
        .query("SELECT count() AS count FROM entity GROUP ALL")
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(json_u64)
        .unwrap_or(0))
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
}

/// Resolve every fact and write the edge or slot it asks for.
pub(crate) async fn write_facts(
    gm: &GraphMemory,
    session_id: &str,
    date: &str,
    facts: &[Fact],
) -> Result<WriteStats, GraphError> {
    let mut stats = WriteStats::default();
    let mut catalog = Catalog::load(gm).await?;
    for fact in facts {
        write_one(gm, &mut catalog, session_id, date, fact, &mut stats).await?;
    }
    Ok(stats)
}

async fn write_one(
    gm: &GraphMemory,
    catalog: &mut Catalog,
    session_id: &str,
    date: &str,
    fact: &Fact,
    stats: &mut WriteStats,
) -> Result<(), GraphError> {
    let subject = catalog
        .resolve(gm, &fact.subject_name, fact.subject_kind, stats)
        .await?;
    if fact.predicate.is_edge() {
        let object_kind = fact.object_kind.unwrap_or(Kind::Technology);
        let object = catalog
            .resolve(gm, &fact.object_name, object_kind, stats)
            .await?;
        write_edge(gm, session_id, fact, &subject, &object, stats).await
    } else {
        write_slot(gm, catalog, session_id, date, fact, &subject, stats).await
    }
}

async fn write_edge(
    gm: &GraphMemory,
    session_id: &str,
    fact: &Fact,
    subject: &Stored,
    object: &Stored,
    stats: &mut WriteStats,
) -> Result<(), GraphError> {
    let description = fact.role.map(|role| format!("role={}", role.as_str()));
    if let Some(existing) = find_edge(gm, subject, object, fact, description.as_deref()).await? {
        reinforce_edge(gm, &existing, fact, session_id).await?;
        stats.reinforced += 1;
        return Ok(());
    }
    create_edge(gm, session_id, fact, subject, object, description).await?;
    stats.edges += 1;
    Ok(())
}

async fn find_edge(
    gm: &GraphMemory,
    subject: &Stored,
    object: &Stored,
    fact: &Fact,
    description: Option<&str>,
) -> Result<Option<StoredEdge>, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT * FROM relates_to WHERE in = type::record($id) AND valid_until = NONE")
        .bind(("id", subject.id.clone()))
        .await?;
    let edges: Vec<StoredEdge> = deserialize_take(&mut response, 0)?;
    Ok(edges.into_iter().find(|edge| {
        edge.rel_type == fact.predicate.as_str()
            && edge.status.as_deref() == Some(fact.status.as_str())
            && edge.description.as_deref() == description
            && record_id(&edge.to_id) == object.id
            && edge.valid_until.is_none()
    }))
}

async fn create_edge(
    gm: &GraphMemory,
    session_id: &str,
    fact: &Fact,
    subject: &Stored,
    object: &Stored,
    description: Option<String>,
) -> Result<(), GraphError> {
    let (_, label) = provenance_of(fact.provenance);
    let evidence = Evidence::from_prior(ExtractionContext::Explicit.prior());
    let item = evidence_item(session_id, fact, label);
    gm.db()
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
                source = NONE,
                status = $status,
                provenance = $provenance,
                ended_by = NONE,
                evidence = $evidence
            "#,
        )
        .bind(("from_id", subject.id.clone()))
        .bind(("to_id", object.id.clone()))
        .bind(("rel_type", fact.predicate.as_str().to_string()))
        .bind(("description", description))
        .bind(("confidence", evidence.mean()))
        .bind(("alpha", evidence.alpha()))
        .bind(("beta", evidence.beta()))
        .bind(("status", fact.status.clone()))
        .bind(("provenance", label.to_string()))
        .bind(("evidence", json!([item])))
        .await?
        .check()?;
    Ok(())
}

async fn reinforce_edge(
    gm: &GraphMemory,
    edge: &StoredEdge,
    fact: &Fact,
    session_id: &str,
) -> Result<(), GraphError> {
    let (provenance, label) = provenance_of(fact.provenance);
    let mut state = EdgeEvidence::new(
        Evidence::from_stored(edge.alpha, edge.beta, edge.confidence),
        edge.self_reinforcements.unwrap_or(0),
    );
    state.corroborate(provenance, gm.provenance_weights());
    let evidence = push_evidence(
        edge.evidence.clone(),
        evidence_item(session_id, fact, label),
    );
    let id = record_id(&edge.id);
    gm.db()
        .query(
            r#"UPDATE type::record($id) SET
                confidence = $confidence,
                alpha = $alpha,
                beta = $beta,
                self_reinforcements = $self_reinforcements,
                evidence = $evidence,
                last_reinforced = time::now()"#,
        )
        .bind(("id", id))
        .bind(("confidence", state.evidence().mean()))
        .bind(("alpha", state.evidence().alpha()))
        .bind(("beta", state.evidence().beta()))
        .bind(("self_reinforcements", state.self_reinforcements()))
        .bind(("evidence", evidence))
        .await?
        .check()?;
    Ok(())
}

async fn write_slot(
    gm: &GraphMemory,
    catalog: &mut Catalog,
    session_id: &str,
    date: &str,
    fact: &Fact,
    subject: &Stored,
    stats: &mut WriteStats,
) -> Result<(), GraphError> {
    let mut attributes = subject.attributes.clone();
    let v2 = v2_map(&mut attributes)?;
    let slots = slots_mut(v2)?;
    if let Some(slot) = slots.iter_mut().find(|slot| same_slot(slot, fact)) {
        reinforce_slot(slot, gm, fact, session_id)?;
        stats.reinforced += 1;
    } else {
        slots.push(new_slot(session_id, date, fact));
        stats.slots += 1;
    }
    gm.update_entity(
        &subject.id,
        EntityUpdate {
            attributes: Some(attributes.clone()),
            ..EntityUpdate::default()
        },
    )
    .await?;
    catalog.set_attributes(&subject.id, attributes);
    Ok(())
}

fn same_slot(slot: &Value, fact: &Fact) -> bool {
    let live = slot.get("valid_until").is_none_or(Value::is_null);
    live && slot_str(slot, "predicate") == Some(fact.predicate.as_str())
        && slot_str(slot, "key") == Some(fact.key.as_str())
        && slot_str(slot, "value") == Some(fact.object_name.as_str())
        && slot_str(slot, "status") == Some(fact.status.as_str())
}

fn reinforce_slot(
    slot: &mut Value,
    gm: &GraphMemory,
    fact: &Fact,
    session_id: &str,
) -> Result<(), GraphError> {
    let (provenance, label) = provenance_of(fact.provenance);
    let alpha = slot.get("alpha").and_then(Value::as_f64);
    let beta = slot.get("beta").and_then(Value::as_f64);
    let mean =
        alpha.unwrap_or(0.0) / (alpha.unwrap_or(0.0) + beta.unwrap_or(0.0)).max(f64::EPSILON);
    let mut state = EdgeEvidence::new(
        Evidence::from_stored(alpha, beta, mean),
        slot.get("self_reinforcements")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    state.corroborate(provenance, gm.provenance_weights());
    let evidence = push_evidence(
        slot.get("evidence").cloned(),
        evidence_item(session_id, fact, label),
    );
    let object = slot
        .as_object_mut()
        .ok_or_else(|| GraphError::Parse("slot is not an object".into()))?;
    object.insert("alpha".into(), json!(state.evidence().alpha()));
    object.insert("beta".into(), json!(state.evidence().beta()));
    object.insert(
        "self_reinforcements".into(),
        json!(state.self_reinforcements()),
    );
    object.insert("evidence".into(), evidence);
    Ok(())
}

fn new_slot(session_id: &str, date: &str, fact: &Fact) -> Value {
    let evidence = Evidence::from_prior(ExtractionContext::Explicit.prior());
    let (_, label) = provenance_of(fact.provenance);
    json!({
        "predicate": fact.predicate.as_str(),
        "key": fact.key,
        "value": fact.object_name,
        "status": fact.status,
        "provenance": label,
        "valid_from": date,
        "valid_until": Value::Null,
        "ended_by": Value::Null,
        "alpha": evidence.alpha(),
        "beta": evidence.beta(),
        "self_reinforcements": 0,
        "evidence": [evidence_item(session_id, fact, label)],
    })
}

fn provenance_of(role: TurnRole) -> (Provenance, &'static str) {
    match role {
        TurnRole::User => (Provenance::User, "user"),
        TurnRole::Assistant => (Provenance::SelfGenerated, "self"),
    }
}

fn evidence_item(session_id: &str, fact: &Fact, role: &str) -> Value {
    json!({
        "session": session_id,
        "turn": fact.turn,
        "quote": fact.quote,
        "role": role,
    })
}

fn push_evidence(existing: Option<Value>, item: Value) -> Value {
    let mut items = match existing {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    items.push(item);
    Value::Array(items)
}

fn slot_str<'a>(slot: &'a Value, field: &str) -> Option<&'a str> {
    slot.get(field).and_then(Value::as_str)
}

fn v2_map(attributes: &mut Value) -> Result<&mut serde_json::Map<String, Value>, GraphError> {
    if !attributes.is_object() {
        *attributes = json!({});
    }
    let root = attributes
        .as_object_mut()
        .ok_or_else(|| GraphError::Parse("entity attributes are not an object".into()))?;
    if !root.get("v2").is_some_and(Value::is_object) {
        root.insert("v2".into(), json!({"kind": "", "slots": []}));
    }
    root.get_mut("v2")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| GraphError::Parse("entity attributes.v2 is not an object".into()))
}

fn slots_mut(v2: &mut serde_json::Map<String, Value>) -> Result<&mut Vec<Value>, GraphError> {
    if !v2.get("slots").is_some_and(Value::is_array) {
        v2.insert("slots".into(), json!([]));
    }
    v2.get_mut("slots")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| GraphError::Parse("entity attributes.v2.slots is not an array".into()))
}

fn record_id(id: &Value) -> String {
    match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StoredEdge {
    id: Value,
    #[serde(rename = "out")]
    to_id: Value,
    rel_type: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    valid_until: Option<Value>,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    alpha: Option<f64>,
    #[serde(default)]
    beta: Option<f64>,
    #[serde(default)]
    self_reinforcements: Option<i64>,
    #[serde(default)]
    evidence: Option<Value>,
}

#[derive(Debug, Clone)]
struct Stored {
    id: String,
    name: String,
    kind: Kind,
    attributes: Value,
}

struct Catalog {
    by_name: HashMap<String, Vec<Stored>>,
}

impl Catalog {
    async fn load(gm: &GraphMemory) -> Result<Self, GraphError> {
        let entities = crud::list_entities(gm.db(), None).await?;
        let mut by_name: HashMap<String, Vec<Stored>> = HashMap::new();
        for entity in entities {
            if let Some(stored) = stored_from(&entity) {
                by_name
                    .entry(normalise(&stored.name))
                    .or_default()
                    .push(stored);
            }
        }
        Ok(Self { by_name })
    }

    async fn resolve(
        &mut self,
        gm: &GraphMemory,
        name: &str,
        kind: Kind,
        stats: &mut WriteStats,
    ) -> Result<Stored, GraphError> {
        let norm = normalise(name);
        if let Some(existing) = self.same(&norm, kind) {
            return Ok(existing);
        }
        if let Some(lower) = self.highest_lower(&norm, kind) {
            return self.upgrade(gm, &norm, lower, kind).await;
        }
        self.create(gm, name.trim(), kind, stats).await
    }

    fn same(&self, norm: &str, kind: Kind) -> Option<Stored> {
        self.by_name
            .get(norm)
            .and_then(|group| group.iter().find(|stored| stored.kind == kind))
            .cloned()
    }

    fn highest_lower(&self, norm: &str, kind: Kind) -> Option<Kind> {
        self.by_name.get(norm).and_then(|group| {
            group
                .iter()
                .filter(|stored| stored.kind.priority() < kind.priority())
                .max_by_key(|stored| stored.kind.priority())
                .map(|stored| stored.kind)
        })
    }

    async fn upgrade(
        &mut self,
        gm: &GraphMemory,
        norm: &str,
        from: Kind,
        to: Kind,
    ) -> Result<Stored, GraphError> {
        let current = self
            .same(norm, from)
            .ok_or_else(|| GraphError::NotFound(format!("no {from:?} entity to upgrade")))?;
        let mut attributes = current.attributes.clone();
        if let Ok(v2) = v2_map(&mut attributes) {
            v2.insert("kind".into(), json!(to.as_str()));
        }
        let mutable = to.entity_type().is_mutable();
        gm.db()
            .query(
                "UPDATE type::record($id) SET entity_type = $entity_type, mutable = $mutable, attributes = $attributes, updated_at = time::now()",
            )
            .bind(("id", current.id.clone()))
            .bind(("entity_type", to.entity_type().to_string()))
            .bind(("mutable", mutable))
            .bind(("attributes", attributes.clone()))
            .await?
            .check()?;
        let upgraded = Stored {
            id: current.id.clone(),
            name: current.name.clone(),
            kind: to,
            attributes,
        };
        if let Some(group) = self.by_name.get_mut(norm) {
            group.retain(|stored| stored.id != current.id);
            group.push(upgraded.clone());
        }
        Ok(upgraded)
    }

    async fn create(
        &mut self,
        gm: &GraphMemory,
        name: &str,
        kind: Kind,
        stats: &mut WriteStats,
    ) -> Result<Stored, GraphError> {
        let attributes = json!({
            "v2": { "kind": kind.as_str(), "slots": [] }
        });
        let entity = gm
            .add_entity(NewEntity {
                name: name.to_string(),
                entity_type: kind.entity_type(),
                abstract_text: format!("{} {name}", kind.as_str()),
                overview: None,
                content: None,
                attributes: Some(attributes.clone()),
                source: None,
            })
            .await?;
        stats.entities_created += 1;
        let stored = Stored {
            id: entity.id_string(),
            name: entity.name.clone(),
            kind,
            attributes,
        };
        self.by_name
            .entry(normalise(&stored.name))
            .or_default()
            .push(stored.clone());
        Ok(stored)
    }

    fn set_attributes(&mut self, id: &str, attributes: Value) {
        for group in self.by_name.values_mut() {
            for stored in group {
                if stored.id == id {
                    stored.attributes = attributes;
                    return;
                }
            }
        }
    }
}

fn stored_from(entity: &Entity) -> Option<Stored> {
    let kind = entity
        .attributes
        .as_ref()
        .and_then(|attrs| attrs.get("v2"))
        .and_then(|v2| v2.get("kind"))
        .and_then(Value::as_str)
        .and_then(Kind::parse)?;
    Some(Stored {
        id: entity.id_string(),
        name: entity.name.clone(),
        kind,
        attributes: entity.attributes.clone().unwrap_or_else(|| json!({})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::v2::schema::{remap, Fact, Kind, Mention, Predicate, Role};

    fn setting(value: &str) -> Fact {
        let mut mention = Mention {
            subject: "app".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Uses,
            role: Role::None,
            key: String::new(),
            object: value.into(),
            object_kind: None,
            status: "decided".into(),
            turn: 2,
            quote: format!("Port {value}"),
            confidence: 0.9,
            provenance: TurnRole::User,
        };
        // "9000" is a literal, so the table stores a setting rather than an entity.
        if value == "redis" {
            mention.predicate = Predicate::Setting;
            mention.key = "cache".into();
            mention.object_kind = None;
            mention.object = "redis".into();
            mention.quote = "cache is redis".into();
        }
        remap(mention).unwrap()
    }

    async fn open_store() -> (tempfile::TempDir, GraphMemory) {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        (tmp, gm)
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn literal_goes_to_slot() {
        let (_tmp, gm) = open_store().await;
        let fact = setting("9000");
        assert_eq!(fact.predicate, Predicate::Setting);
        write_facts(&gm, "s1", "2026-01-02", &[fact]).await.unwrap();

        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        assert!(
            entities
                .iter()
                .all(|entity| !looks_like_literal(&entity.name)),
            "{:?}",
            entities
                .iter()
                .map(|entity| &entity.name)
                .collect::<Vec<_>>()
        );
        assert!(entities.iter().all(|entity| entity.name != "9000"));
        let app = entities.iter().find(|entity| entity.name == "app").unwrap();
        let slots = app.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0]["predicate"], "setting");
        assert_eq!(slots[0]["value"], "9000");
        assert_eq!(slots[0]["key"], "value");
        assert!(crud::list_all_relationships(gm.db())
            .await
            .unwrap()
            .is_empty());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn identical_fact_reinforces() {
        let (_tmp, gm) = open_store().await;
        let slot = setting("redis");
        let edge = uses_edge();
        write_facts(&gm, "s1", "2026-01-02", &[slot.clone(), edge.clone()])
            .await
            .unwrap();
        write_facts(&gm, "s1", "2026-01-02", &[slot, edge])
            .await
            .unwrap();

        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        let app = entities.iter().find(|entity| entity.name == "app").unwrap();
        let slots = app.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0]["evidence"].as_array().unwrap().len(), 2);
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let alpha = slots[0]["alpha"].as_f64().unwrap();
        let beta = slots[0]["beta"].as_f64().unwrap();
        assert!((beta - prior.beta()).abs() < 1e-9, "beta moved: {beta}");
        assert!(alpha > prior.alpha(), "alpha did not grow: {alpha}");

        let edges = crud::list_all_relationships(gm.db()).await.unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].rel_type, "uses");
        assert_eq!(edges[0].description.as_deref(), Some("role=library"));
        let again = Evidence::from_stored(edges[0].alpha, edges[0].beta, edges[0].confidence);
        assert!((again.beta() - prior.beta()).abs() < 1e-9);
        assert!(again.alpha() > prior.alpha());
    }

    fn uses_edge() -> Fact {
        let mut mention = Mention {
            subject: "app".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Uses,
            role: Role::Library,
            key: "ignored".into(),
            object: "redis".into(),
            object_kind: Some(Kind::Technology),
            status: "decided".into(),
            turn: 1,
            quote: "app uses redis".into(),
            confidence: 0.9,
            provenance: TurnRole::User,
        };
        // "redis" is not a literal shape, so this stays an edge.
        let _ = mention.object_kind.take();
        mention.object_kind = Some(Kind::Technology);
        remap(mention).unwrap()
    }

    fn looks_like_literal(name: &str) -> bool {
        super::super::schema::looks_literal(name)
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn v1_stamped_store_refused() {
        let (_tmp, gm) = open_store().await;
        write_stamp(gm.db(), "v1").await.unwrap();
        let err = ensure_v2_store(&gm).await.unwrap_err();
        assert_eq!(err.to_string(), format!("extraction failed: {V2_REFUSAL}"));
        assert_eq!(read_stamp(gm.db()).await.unwrap().as_deref(), Some("v1"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn unstamped_store_with_entities_refused() {
        let (_tmp, gm) = open_store().await;
        gm.db()
            .query(
                "CREATE entity SET name = 'pine', entity_type = 'project', abstract = 'host', overview = ''",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        let err = ensure_v2_store(&gm).await.unwrap_err();
        assert!(err.to_string().contains(V2_REFUSAL), "{err}");
        assert!(read_stamp(gm.db()).await.unwrap().is_none());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn empty_store_stamped() {
        let (_tmp, gm) = open_store().await;
        ensure_v2_store(&gm).await.unwrap();
        assert_eq!(read_stamp(gm.db()).await.unwrap().as_deref(), Some("v2"));
        // A second pass does not refuse a store this pipeline already stamped.
        ensure_v2_store(&gm).await.unwrap();
    }
}
