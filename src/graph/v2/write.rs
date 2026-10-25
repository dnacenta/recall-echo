// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Writes for the second pipeline: resolved entities, edges, slots, and
//! reinforcement of an identical live fact.
//!
//! Nothing here retires or contradicts an edge. A repeated fact from another
//! sighting adds evidence and corroborates. The same session, turn and quote
//! is already stored, so a retry does not corroborate it. That keeps the prior
//! recovered from the edge's first β: the creating session never moves β.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::super::confidence::{EdgeEvidence, Evidence, ExtractionContext, Provenance};
use super::super::error::GraphError;
use super::super::llm::LlmProvider;
use super::super::store::Db;
use super::super::types::EntityUpdate;
use super::super::{deserialize_take, GraphMemory};
use super::input::{ProjectIdentity, TurnRole};
use super::resolve::{self, MentionDraft};
use super::schema::{normalise, Fact, Kind};
use surrealdb::Surreal;

/// Shown when v2 is asked to write into a store it did not build.
pub(crate) const V2_REFUSAL: &str = "this graph was built by pipeline v1; re-extract into a fresh store with: recall-echo graph extract --pipeline v2 --rebuild";

const STAMP_V2: &str = "v2";

/// What one batch of facts did to the store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WriteStats {
    pub entities_created: u32,
    pub edges: u32,
    pub slots: u32,
    pub reinforced: u32,
    /// The fact was already stored with this session, turn and quote.
    pub already_seen: u32,
    /// Resolver calls made while binding mentions. Zero or one.
    pub resolver_calls: u32,
    /// Facts dropped because a mention had no binding. The session still writes.
    pub unresolved: u32,
    /// Tier hits and rejected resolver proposals for this batch.
    pub resolve: resolve::ResolveReport,
}

/// What one session's write needs besides the facts.
pub(crate) struct WriteCtx<'a> {
    pub llm: &'a dyn LlmProvider,
    pub project: &'a ProjectIdentity,
    pub session_id: &'a str,
    pub date: &'a str,
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
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
) -> Result<WriteStats, GraphError> {
    let resolution = resolve::resolve_session(gm, ctx.llm, ctx.project, &drafts_of(facts)).await?;
    let mut stats = WriteStats {
        entities_created: resolution.entities_created,
        resolver_calls: resolution.report.resolver_called,
        resolve: resolution.report.clone(),
        ..WriteStats::default()
    };
    let mut catalog = Catalog::from_resolution(&resolution);
    for fact in facts {
        write_one(gm, &mut catalog, ctx.session_id, ctx.date, fact, &mut stats).await?;
    }
    Ok(stats)
}

fn drafts_of(facts: &[Fact]) -> Vec<MentionDraft> {
    let mut drafts = Vec::new();
    let mut creating = HashSet::new();
    for fact in facts {
        if push_endpoint(&mut drafts, &fact.subject_name, fact.subject_kind, fact) {
            creating.insert(normalise(&fact.subject_name));
        }
        if fact.predicate.is_edge() {
            let kind = fact.object_kind.unwrap_or(Kind::Technology);
            if push_endpoint(&mut drafts, &fact.object_name, kind, fact) {
                creating.insert(normalise(&fact.object_name));
            }
        } else if let Some(kind) = fact.object_kind {
            if push_endpoint(&mut drafts, &fact.object_name, kind, fact) {
                creating.insert(normalise(&fact.object_name));
            }
        }
    }
    for fact in facts {
        let folded = normalise(&fact.replaces);
        if folded.is_empty() || !creating.insert(folded) {
            continue;
        }
        drafts.push(mention_draft(
            fact.replaces.trim(),
            Kind::Technology,
            fact,
            false,
        ));
    }
    drafts
}

fn push_endpoint(drafts: &mut Vec<MentionDraft>, name: &str, kind: Kind, fact: &Fact) -> bool {
    if normalise(name).is_empty() {
        return false;
    }
    drafts.push(mention_draft(name.trim(), kind, fact, true));
    true
}

fn mention_draft(name: &str, kind: Kind, fact: &Fact, creates: bool) -> MentionDraft {
    MentionDraft {
        name: name.to_string(),
        kind,
        subject: fact.subject_name.clone(),
        predicate: fact.predicate.as_str().to_string(),
        object: fact.object_name.clone(),
        creates,
    }
}

async fn write_one(
    gm: &GraphMemory,
    catalog: &mut Catalog,
    session_id: &str,
    date: &str,
    fact: &Fact,
    stats: &mut WriteStats,
) -> Result<(), GraphError> {
    let Some(subject) = bound(catalog, &fact.subject_name, fact.subject_kind, stats)? else {
        return Ok(());
    };
    if fact.predicate.is_edge() {
        let object_kind = fact.object_kind.unwrap_or(Kind::Technology);
        let Some(object) = bound(catalog, &fact.object_name, object_kind, stats)? else {
            return Ok(());
        };
        write_edge(gm, session_id, fact, &subject, &object, stats).await
    } else {
        write_slot(gm, catalog, session_id, date, fact, &subject, stats).await
    }
}

/// A missing binding drops this fact. Any other error still fails the session.
fn bound(
    catalog: &Catalog,
    name: &str,
    kind: Kind,
    stats: &mut WriteStats,
) -> Result<Option<Stored>, GraphError> {
    match catalog.get(name, kind) {
        Ok(stored) => Ok(Some(stored)),
        Err(GraphError::NotFound(_)) => {
            stats.unresolved += 1;
            Ok(None)
        }
        Err(err) => Err(err),
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
        match reinforce_edge(gm, &existing, fact, session_id).await? {
            Sight::Recorded => stats.reinforced += 1,
            Sight::AlreadySeen => stats.already_seen += 1,
        }
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
) -> Result<Sight, GraphError> {
    if evidence_seen(edge.evidence.as_ref(), session_id, fact) {
        return Ok(Sight::AlreadySeen);
    }
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
    Ok(Sight::Recorded)
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
    let persist = if let Some(slot) = slots.iter_mut().find(|slot| same_slot(slot, fact)) {
        match reinforce_slot(slot, gm, fact, session_id)? {
            Sight::Recorded => {
                stats.reinforced += 1;
                true
            }
            Sight::AlreadySeen => {
                stats.already_seen += 1;
                false
            }
        }
    } else {
        slots.push(new_slot(session_id, date, fact));
        stats.slots += 1;
        true
    };
    if !persist {
        return Ok(());
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
) -> Result<Sight, GraphError> {
    if evidence_seen(slot.get("evidence"), session_id, fact) {
        return Ok(Sight::AlreadySeen);
    }
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
    Ok(Sight::Recorded)
}

/// Whether this sighting was new or was already on the fact.
enum Sight {
    Recorded,
    AlreadySeen,
}

/// True when this session, turn and quote is already an evidence item.
fn evidence_seen(existing: Option<&Value>, session_id: &str, fact: &Fact) -> bool {
    existing.and_then(Value::as_array).is_some_and(|items| {
        items
            .iter()
            .any(|item| same_evidence(item, session_id, fact))
    })
}

fn same_evidence(item: &Value, session_id: &str, fact: &Fact) -> bool {
    item.get("session").and_then(Value::as_str) == Some(session_id)
        && item.get("quote").and_then(Value::as_str) == Some(fact.quote.as_str())
        && json_usize(item.get("turn")) == Some(fact.turn)
}

fn json_usize(value: Option<&Value>) -> Option<usize> {
    let value = value?;
    value
        .as_u64()
        .and_then(|number| usize::try_from(number).ok())
        .or_else(|| {
            value
                .as_i64()
                .and_then(|number| usize::try_from(number).ok())
        })
        .or_else(|| {
            let number = value.as_f64()?;
            (number.is_finite() && number >= 0.0 && number.fract() == 0.0)
                .then_some(number as u64)
                .and_then(|number| usize::try_from(number).ok())
        })
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
    attributes: Value,
}

struct Catalog {
    by_id: HashMap<String, Stored>,
    binding: HashMap<(String, Kind), String>,
}

impl Catalog {
    fn from_resolution(resolution: &resolve::Resolution) -> Self {
        let mut by_id = HashMap::new();
        let mut binding = HashMap::new();
        for item in &resolution.bindings {
            binding.insert((item.norm.clone(), item.kind), item.entity.id.clone());
            by_id.insert(
                item.entity.id.clone(),
                Stored {
                    id: item.entity.id.clone(),
                    attributes: item.entity.attributes.clone(),
                },
            );
        }
        Self { by_id, binding }
    }

    fn get(&self, name: &str, kind: Kind) -> Result<Stored, GraphError> {
        let id = self
            .binding
            .get(&(normalise(name), kind))
            .ok_or_else(|| GraphError::NotFound(format!("no resolved entity for {name}")))?;
        self.by_id
            .get(id)
            .cloned()
            .ok_or_else(|| GraphError::NotFound(id.clone()))
    }

    fn set_attributes(&mut self, id: &str, attributes: Value) {
        if let Some(stored) = self.by_id.get_mut(id) {
            stored.attributes = attributes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::crud;
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
        write_session(&gm, "s1", "2026-01-02", &[fact])
            .await
            .unwrap();

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
        write_session(&gm, "s1", "2026-01-02", &[slot.clone(), edge.clone()])
            .await
            .unwrap();
        // A later session is a new sighting. The same session is covered by
        // `retry_of_the_creating_session_does_not_corroborate`.
        let again = write_session(&gm, "s2", "2026-01-03", &[slot, edge])
            .await
            .unwrap();
        assert_eq!(again.reinforced, 2);
        assert_eq!(again.already_seen, 0);

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

    /// A retry writes the creating session again. The stored evidence already
    /// holds that session, turn and quote, so neither the slot nor the edge moves.
    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn retry_of_the_creating_session_does_not_corroborate() {
        let (_tmp, gm) = open_store().await;
        let facts = [setting("redis"), uses_edge()];
        write_session(&gm, "s1", "2026-01-02", &facts)
            .await
            .unwrap();
        let before = stored_belief(&gm).await;

        let retry = write_session(&gm, "s1", "2026-01-02", &facts)
            .await
            .unwrap();
        assert_eq!(retry.already_seen, 2);
        assert_eq!(retry.reinforced, 0);
        assert_eq!(retry.slots, 0);
        assert_eq!(retry.edges, 0);

        let after = stored_belief(&gm).await;
        assert_eq!(after, before);
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        assert_eq!(after.slot_evidence, 1);
        assert_eq!(after.edge_evidence, 1);
        assert_eq!(after.slot_self, 0);
        assert_eq!(after.edge_self, 0);
        assert!((after.slot_alpha - prior.alpha()).abs() < 1e-9);
        assert!((after.slot_beta - prior.beta()).abs() < 1e-9);
        assert!((after.edge_alpha - prior.alpha()).abs() < 1e-9);
        assert!((after.edge_beta - prior.beta()).abs() < 1e-9);
    }

    #[cfg(feature = "embedded")]
    async fn stored_belief(gm: &GraphMemory) -> Belief {
        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        let app = entities.iter().find(|entity| entity.name == "app").unwrap();
        let slot = &app.attributes.as_ref().unwrap()["v2"]["slots"][0];
        let mut response = gm
            .db()
            .query("SELECT alpha, beta, self_reinforcements, evidence FROM relates_to")
            .await
            .unwrap();
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        let edge = rows.first().expect("edge");
        Belief {
            slot_alpha: slot["alpha"].as_f64().unwrap(),
            slot_beta: slot["beta"].as_f64().unwrap(),
            slot_self: slot["self_reinforcements"].as_i64().unwrap(),
            slot_evidence: slot["evidence"].as_array().unwrap().len(),
            edge_alpha: edge["alpha"].as_f64().unwrap(),
            edge_beta: edge["beta"].as_f64().unwrap(),
            edge_self: edge["self_reinforcements"].as_i64().unwrap(),
            edge_evidence: edge["evidence"].as_array().unwrap().len(),
        }
    }

    #[cfg(feature = "embedded")]
    #[derive(Debug, PartialEq)]
    struct Belief {
        slot_alpha: f64,
        slot_beta: f64,
        slot_self: i64,
        slot_evidence: usize,
        edge_alpha: f64,
        edge_beta: f64,
        edge_self: i64,
        edge_evidence: usize,
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

    /// These writes hit a fresh store or an exact name, so the resolver is never asked.
    struct Quiet;

    #[async_trait::async_trait]
    impl LlmProvider for Quiet {
        async fn complete(
            &self,
            _system_prompt: &str,
            _user_message: &str,
            _max_tokens: u32,
        ) -> Result<String, GraphError> {
            panic!("resolver called");
        }
    }

    async fn write_session(
        gm: &GraphMemory,
        session_id: &str,
        date: &str,
        facts: &[Fact],
    ) -> Result<WriteStats, GraphError> {
        let project = ProjectIdentity {
            name: String::new(),
            aliases: Vec::new(),
        };
        let llm = Quiet;
        let ctx = WriteCtx {
            llm: &llm,
            project: &project,
            session_id,
            date,
        };
        write_facts(gm, &ctx, facts).await
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

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn literal_object_edges_become_technology() {
        let (_tmp, gm) = open_store().await;
        let facts = [
            raw_edge(Predicate::PartOf, "api", Kind::Component, "linkshelf"),
            raw_edge(Predicate::DependsOn, "worker", Kind::Component, "queue"),
            raw_edge(Predicate::WorksOn, "ada", Kind::Person, "release-bot"),
        ];
        let stats = write_session(&gm, "s1", "2026-01-02", &facts)
            .await
            .unwrap();
        assert_eq!(stats.edges, 3);
        assert_eq!(stats.unresolved, 0);
        for name in ["linkshelf", "queue", "release-bot"] {
            let entity = crud::list_entities(gm.db(), None)
                .await
                .unwrap()
                .into_iter()
                .find(|entity| entity.name == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            let kind = entity
                .attributes
                .as_ref()
                .unwrap()
                .pointer("/v2/kind")
                .and_then(serde_json::Value::as_str);
            assert_eq!(kind, Some("technology"), "{name}");
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_missing_binding_drops_one_fact() {
        let (_tmp, gm) = open_store().await;
        let mut blank = setting("9000");
        blank.subject_name = "   ".into();
        blank.quote = "blank subject".into();
        let stats = write_session(&gm, "s1", "2026-01-02", &[blank, setting("redis")])
            .await
            .unwrap();
        assert_eq!(stats.unresolved, 1);
        assert_eq!(stats.slots, 1);
        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        assert!(entities.iter().any(|entity| entity.name == "app"));
    }

    fn raw_edge(predicate: Predicate, subject: &str, subject_kind: Kind, object: &str) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind,
            predicate,
            role: None,
            key: String::new(),
            object_name: object.into(),
            object_kind: None,
            status: "decided".into(),
            turn: 1,
            quote: format!("{subject} links {object}"),
            confidence: 0.9,
            provenance: TurnRole::User,
            replaces: String::new(),
        }
    }
}
