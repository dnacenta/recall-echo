// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Writes for the second pipeline: resolved entities, edges, slots, and
//! reinforcement of an identical live fact.
//!
//! A repeated fact from another sighting adds evidence and corroborates.
//! The same session, turn and quote is already stored, so a retry does not
//! corroborate it and does not move a retirement. That keeps the prior
//! recovered from the record's first β: the creating session never moves β.
//!
//! Single-valued slots, an explicit rejection, and an explicit replacement
//! close the live record they succeed. The close is stamped with the session
//! date and a reason. A record that is already closed keeps its first stamp.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use serde_json::{json, Value};
use surrealdb::method::Transaction;
use surrealdb::Surreal;

use super::super::confidence::{EdgeEvidence, Evidence, ExtractionContext, Provenance};
use super::super::crud;
use super::super::error::GraphError;
use super::super::llm::LlmProvider;
use super::super::store::Db;
use super::super::types::Retirement;
use super::super::{deserialize_take, GraphMemory};
use super::input::{ProjectIdentity, Turn};
use super::resolve::{self, MentionDraft};
use super::schema::{normalise, Fact, Kind, Predicate};

/// Production writes never fail a commit on purpose.
pub(crate) static NO_CONFLICTS: AtomicU32 = AtomicU32::new(0);

const SUPERSEDED: &str = "superseded";
const REJECTED: &str = "rejected";
const REPLACED: &str = "replaced";
const PROMOTED: &str = "promoted";

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
    /// Facts this batch closed, in the order it closed them.
    pub retirements: Vec<Retirement>,
    /// A retirement whose target was missing or whose slot was not an object.
    pub retirements_skipped: u32,
    /// Milliseconds spent resolving mentions, measured around that call.
    pub resolve_ms: u64,
    /// Milliseconds spent committing edges, slots and retirements.
    pub write_ms: u64,
}

/// Runs after a conflict rolls the attempt back, before the next attempt.
///
/// Production writes leave this empty. The conflict test uses it to commit
/// a slot on the main connection between the two attempts.
pub(crate) trait ConflictHook: Send + Sync {
    fn between_attempts<'a>(
        &'a self,
        gm: &'a GraphMemory,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), GraphError>> + Send + 'a>>;
}

/// What one session's write needs besides the facts.
pub(crate) struct WriteCtx<'a> {
    pub llm: &'a dyn LlmProvider,
    pub project: &'a ProjectIdentity,
    pub session_id: &'a str,
    pub date: &'a str,
    /// Turns of the session, so `replaces` can be checked against the cited one.
    pub turns: &'a [Turn],
    /// Decremented when the next commit should fail as a conflict. Stays zero
    /// outside the conflict test: a real conflict still retries once.
    pub conflict_budget: &'a AtomicU32,
    /// Midnight UTC of `date`, when `date` is a calendar day. None skips
    /// every retirement and still writes the facts.
    pub valid_until: Option<String>,
    pub on_conflict: Option<&'a dyn ConflictHook>,
}

impl<'a> WriteCtx<'a> {
    pub(crate) fn new(
        llm: &'a dyn LlmProvider,
        project: &'a ProjectIdentity,
        session_id: &'a str,
        date: &'a str,
        turns: &'a [Turn],
    ) -> Self {
        Self {
            llm,
            project,
            session_id,
            date,
            turns,
            conflict_budget: &NO_CONFLICTS,
            valid_until: session_midnight(date),
            on_conflict: None,
        }
    }
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
///
/// Resolution commits on its own, before this function's transaction: an
/// alias or a new entity is already durable, and a retry binds the same
/// rows. The session's edges, slots and retirements then commit together.
/// A conflict rolls that transaction back and the write runs once more.
pub(crate) async fn write_facts(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
) -> Result<WriteStats, GraphError> {
    let resolve_started = Instant::now();
    let resolution = resolve::resolve_session(gm, ctx.llm, ctx.project, &drafts_of(facts)).await?;
    let resolve_ms = elapsed_ms(resolve_started);
    let write_started = Instant::now();
    let mut attempt = 0u8;
    loop {
        match write_attempt(gm, ctx, facts, &resolution).await {
            Ok(mut stats) => {
                stats.resolve_ms = resolve_ms;
                stats.write_ms = elapsed_ms(write_started);
                return Ok(stats);
            }
            Err(err) if attempt == 0 && is_write_conflict(&err) => {
                attempt += 1;
                if let Some(hook) = ctx.on_conflict {
                    hook.between_attempts(gm).await?;
                }
            }
            Err(err) => return Err(err),
        }
    }
}

async fn write_attempt(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
    resolution: &resolve::Resolution,
) -> Result<WriteStats, GraphError> {
    let tx = gm.db().clone().begin().await?;
    let mut stats = WriteStats {
        entities_created: resolution.entities_created,
        resolver_calls: resolution.report.resolver_called,
        resolve: resolution.report.clone(),
        ..WriteStats::default()
    };
    let mut catalog = Catalog::from_resolution(resolution);
    // The catalog above was read before this transaction. Re-read inside it
    // so a retry sees slots another writer committed after the first attempt.
    if let Err(err) = refresh_catalog(&tx, &mut catalog).await {
        let _ = tx.cancel().await;
        return Err(err);
    }
    let applied = apply_all(gm, &tx, ctx, &mut catalog, &mut stats, facts).await;
    if let Err(err) = applied {
        let _ = tx.cancel().await;
        return Err(err);
    }
    if take_conflict(ctx.conflict_budget) {
        tx.cancel().await?;
        return Err(conflict_error());
    }
    tx.commit().await?;
    Ok(stats)
}

async fn apply_all(
    gm: &GraphMemory,
    tx: &Transaction<Db>,
    ctx: &WriteCtx<'_>,
    catalog: &mut Catalog,
    stats: &mut WriteStats,
    facts: &[Fact],
) -> Result<(), GraphError> {
    let mut attempt = Attempt {
        gm,
        tx,
        ctx,
        catalog,
        stats,
        names: HashMap::new(),
        edges: HashMap::new(),
        scope: HashMap::new(),
    };
    for index in ordered_indexes(facts) {
        attempt.apply(&facts[index]).await?;
    }
    Ok(())
}

fn ordered_indexes(facts: &[Fact]) -> Vec<usize> {
    let mut indexes: Vec<usize> = (0..facts.len()).collect();
    indexes.sort_by(|&left, &right| {
        facts[left]
            .turn
            .cmp(&facts[right].turn)
            .then_with(|| status_rank(&facts[left].status).cmp(&status_rank(&facts[right].status)))
            .then(left.cmp(&right))
    });
    indexes
}

/// Rejected facts run before a decided fact of the same turn, so a rejection
/// closes the previous value and the decided fact is what gets stored.
fn status_rank(status: &str) -> u8 {
    match status {
        "rejected" => 0,
        "considered" => 1,
        "decided" => 2,
        _ => 3,
    }
}

fn take_conflict(budget: &AtomicU32) -> bool {
    let mut current = budget.load(Ordering::SeqCst);
    loop {
        if current == 0 {
            return false;
        }
        match budget.compare_exchange(current, current - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn conflict_error() -> GraphError {
    GraphError::Db(surrealdb::Error::thrown(
        "transaction conflict: please retry".to_string(),
    ))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn is_write_conflict(err: &GraphError) -> bool {
    match err {
        GraphError::Db(inner) => {
            let message = inner.to_string().to_lowercase();
            message.contains("conflict") || message.contains("please retry")
        }
        _ => false,
    }
}

struct Attempt<'a> {
    gm: &'a GraphMemory,
    tx: &'a Transaction<Db>,
    ctx: &'a WriteCtx<'a>,
    catalog: &'a mut Catalog,
    stats: &'a mut WriteStats,
    names: HashMap<String, String>,
    /// Live outgoing edges of one subject, for this attempt.
    edges: HashMap<String, Vec<StoredEdge>>,
    /// Component ids in a project's scope, for this attempt.
    scope: HashMap<String, Vec<String>>,
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

impl<'a> Attempt<'a> {
    async fn apply(&mut self, source: &Fact) -> Result<(), GraphError> {
        let Some(subject) = bound(
            self.catalog,
            &source.subject_name,
            source.subject_kind,
            self.stats,
        )?
        else {
            return Ok(());
        };
        self.names
            .insert(subject.id.clone(), source.subject_name.clone());
        let mut fact = source.clone();
        fact.key = fold_key(&fact.key);
        if fact.status == "rejected" {
            self.retire_rejected(&fact, &subject).await?;
        } else if fact.status == "decided" {
            if replaces_supported(&fact, self.ctx.turns) {
                self.retire_replaced(&fact, &subject).await?;
            }
            if single_valued(fact.predicate) {
                self.supersede_slot(&mut fact, &subject).await?;
            }
            self.promote(&fact, &subject).await?;
        }
        let Some(subject) = self.catalog.by_id.get(&subject.id).cloned() else {
            return Ok(());
        };
        if fact.predicate.is_edge() {
            let object_kind = fact.object_kind.unwrap_or(Kind::Technology);
            let Some(object) = bound(self.catalog, &fact.object_name, object_kind, self.stats)?
            else {
                return Ok(());
            };
            self.names
                .insert(object.id.clone(), fact.object_name.clone());
            self.write_edge(&fact, &subject, &object).await
        } else {
            self.write_slot(&fact, &subject).await
        }
    }

    async fn write_edge(
        &mut self,
        fact: &Fact,
        subject: &Stored,
        object: &Stored,
    ) -> Result<(), GraphError> {
        let description = fact.role.map(|role| format!("role={}", role.as_str()));
        if let Some(existing) = self
            .find_edge(subject, object, fact, description.as_deref())
            .await?
        {
            match self.reinforce_edge(&subject.id, &existing, fact).await? {
                Sight::Recorded => self.stats.reinforced += 1,
                Sight::AlreadySeen => self.stats.already_seen += 1,
            }
            return Ok(());
        }
        self.create_edge(fact, subject, object, description).await?;
        self.stats.edges += 1;
        Ok(())
    }

    async fn find_edge(
        &mut self,
        subject: &Stored,
        object: &Stored,
        fact: &Fact,
        description: Option<&str>,
    ) -> Result<Option<StoredEdge>, GraphError> {
        let edges = self.live_edges(&subject.id).await?;
        Ok(edges.into_iter().find(|edge| {
            edge.rel_type == fact.predicate.as_str()
                && edge.status.as_deref() == Some(fact.status.as_str())
                && edge.description.as_deref() == description
                && record_id(&edge.to_id) == object.id
                && !edge_closed(edge.valid_until.as_ref())
        }))
    }

    async fn create_edge(
        &mut self,
        fact: &Fact,
        subject: &Stored,
        object: &Stored,
        description: Option<String>,
    ) -> Result<(), GraphError> {
        let (provenance, label) = provenance_of(fact.provenance);
        let evidence = prior_evidence(provenance);
        let item = evidence_item(self.ctx.session_id, fact, label);
        self.tx
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
        self.forget_edges(&subject.id, &object.id);
        Ok(())
    }

    async fn reinforce_edge(
        &mut self,
        subject_id: &str,
        edge: &StoredEdge,
        fact: &Fact,
    ) -> Result<Sight, GraphError> {
        if evidence_seen(edge.evidence.as_ref(), self.ctx.session_id, fact) {
            return Ok(Sight::AlreadySeen);
        }
        let (provenance, label) = provenance_of(fact.provenance);
        let mut state = EdgeEvidence::new(
            Evidence::from_stored(edge.alpha, edge.beta, edge.confidence),
            edge.self_reinforcements.unwrap_or(0),
        );
        state.corroborate(provenance, self.gm.provenance_weights());
        let evidence = push_evidence(
            edge.evidence.clone(),
            evidence_item(self.ctx.session_id, fact, label),
        );
        let id = record_id(&edge.id);
        self.tx
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
        self.forget_edges(subject_id, &record_id(&edge.to_id));
        Ok(Sight::Recorded)
    }

    async fn write_slot(&mut self, fact: &Fact, subject: &Stored) -> Result<(), GraphError> {
        let mut attributes = subject.attributes.clone();
        let v2 = v2_map(&mut attributes)?;
        let slots = slots_mut(v2)?;
        let persist = if let Some(slot) = slots.iter_mut().find(|slot| same_slot(slot, fact)) {
            match reinforce_slot(slot, self.gm, fact, self.ctx.session_id)? {
                Sight::Recorded => {
                    self.stats.reinforced += 1;
                    true
                }
                Sight::AlreadySeen => {
                    self.stats.already_seen += 1;
                    false
                }
            }
        } else {
            slots.push(new_slot(self.ctx.session_id, self.ctx.date, fact));
            self.stats.slots += 1;
            true
        };
        if !persist {
            return Ok(());
        }
        self.persist(&subject.id, attributes).await
    }

    async fn persist(&mut self, id: &str, attributes: Value) -> Result<(), GraphError> {
        self.tx
            .query(
                "UPDATE type::record($id) SET attributes = $attributes, updated_at = time::now()",
            )
            .bind(("id", id.to_string()))
            .bind(("attributes", attributes.clone()))
            .await?
            .check()?;
        self.catalog.set_attributes(id, attributes);
        Ok(())
    }

    async fn supersede_slot(
        &mut self,
        fact: &mut Fact,
        subject: &Stored,
    ) -> Result<(), GraphError> {
        let Some((stored, mut slots)) = self.load_slots(&subject.id).await? else {
            return Ok(());
        };
        if fact.predicate == Predicate::Setting {
            fact.key = preferred_key(&fact.key, &slots);
        }
        let Some(until) = self.ctx.valid_until.clone() else {
            let session_id = self.ctx.session_id;
            self.skip_matches(&slots, |slot| {
                supersede_target(slot, fact) && !opened_here(slot.get("evidence"), session_id)
            });
            return Ok(());
        };
        let ended_by = format!("{}:{SUPERSEDED}", self.ctx.session_id);
        let mut changed = false;
        for slot in &mut slots {
            if !slot.is_object() {
                self.stats.retirements_skipped += 1;
                continue;
            }
            if !supersede_target(slot, fact)
                || opened_here(slot.get("evidence"), self.ctx.session_id)
            {
                continue;
            }
            let closed = closed_of(slot, fact);
            if !stamp_slot(slot, &until, &ended_by) {
                continue;
            }
            changed = true;
            let subject_name = self.label(&stored.id, &fact.subject_name).await?;
            self.record(SUPERSEDED, &subject_name, closed, &fact.object_name);
        }
        if changed {
            self.store_slots(&stored.id, &stored.attributes, slots)
                .await?;
        }
        Ok(())
    }

    async fn promote(&mut self, fact: &Fact, subject: &Stored) -> Result<(), GraphError> {
        if fact.predicate.is_edge() {
            self.promote_edge(fact, subject).await
        } else {
            self.promote_slot(fact, subject).await
        }
    }

    async fn promote_slot(&mut self, fact: &Fact, subject: &Stored) -> Result<(), GraphError> {
        let Some((stored, mut slots)) = self.load_slots(&subject.id).await? else {
            return Ok(());
        };
        let Some(until) = self.ctx.valid_until.clone() else {
            let session_id = self.ctx.session_id;
            self.skip_matches(&slots, |slot| {
                considered_same_slot(slot, fact) && !opened_here(slot.get("evidence"), session_id)
            });
            return Ok(());
        };
        let ended_by = format!("{}:{PROMOTED}", self.ctx.session_id);
        let mut changed = false;
        for slot in &mut slots {
            if !slot.is_object() {
                self.stats.retirements_skipped += 1;
                continue;
            }
            if !considered_same_slot(slot, fact)
                || opened_here(slot.get("evidence"), self.ctx.session_id)
            {
                continue;
            }
            let closed = closed_of(slot, fact);
            if !stamp_slot(slot, &until, &ended_by) {
                continue;
            }
            changed = true;
            let subject_name = self.label(&stored.id, &fact.subject_name).await?;
            self.record(PROMOTED, &subject_name, closed, &fact.object_name);
        }
        if changed {
            self.store_slots(&stored.id, &stored.attributes, slots)
                .await?;
        }
        Ok(())
    }

    async fn promote_edge(&mut self, fact: &Fact, subject: &Stored) -> Result<(), GraphError> {
        let object_kind = fact.object_kind.unwrap_or(Kind::Technology);
        let Ok(object) = self.catalog.get(&fact.object_name, object_kind) else {
            return Ok(());
        };
        let description = fact.role.map(|role| format!("role={}", role.as_str()));
        let edges = self.live_edges(&subject.id).await?;
        for edge in edges {
            if edge.rel_type != fact.predicate.as_str()
                || edge.status.as_deref() != Some("considered")
                || edge.description.as_deref() != description.as_deref()
                || record_id(&edge.to_id) != object.id
                || opened_here(edge.evidence.as_ref(), self.ctx.session_id)
            {
                continue;
            }
            self.close_edge(&subject.id, &edge, PROMOTED, &fact.object_name)
                .await?;
        }
        Ok(())
    }

    async fn retire_rejected(&mut self, fact: &Fact, subject: &Stored) -> Result<(), GraphError> {
        let scope = self.project_scope(subject, fact.subject_kind).await?;
        let object_id = self.bound_object_id(fact);
        for id in scope {
            self.rejected_on(&id, fact, object_id.as_deref()).await?;
        }
        Ok(())
    }

    async fn rejected_on(
        &mut self,
        id: &str,
        fact: &Fact,
        object_id: Option<&str>,
    ) -> Result<(), GraphError> {
        if !fact.predicate.is_edge() {
            // A rejected setting closes that key only. The same value on
            // another key stays live.
            self.close_slots(id, fact, REJECTED, "", |slot| {
                open_status(slot, &["decided", "considered"])
                    && slot_str(slot, "predicate") == Some(fact.predicate.as_str())
                    && values_equal(slot_str(slot, "value").unwrap_or(""), &fact.object_name)
                    && rejected_key_ok(slot, fact)
            })
            .await?;
        }
        let Some(object_id) = object_id else {
            return Ok(());
        };
        let edges = self.live_edges(id).await?;
        for edge in edges {
            if edge.rel_type != fact.predicate.as_str()
                || !status_is(&edge, &["decided", "considered"])
                || record_id(&edge.to_id) != object_id
                || opened_here(edge.evidence.as_ref(), self.ctx.session_id)
            {
                continue;
            }
            self.close_edge(id, &edge, REJECTED, "").await?;
        }
        Ok(())
    }

    async fn retire_replaced(&mut self, fact: &Fact, subject: &Stored) -> Result<(), GraphError> {
        let scope = self.project_scope(subject, fact.subject_kind).await?;
        let entity_id = self.replacement_entity(&fact.replaces).await?;
        let object_id = self.bound_object_id(fact);
        for id in scope {
            self.replaced_on(&id, fact, entity_id.as_deref(), object_id.as_deref())
                .await?;
        }
        Ok(())
    }

    async fn replaced_on(
        &mut self,
        id: &str,
        fact: &Fact,
        entity_id: Option<&str>,
        object_id: Option<&str>,
    ) -> Result<(), GraphError> {
        let replaces = fact.replaces.clone();
        let new_value = fact.object_name.clone();
        // A literal replacement closes settings and the single-valued issue
        // and release-status slots. A rule, a preference, or release contents
        // stays live even when its text contains the needle.
        self.close_slots(id, fact, REPLACED, &new_value, |slot| {
            open_status(slot, &["decided", "considered"])
                && literal_slot_ok(slot)
                && literal_matches(slot_str(slot, "value").unwrap_or(""), &replaces)
                && !same_decided_slot(slot, fact)
        })
        .await?;
        let Some(entity_id) = entity_id else {
            return Ok(());
        };
        let edges = self.live_edges(id).await?;
        for edge in edges {
            if edge.rel_type != fact.predicate.as_str()
                || !status_is(&edge, &["decided", "considered"])
                || record_id(&edge.to_id) != entity_id
                || opened_here(edge.evidence.as_ref(), self.ctx.session_id)
                || object_id.is_some_and(|object| same_decided_edge(&edge, fact, object))
            {
                continue;
            }
            self.close_edge(id, &edge, REPLACED, &new_value).await?;
        }
        Ok(())
    }

    async fn close_slots(
        &mut self,
        id: &str,
        fact: &Fact,
        reason: &str,
        new_value: &str,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<(), GraphError> {
        let Some((stored, mut slots)) = self.load_slots(id).await? else {
            return Ok(());
        };
        let session_id = self.ctx.session_id;
        let Some(until) = self.ctx.valid_until.clone() else {
            self.skip_matches(&slots, |slot| {
                matches(slot) && !opened_here(slot.get("evidence"), session_id)
            });
            return Ok(());
        };
        let ended_by = format!("{}:{reason}", self.ctx.session_id);
        let mut changed = false;
        let mut notes = Vec::new();
        for slot in &mut slots {
            if !slot.is_object() {
                self.stats.retirements_skipped += 1;
                continue;
            }
            if !matches(slot) || opened_here(slot.get("evidence"), self.ctx.session_id) {
                continue;
            }
            let closed = closed_of(slot, fact);
            if !stamp_slot(slot, &until, &ended_by) {
                continue;
            }
            changed = true;
            notes.push(closed);
        }
        if changed {
            let subject_name = self.label(&stored.id, &fact.subject_name).await?;
            self.store_slots(&stored.id, &stored.attributes, slots)
                .await?;
            for closed in notes {
                self.record(reason, &subject_name, closed, new_value);
            }
        }
        Ok(())
    }

    async fn close_edge(
        &mut self,
        subject_id: &str,
        edge: &StoredEdge,
        reason: &str,
        new_value: &str,
    ) -> Result<(), GraphError> {
        let id = record_id(&edge.id);
        if id.is_empty() {
            self.stats.retirements_skipped += 1;
            return Ok(());
        }
        if edge_closed(edge.valid_until.as_ref()) {
            return Ok(());
        }
        let mut response = self
            .tx
            .query("SELECT valid_until FROM type::record($id)")
            .bind(("id", id.clone()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let Some(row) = rows.into_iter().next() else {
            self.stats.retirements_skipped += 1;
            return Ok(());
        };
        if crud::edge_already_closed(row.get("valid_until")) {
            return Ok(());
        }
        let Some(until) = self.ctx.valid_until.clone() else {
            self.stats.retirements_skipped += 1;
            return Ok(());
        };
        let ended_by = format!("{}:{reason}", self.ctx.session_id);
        let mut response = self
            .tx
            .query(CLOSE_EDGE)
            .bind(("id", id))
            .bind(("valid_until", until))
            .bind(("ended_by", ended_by))
            .await?;
        let closed: Vec<Value> = deserialize_take(&mut response, 0)?;
        if closed.is_empty() {
            return Ok(());
        }
        let object_id = record_id(&edge.to_id);
        let old_value = self.label(&object_id, &object_id).await?;
        let subject_name = self.label(subject_id, subject_id).await?;
        self.record(
            reason,
            &subject_name,
            Closed {
                predicate: edge.rel_type.clone(),
                key: String::new(),
                old_value,
            },
            new_value,
        );
        self.forget_edges(subject_id, &object_id);
        Ok(())
    }

    fn record(&mut self, reason: &str, subject: &str, closed: Closed, new_value: &str) {
        self.stats.retirements.push(Retirement {
            reason: reason.to_string(),
            subject: subject.to_string(),
            predicate: closed.predicate,
            key: closed.key,
            old_value: closed.old_value,
            new_value: new_value.to_string(),
        });
    }

    /// A retirement that would have been stamped, counted and left in place.
    fn skip_matches(&mut self, slots: &[Value], matches: impl Fn(&Value) -> bool) {
        for slot in slots {
            if !slot.is_object() {
                self.stats.retirements_skipped += 1;
                continue;
            }
            if matches(slot) {
                self.stats.retirements_skipped += 1;
            }
        }
    }

    fn forget_edges(&mut self, subject_id: &str, object_id: &str) {
        self.edges.remove(subject_id);
        self.scope.remove(object_id);
    }

    async fn project_scope(
        &mut self,
        subject: &Stored,
        kind: Kind,
    ) -> Result<Vec<String>, GraphError> {
        if let Some(ids) = self.scope.get(&subject.id) {
            return Ok(ids.clone());
        }
        let mut ids = vec![subject.id.clone()];
        if kind != Kind::Project {
            self.scope.insert(subject.id.clone(), ids.clone());
            return Ok(ids);
        }
        let mut response = self
            .tx
            .query(SCOPE_PART_OF)
            .bind(("id", subject.id.clone()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let mut incoming = Vec::new();
        for row in rows {
            let Some(link) = row.get("in") else {
                continue;
            };
            incoming.push(record_id(link));
        }
        for id in incoming {
            if self.is_component(&id).await? {
                ids.push(id);
            }
        }
        self.scope.insert(subject.id.clone(), ids.clone());
        Ok(ids)
    }

    async fn is_component(&mut self, id: &str) -> Result<bool, GraphError> {
        let Some(stored) = self.ensure_entity(id).await? else {
            return Ok(false);
        };
        Ok(stored_kind(&stored.attributes) == Some(Kind::Component))
    }

    fn bound_object_id(&mut self, fact: &Fact) -> Option<String> {
        if !fact.predicate.is_edge() {
            return None;
        }
        let kind = fact.object_kind.unwrap_or(Kind::Technology);
        let stored = self.catalog.get(&fact.object_name, kind).ok()?;
        self.names
            .insert(stored.id.clone(), fact.object_name.clone());
        Some(stored.id)
    }

    async fn replacement_entity(&mut self, replaces: &str) -> Result<Option<String>, GraphError> {
        if let Ok(stored) = self.catalog.get(replaces, Kind::Technology) {
            return Ok(Some(stored.id));
        }
        let folded = normalise(replaces);
        if folded.is_empty() {
            return Ok(None);
        }
        if let Some(id) = crud::get_alias(self.gm.db(), &folded).await? {
            return Ok(Some(id));
        }
        let mut response = self
            .tx
            .query("SELECT id, attributes FROM entity WHERE name = $name")
            .bind(("name", replaces.trim().to_string()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let mut found = Vec::new();
        for row in rows {
            let Some(id) = row.get("id") else {
                continue;
            };
            let kind = stored_kind(row.get("attributes").unwrap_or(&Value::Null));
            found.push((record_id(id), kind));
        }
        if let Some((id, _)) = found
            .iter()
            .find(|(_, kind)| *kind == Some(Kind::Technology))
        {
            return Ok(Some(id.clone()));
        }
        if found.len() == 1 {
            return Ok(Some(found[0].0.clone()));
        }
        Ok(None)
    }

    async fn load_slots(&mut self, id: &str) -> Result<Option<(Stored, Vec<Value>)>, GraphError> {
        let Some(stored) = self.ensure_entity(id).await? else {
            self.stats.retirements_skipped += 1;
            return Ok(None);
        };
        match slot_list(&stored.attributes) {
            Ok(slots) => Ok(Some((stored, slots))),
            Err(()) => {
                self.stats.retirements_skipped += 1;
                Ok(None)
            }
        }
    }

    async fn store_slots(
        &mut self,
        id: &str,
        attributes: &Value,
        slots: Vec<Value>,
    ) -> Result<(), GraphError> {
        let mut attributes = attributes.clone();
        let v2 = v2_map(&mut attributes)?;
        v2.insert("slots".to_string(), Value::Array(slots));
        self.persist(id, attributes).await
    }

    async fn ensure_entity(&mut self, id: &str) -> Result<Option<Stored>, GraphError> {
        if let Some(stored) = self.catalog.by_id.get(id) {
            return Ok(Some(stored.clone()));
        }
        let mut response = self
            .tx
            .query("SELECT name, attributes FROM type::record($id)")
            .bind(("id", id.to_string()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        if let Some(name) = row.get("name").and_then(Value::as_str) {
            if !name.is_empty() {
                self.names.insert(id.to_string(), name.to_string());
            }
        }
        let attributes = row.get("attributes").cloned().unwrap_or_else(|| json!({}));
        let stored = Stored {
            id: id.to_string(),
            attributes,
        };
        self.catalog.by_id.insert(id.to_string(), stored.clone());
        Ok(Some(stored))
    }

    async fn label(&mut self, id: &str, fallback: &str) -> Result<String, GraphError> {
        if let Some(name) = self.names.get(id) {
            if !name.is_empty() {
                return Ok(name.clone());
            }
        }
        let mut response = self
            .tx
            .query("SELECT name FROM type::record($id)")
            .bind(("id", id.to_string()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let name = rows
            .into_iter()
            .find_map(|row| {
                row.get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| fallback.to_string());
        self.names.insert(id.to_string(), name.clone());
        Ok(name)
    }

    async fn live_edges(&mut self, subject_id: &str) -> Result<Vec<StoredEdge>, GraphError> {
        if let Some(edges) = self.edges.get(subject_id) {
            return Ok(edges.clone());
        }
        let mut response = self
            .tx
            .query(LIVE_EDGES_OF)
            .bind(("id", subject_id.to_string()))
            .await?;
        let edges: Vec<StoredEdge> = deserialize_take(&mut response, 0)?;
        self.edges.insert(subject_id.to_string(), edges.clone());
        Ok(edges)
    }
}

/// Outgoing live edges of one record. A graph-edge scan, not a table scan.
const LIVE_EDGES_OF: &str =
    "SELECT * FROM type::record($id)->(relates_to WHERE valid_until = NONE)";

/// Live `part_of` edges that point at one record. A graph-edge scan.
const SCOPE_PART_OF: &str = "SELECT in FROM type::record($id)<-(relates_to \
     WHERE rel_type = 'part_of' AND valid_until = NONE)";

const CLOSE_EDGE: &str = r#"
    UPDATE type::record($id) SET
        valid_until = type::datetime($valid_until),
        ended_by = $ended_by
    WHERE valid_until = NONE
    RETURN BEFORE
"#;

/// Affixes `canon_key` peels one of. A family peels every copy.
const KEY_AFFIXES: &[&str] = &[
    "default", "current", "new", "value", "number", "setting", "used",
];

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

fn single_valued(predicate: Predicate) -> bool {
    matches!(
        predicate,
        Predicate::Setting
            | Predicate::IssueSymptom
            | Predicate::IssueStatus
            | Predicate::IssueCause
            | Predicate::IssueFix
            | Predicate::ReleaseStatus
    )
}

/// `true` when `replaces` has a content token in the cited turn.
///
/// A content token is an alphanumeric run of at least three characters. The
/// cited turn is the one the fact names; the quote stands in when that turn
/// is not in the session.
fn replaces_supported(fact: &Fact, turns: &[Turn]) -> bool {
    let tokens = content_tokens(&fact.replaces);
    if tokens.is_empty() {
        return false;
    }
    let text = turns
        .iter()
        .find(|turn| turn.index == fact.turn)
        .map(|turn| turn.text.as_str())
        .unwrap_or(fact.quote.as_str());
    let present = content_tokens(text);
    tokens
        .iter()
        .any(|token| present.iter().any(|item| item == token))
}

fn content_tokens(text: &str) -> Vec<String> {
    alphanumeric_runs(text)
        .into_iter()
        .filter(|token| token.chars().count() >= 3)
        .collect()
}

fn alphanumeric_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// Midnight UTC of a calendar day, or none when `date` is not one.
///
/// `YYYY-MM-DD` and a `T` timestamp of that day both stamp the day. A blank
/// date or a time written with a space is not a day, and a retirement that
/// needed one is skipped rather than failing the session.
fn session_midnight(date: &str) -> Option<String> {
    let day = match date.split_once('T') {
        Some((day, _)) => day,
        None => date,
    };
    if day.len() != 10 {
        return None;
    }
    let mut parts = day.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let dom: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !is_calendar_day(year, month, dom) {
        return None;
    }
    Some(format!("{day}T00:00:00Z"))
}

fn is_calendar_day(year: i32, month: u32, day: u32) -> bool {
    let max = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max).contains(&day)
}

/// Keys name one setting when they are equal, or when the tokens left after
/// the listed affixes are the same non-empty list.
///
/// `port` and `port_value_setting` match. `storage` and `tag_storage` do not:
/// sharing a token is not a family. A new key is rewritten onto the live key
/// it repeats, and a different value closes that live setting.
fn keys_match(left: &str, right: &str) -> bool {
    let left = fold_key(left);
    let right = fold_key(right);
    if left == right {
        return true;
    }
    let left_stem = key_stem(&left);
    let right_stem = key_stem(&right);
    !left_stem.is_empty() && left_stem == right_stem
}

/// The key stored on a new slot: trimmed and lower case.
fn fold_key(key: &str) -> String {
    key.trim().to_lowercase()
}

fn key_stem(key: &str) -> Vec<String> {
    key.split('_')
        .filter(|token| !token.is_empty() && !KEY_AFFIXES.contains(token))
        .map(str::to_string)
        .collect()
}

fn preferred_key(incoming: &str, slots: &[Value]) -> String {
    let incoming = fold_key(incoming);
    let mut matches: Vec<&str> = slots
        .iter()
        .filter_map(|slot| {
            if !slot_live(slot)
                || slot_str(slot, "predicate") != Some("setting")
                || slot_str(slot, "status") != Some("decided")
            {
                return None;
            }
            let key = slot_str(slot, "key")?;
            keys_match(&incoming, key).then_some(key)
        })
        .collect();
    // An exact key, or the same key with different case, is stored folded.
    // A longer family member is rewritten onto the shortest live key.
    if matches.iter().any(|key| fold_key(key) == incoming) {
        return incoming;
    }
    matches.sort_by(|left, right| left.len().cmp(&right.len()).then(left.cmp(right)));
    matches
        .first()
        .map(|key| (*key).to_string())
        .unwrap_or(incoming)
}

fn supersede_target(slot: &Value, fact: &Fact) -> bool {
    if !slot_live(slot) || slot_str(slot, "status") != Some("decided") {
        return false;
    }
    if slot_str(slot, "predicate") != Some(fact.predicate.as_str()) {
        return false;
    }
    if values_equal(slot_str(slot, "value").unwrap_or(""), &fact.object_name) {
        return false;
    }
    if fact.predicate == Predicate::Setting {
        return keys_match(slot_str(slot, "key").unwrap_or(""), &fact.key);
    }
    true
}

fn considered_same_slot(slot: &Value, fact: &Fact) -> bool {
    slot_live(slot)
        && slot_str(slot, "status") == Some("considered")
        && slot_str(slot, "predicate") == Some(fact.predicate.as_str())
        && slot_key_matches(slot, fact)
        && values_equal(slot_str(slot, "value").unwrap_or(""), &fact.object_name)
}

fn same_decided_slot(slot: &Value, fact: &Fact) -> bool {
    slot_str(slot, "status") == Some("decided")
        && slot_str(slot, "predicate") == Some(fact.predicate.as_str())
        && slot_key_matches(slot, fact)
        && values_equal(slot_str(slot, "value").unwrap_or(""), &fact.object_name)
}

/// A rejected setting must name the same key. Other slot predicates match on
/// the predicate and the value.
fn rejected_key_ok(slot: &Value, fact: &Fact) -> bool {
    fact.predicate != Predicate::Setting || slot_key_matches(slot, fact)
}

/// Literal `replaces` closes only the single-valued slots.
fn literal_slot_ok(slot: &Value) -> bool {
    slot_str(slot, "predicate")
        .and_then(Predicate::parse)
        .is_some_and(single_valued)
}

fn slot_key_matches(slot: &Value, fact: &Fact) -> bool {
    let key = slot_str(slot, "key").unwrap_or("");
    if fact.predicate == Predicate::Setting {
        keys_match(key, &fact.key)
    } else if single_valued(fact.predicate) {
        fold_key(key) == fold_key(&fact.key)
    } else {
        key == fact.key
    }
}

fn same_decided_edge(edge: &StoredEdge, fact: &Fact, object_id: &str) -> bool {
    let description = fact.role.map(|role| format!("role={}", role.as_str()));
    edge.status.as_deref() == Some("decided")
        && edge.rel_type == fact.predicate.as_str()
        && edge.description.as_deref() == description.as_deref()
        && record_id(&edge.to_id) == object_id
}

fn open_status(slot: &Value, allowed: &[&str]) -> bool {
    slot_live(slot) && slot_str(slot, "status").is_some_and(|status| allowed.contains(&status))
}

fn status_is(edge: &StoredEdge, allowed: &[&str]) -> bool {
    edge.status
        .as_deref()
        .is_some_and(|status| allowed.contains(&status))
}

fn values_equal(left: &str, right: &str) -> bool {
    fold_value(left) == fold_value(right)
}

/// Lower case, with whitespace collapsed. `-`, `+`, `#`, `.` and `/` stay,
/// so a sign, a version dot and a path separator still distinguish values.
/// Any other punctuation becomes whitespace.
fn fold_value(value: &str) -> String {
    let mut folded = String::new();
    let mut spaced = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            spaced = true;
            continue;
        }
        if ch.is_alphanumeric() || matches!(ch, '-' | '+' | '#' | '.' | '/') {
            if spaced && !folded.is_empty() {
                folded.push(' ');
            }
            spaced = false;
            for lower in ch.to_lowercase() {
                folded.push(lower);
            }
        } else {
            spaced = true;
        }
    }
    folded
}

/// Whole folded value, one whole path segment, or a contiguous run of two
/// or more tokens. One token is not a substring and not a token inside a
/// longer value: `json` matches a value that is `json`, and a path segment
/// `json`, and does not match `JSON logs`.
fn literal_matches(value: &str, replaces: &str) -> bool {
    let needle = fold_value(replaces);
    if needle.is_empty() {
        return false;
    }
    if fold_value(value) == needle {
        return true;
    }
    if value
        .split(['/', '\\'])
        .any(|segment| fold_value(segment) == needle)
    {
        return true;
    }
    let needle_tokens = alphanumeric_runs(replaces);
    if needle_tokens.len() < 2 {
        return false;
    }
    let value_tokens = alphanumeric_runs(value);
    value_tokens
        .windows(needle_tokens.len())
        .any(|window| window == needle_tokens.as_slice())
}

fn slot_live(slot: &Value) -> bool {
    !edge_closed(slot.get("valid_until"))
}

fn edge_closed(valid_until: Option<&Value>) -> bool {
    crud::edge_already_closed(valid_until)
}

/// A fact stored in this session stays live for the rest of the session.
///
/// Rejected facts run before decided facts of the same turn, so a rejection
/// does not see the decided fact beside it. A later turn does not close an
/// earlier fact of this session either.
fn opened_here(evidence: Option<&Value>, session_id: &str) -> bool {
    let Some(items) = evidence.and_then(Value::as_array) else {
        return false;
    };
    let Some(first) = items.first() else {
        return false;
    };
    first.get("session").and_then(Value::as_str) == Some(session_id)
}

fn stamp_slot(slot: &mut Value, until: &str, ended_by: &str) -> bool {
    if !slot_live(slot) {
        return false;
    }
    let Some(object) = slot.as_object_mut() else {
        return false;
    };
    object.insert("valid_until".to_string(), json!(until));
    object.insert("ended_by".to_string(), json!(ended_by));
    true
}

fn slot_list(attributes: &Value) -> Result<Vec<Value>, ()> {
    let Some(v2) = attributes.get("v2") else {
        return Ok(Vec::new());
    };
    if !v2.is_object() {
        return Err(());
    }
    match v2.get("slots") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(()),
    }
}

fn stored_kind(attributes: &Value) -> Option<Kind> {
    attributes
        .get("v2")
        .and_then(|v2| v2.get("kind"))
        .and_then(Value::as_str)
        .and_then(Kind::parse)
}

fn same_slot(slot: &Value, fact: &Fact) -> bool {
    let live = slot.get("valid_until").is_none_or(Value::is_null);
    if !(live
        && slot_str(slot, "predicate") == Some(fact.predicate.as_str())
        && slot_str(slot, "status") == Some(fact.status.as_str()))
    {
        return false;
    }
    // A single-valued slot reinforces when the folded values agree. A rule,
    // a preference or release contents stays exact, so two wordings remain
    // two facts.
    if single_valued(fact.predicate) {
        return slot_key_matches(slot, fact)
            && values_equal(slot_str(slot, "value").unwrap_or(""), &fact.object_name);
    }
    slot_str(slot, "key") == Some(fact.key.as_str())
        && slot_str(slot, "value") == Some(fact.object_name.as_str())
}

struct Closed {
    predicate: String,
    key: String,
    old_value: String,
}

fn closed_of(slot: &Value, fact: &Fact) -> Closed {
    Closed {
        predicate: slot_str(slot, "predicate")
            .unwrap_or(fact.predicate.as_str())
            .to_string(),
        key: slot_str(slot, "key").unwrap_or("").to_string(),
        old_value: slot_str(slot, "value").unwrap_or("").to_string(),
    }
}

async fn refresh_catalog(tx: &Transaction<Db>, catalog: &mut Catalog) -> Result<(), GraphError> {
    let ids: Vec<String> = catalog.by_id.keys().cloned().collect();
    for id in ids {
        let mut response = tx
            .query("SELECT attributes FROM type::record($id)")
            .bind(("id", id.clone()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let Some(row) = rows.into_iter().next() else {
            continue;
        };
        let attributes = row
            .get("attributes")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        catalog.set_attributes(&id, attributes);
    }
    Ok(())
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
    let (provenance, label) = provenance_of(fact.provenance);
    let evidence = prior_evidence(provenance);
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

fn provenance_of(provenance: Provenance) -> (Provenance, &'static str) {
    (provenance, provenance.as_str())
}

/// The creating sighting stores this prior and does not corroborate.
///
/// User text is explicit (0.9). An assistant turn is inferred (0.6). A fact
/// that is not quotable from one turn is speculative (0.3). v2 does not write
/// an external class; that arm keeps the explicit prior.
fn prior_evidence(provenance: Provenance) -> Evidence {
    let mean = match provenance {
        Provenance::User | Provenance::External => ExtractionContext::Explicit.prior(),
        Provenance::Assistant => ExtractionContext::Inferred.prior(),
        Provenance::SelfGenerated => ExtractionContext::Speculative.prior(),
    };
    Evidence::from_prior(mean)
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
    use crate::graph::v2::input::TurnRole;
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

    #[test]
    fn create_stores_the_class_prior_and_does_not_corroborate() {
        let user = setting("redis");
        assert_eq!(user.provenance, Provenance::User);
        let slot = new_slot("s1", "2026-01-02", &user);
        assert_slot_prior(&slot, "user", 9.0, 1.0);

        let mut assistant = user.clone();
        assistant.provenance = Provenance::Assistant;
        let slot = new_slot("s1", "2026-01-02", &assistant);
        assert_slot_prior(&slot, "assistant", 6.0, 4.0);

        let mut inferred = user.clone();
        inferred.provenance = Provenance::SelfGenerated;
        let slot = new_slot("s1", "2026-01-02", &inferred);
        assert_slot_prior(&slot, "self", 3.0, 7.0);
    }

    fn assert_slot_prior(slot: &Value, label: &str, alpha: f64, beta: f64) {
        assert_eq!(slot["provenance"], label);
        let got_alpha = slot["alpha"].as_f64().expect("alpha");
        let got_beta = slot["beta"].as_f64().expect("beta");
        assert!((got_alpha - alpha).abs() < 1e-9, "{got_alpha} != {alpha}");
        assert!((got_beta - beta).abs() < 1e-9, "{got_beta} != {beta}");
        assert_eq!(slot["self_reinforcements"], 0);
        let keys: std::collections::BTreeSet<_> =
            slot.as_object().expect("slot").keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "alpha",
                "beta",
                "ended_by",
                "evidence",
                "key",
                "predicate",
                "provenance",
                "self_reinforcements",
                "status",
                "valid_from",
                "valid_until",
                "value",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
        let evidence = slot["evidence"].as_array().expect("evidence");
        assert_eq!(evidence.len(), 1);
        let item_keys: std::collections::BTreeSet<_> = evidence[0]
            .as_object()
            .expect("item")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            item_keys,
            ["quote", "role", "session", "turn"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        assert_eq!(evidence[0]["role"], label);
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

    /// An assistant create stores the inferred prior and does not tally. A
    /// later user sighting adds the user weight only. A later assistant
    /// sighting adds the assistant weight and one tally.
    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn assistant_create_then_user_and_assistant_sightings() {
        let (_tmp, gm) = open_store().await;
        let weights = *gm.provenance_weights();
        assert!((weights.weight_user - 0.8).abs() < 1e-9);
        assert!((weights.weight_assistant - 0.3).abs() < 1e-9);
        let prior = Evidence::from_prior(ExtractionContext::Inferred.prior());

        let mut created = setting("redis");
        created.provenance = Provenance::Assistant;
        created.turn = 1;
        created.quote = "cache is redis".into();
        write_session(&gm, "s1", "2026-01-02", &[created])
            .await
            .unwrap();
        let slot = cache_slot(&gm).await;
        assert_close(slot["alpha"].as_f64().unwrap(), prior.alpha());
        assert_close(slot["beta"].as_f64().unwrap(), prior.beta());
        assert_eq!(slot["self_reinforcements"], 0);
        assert_eq!(slot["provenance"], "assistant");
        assert_eq!(slot["evidence"].as_array().unwrap().len(), 1);

        let mut user = setting("redis");
        user.provenance = Provenance::User;
        user.turn = 2;
        user.quote = "the cache stays redis".into();
        write_session(&gm, "s2", "2026-01-03", &[user])
            .await
            .unwrap();
        let slot = cache_slot(&gm).await;
        assert_close(
            slot["alpha"].as_f64().unwrap(),
            prior.alpha() + weights.weight_user,
        );
        assert_close(slot["beta"].as_f64().unwrap(), prior.beta());
        assert_eq!(slot["self_reinforcements"], 0);
        assert_eq!(slot["evidence"].as_array().unwrap().len(), 2);

        let mut assistant = setting("redis");
        assistant.provenance = Provenance::Assistant;
        assistant.turn = 3;
        assistant.quote = "noted that the cache is redis".into();
        write_session(&gm, "s3", "2026-01-04", &[assistant])
            .await
            .unwrap();
        let slot = cache_slot(&gm).await;
        assert_close(
            slot["alpha"].as_f64().unwrap(),
            prior.alpha() + weights.weight_user + weights.weight_assistant,
        );
        assert_close(slot["beta"].as_f64().unwrap(), prior.beta());
        assert_eq!(slot["self_reinforcements"], 1);
        assert_eq!(slot["evidence"].as_array().unwrap().len(), 3);
    }

    #[cfg(feature = "embedded")]
    async fn cache_slot(gm: &GraphMemory) -> Value {
        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        let app = entities.iter().find(|entity| entity.name == "app").unwrap();
        app.attributes.as_ref().unwrap()["v2"]["slots"][0].clone()
    }

    fn assert_close(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-9, "{got} != {want}");
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
        let ctx = WriteCtx::new(&llm, &project, session_id, date, &[]);
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
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    fn decided_slot(key: &str, value: &str, turn: usize) -> Fact {
        Fact {
            subject_name: "app".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: key.into(),
            object_name: value.into(),
            object_kind: None,
            status: "decided".into(),
            turn,
            quote: format!("{key} is {value}"),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    fn uses_named(
        subject: &str,
        kind: Kind,
        object: &str,
        status: &str,
        replaces: &str,
        turn: usize,
    ) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind: kind,
            predicate: Predicate::Uses,
            role: Some(Role::Library),
            key: String::new(),
            object_name: object.into(),
            object_kind: Some(Kind::Technology),
            status: status.into(),
            turn,
            quote: format!("{subject} uses {object}"),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: replaces.into(),
        }
    }

    async fn write_turns(
        gm: &GraphMemory,
        session_id: &str,
        date: &str,
        turns: &[Turn],
        facts: &[Fact],
        budget: Option<&AtomicU32>,
    ) -> Result<WriteStats, GraphError> {
        let project = ProjectIdentity {
            name: String::new(),
            aliases: Vec::new(),
        };
        let llm = Quiet;
        let ctx = match budget {
            Some(budget) => WriteCtx {
                llm: &llm,
                project: &project,
                session_id,
                date,
                turns,
                conflict_budget: budget,
                valid_until: session_midnight(date),
                on_conflict: None,
            },
            None => WriteCtx::new(&llm, &project, session_id, date, turns),
        };
        write_facts(gm, &ctx, facts).await
    }

    async fn app_slots(gm: &GraphMemory) -> Vec<Value> {
        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        let app = entities.iter().find(|entity| entity.name == "app").unwrap();
        app.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn slot_named<'a>(slots: &'a [Value], key: &str, value: &str) -> &'a Value {
        slots
            .iter()
            .find(|slot| slot["key"] == key && slot["value"] == value)
            .unwrap_or_else(|| panic!("missing {key}={value} in {slots:?}"))
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn setting_slot_retires_old_value_on_session_date() {
        let (_tmp, gm) = open_store().await;
        write_turns(
            &gm,
            "s03",
            "2026-04-03",
            &[],
            &[decided_slot("port", "8080", 1)],
            None,
        )
        .await
        .unwrap();
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let first = write_turns(
            &gm,
            "s11",
            "2026-04-11",
            &[],
            &[decided_slot("port", "8765", 1)],
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.retirements.len(), 1);
        assert_eq!(first.retirements[0].reason, "superseded");
        assert_eq!(first.retirements[0].subject, "app");
        assert_eq!(first.retirements[0].predicate, "setting");
        assert_eq!(first.retirements[0].old_value, "8080");
        assert_eq!(first.retirements[0].new_value, "8765");

        let slots = app_slots(&gm).await;
        let old = slot_named(&slots, "port", "8080");
        let live = slot_named(&slots, "port", "8765");
        assert_eq!(old["valid_until"], "2026-04-11T00:00:00Z");
        assert_eq!(old["ended_by"], "s11:superseded");
        assert!((old["alpha"].as_f64().unwrap() - prior.alpha()).abs() < 1e-9);
        assert!((old["beta"].as_f64().unwrap() - prior.beta()).abs() < 1e-9);
        assert_eq!(old["evidence"].as_array().unwrap().len(), 1);
        assert!(live["valid_until"].is_null());
        assert!(live["ended_by"].is_null());
        let live_alpha = live["alpha"].as_f64().unwrap();

        let retry = write_turns(
            &gm,
            "s11",
            "2026-04-11",
            &[],
            &[decided_slot("port", "8765", 1)],
            None,
        )
        .await
        .unwrap();
        assert!(retry.retirements.is_empty());
        assert_eq!(retry.already_seen, 1);
        assert_eq!(retry.reinforced, 0);
        let slots = app_slots(&gm).await;
        let old = slot_named(&slots, "port", "8080");
        let live = slot_named(&slots, "port", "8765");
        assert_eq!(old["valid_until"], "2026-04-11T00:00:00Z");
        assert_eq!(old["ended_by"], "s11:superseded");
        assert_eq!(old["evidence"].as_array().unwrap().len(), 1);
        assert!((live["alpha"].as_f64().unwrap() - live_alpha).abs() < 1e-9);
        assert!((live["beta"].as_f64().unwrap() - prior.beta()).abs() < 1e-9);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn key_family_match_retires_sibling_key() {
        let (_tmp, gm) = open_store().await;
        write_session(&gm, "s1", "2026-04-03", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[decided_slot("port_value_setting", "8765", 1)],
        )
        .await
        .unwrap();
        let slots = app_slots(&gm).await;
        assert!(slots.iter().all(|slot| slot["key"] == "port"), "{slots:?}");
        let old = slot_named(&slots, "port", "8080");
        let live = slot_named(&slots, "port", "8765");
        assert_eq!(old["ended_by"], "s2:superseded");
        assert!(live["valid_until"].is_null());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn distinct_keys_sharing_a_token_stay_live() {
        let (_tmp, gm) = open_store().await;
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                decided_slot("storage", "shelf.json", 1),
                decided_slot("port", "8080", 2),
            ],
        )
        .await
        .unwrap();
        let stats = write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[
                decided_slot("tag_storage", "join table", 1),
                decided_slot("serve_port", "8765", 2),
            ],
        )
        .await
        .unwrap();
        assert!(stats.retirements.is_empty(), "{:?}", stats.retirements);
        let slots = app_slots(&gm).await;
        for (key, value) in [
            ("storage", "shelf.json"),
            ("tag_storage", "join table"),
            ("port", "8080"),
            ("serve_port", "8765"),
        ] {
            assert!(slot_named(&slots, key, value)["valid_until"].is_null());
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn uses_never_auto_retires() {
        let (_tmp, gm) = open_store().await;
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[uses_named(
                "app",
                Kind::Project,
                "requests",
                "decided",
                "",
                1,
            )],
        )
        .await
        .unwrap();
        let stats = write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[uses_named("app", Kind::Project, "httpx", "decided", "", 1)],
        )
        .await
        .unwrap();
        assert!(stats.retirements.is_empty(), "{:?}", stats.retirements);
        let edges = live_uses(&gm).await;
        assert_eq!(edges.len(), 2, "{edges:?}");
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn rejected_retires_across_components() {
        let (_tmp, gm) = open_store().await;
        let part = Fact {
            subject_name: "worker".into(),
            subject_kind: Kind::Component,
            predicate: Predicate::PartOf,
            role: None,
            key: String::new(),
            object_name: "app".into(),
            object_kind: Some(Kind::Project),
            status: "decided".into(),
            turn: 1,
            quote: "worker is part of app".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        };
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                uses_named("app", Kind::Project, "requests", "decided", "", 1),
                uses_named("worker", Kind::Component, "requests", "decided", "", 2),
                part,
            ],
        )
        .await
        .unwrap();
        let before = edge_beliefs(&gm).await;
        let stats = write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[uses_named(
                "app",
                Kind::Project,
                "requests",
                "rejected",
                "",
                1,
            )],
        )
        .await
        .unwrap();
        assert_eq!(stats.retirements.len(), 2, "{:?}", stats.retirements);
        assert!(stats
            .retirements
            .iter()
            .all(|item| item.reason == "rejected"));
        assert!(stats
            .retirements
            .iter()
            .all(|item| item.new_value.is_empty()));
        assert!(stats
            .retirements
            .iter()
            .any(|item| item.subject == "app" && item.old_value == "requests"));
        assert!(stats
            .retirements
            .iter()
            .any(|item| item.subject == "worker" && item.old_value == "requests"));
        let closed = closed_uses(&gm, "decided").await;
        assert_eq!(closed.len(), 2, "{closed:?}");
        assert!(closed
            .iter()
            .all(|row| row.ended_by.as_deref() == Some("s2:rejected")));
        assert!(closed
            .iter()
            .all(|row| row.until.contains("2026-04-11T00:00:00")));
        let after = edge_beliefs(&gm).await;
        assert_eq!(after, before, "retirement does not move alpha or beta");
        let retry = write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[uses_named(
                "app",
                Kind::Project,
                "requests",
                "rejected",
                "",
                1,
            )],
        )
        .await
        .unwrap();
        assert!(retry.retirements.is_empty(), "{:?}", retry.retirements);
        assert_eq!(closed_uses(&gm, "decided").await, closed);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn replaces_retires_uses_on_the_project_and_its_component() {
        let (_tmp, gm) = open_store().await;
        let part = Fact {
            subject_name: "worker".into(),
            subject_kind: Kind::Component,
            predicate: Predicate::PartOf,
            role: None,
            key: String::new(),
            object_name: "app".into(),
            object_kind: Some(Kind::Project),
            status: "decided".into(),
            turn: 1,
            quote: "worker is part of app".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        };
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                uses_named("app", Kind::Project, "requests", "decided", "", 1),
                uses_named("worker", Kind::Component, "requests", "decided", "", 2),
                part,
            ],
        )
        .await
        .unwrap();
        let turns = [Turn {
            index: 3,
            role: TurnRole::User,
            text: "switch the client to httpx instead of requests".into(),
        }];
        let mut next = uses_named("app", Kind::Project, "httpx", "decided", "requests", 3);
        next.quote = turns[0].text.clone();
        let stats = write_turns(&gm, "s2", "2026-04-11", &turns, &[next], None)
            .await
            .unwrap();
        assert_eq!(stats.retirements.len(), 2, "{:?}", stats.retirements);
        assert!(stats
            .retirements
            .iter()
            .all(|item| item.reason == "replaced"));
        assert!(stats
            .retirements
            .iter()
            .all(|item| item.new_value == "httpx"));
        let closed = closed_uses(&gm, "decided").await;
        assert_eq!(closed.len(), 2, "{closed:?}");
        assert!(closed
            .iter()
            .all(|row| row.ended_by.as_deref() == Some("s2:replaced")));
        assert_eq!(live_uses(&gm).await.len(), 1);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn replaces_retires_setting_value_across_predicates() {
        let (_tmp, gm) = open_store().await;
        let mut path = decided_slot("storage_path", "~/.local/share/x/shelf.json", 1);
        path.quote = "the shelf file is ~/.local/share/x/shelf.json".into();
        write_session(&gm, "s1", "2026-04-03", &[path])
            .await
            .unwrap();
        let turns = [Turn {
            index: 4,
            role: TurnRole::User,
            text: "move storage onto SQLite and drop shelf.json".into(),
        }];
        let mut fact = uses_named("app", Kind::Project, "SQLite", "decided", "shelf.json", 4);
        fact.quote = turns[0].text.clone();
        let stats = write_turns(&gm, "s2", "2026-04-11", &turns, &[fact], None)
            .await
            .unwrap();
        assert_eq!(stats.retirements.len(), 1, "{:?}", stats.retirements);
        assert_eq!(stats.retirements[0].reason, "replaced");
        assert_eq!(stats.retirements[0].predicate, "setting");
        assert_eq!(
            stats.retirements[0].old_value,
            "~/.local/share/x/shelf.json"
        );
        assert_eq!(stats.retirements[0].new_value, "SQLite");
        let slots = app_slots(&gm).await;
        let old = slot_named(&slots, "storage_path", "~/.local/share/x/shelf.json");
        assert_eq!(old["ended_by"], "s2:replaced");
        assert_eq!(old["valid_until"], "2026-04-11T00:00:00Z");
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn replaces_without_support_in_turn_is_ignored() {
        let (_tmp, gm) = open_store().await;
        let path = decided_slot("storage_path", "~/.local/share/x/shelf.json", 1);
        write_session(&gm, "s1", "2026-04-03", &[path])
            .await
            .unwrap();
        let turns = [Turn {
            index: 4,
            role: TurnRole::User,
            text: "switch the store to SQLite tomorrow".into(),
        }];
        let mut fact = uses_named("app", Kind::Project, "SQLite", "decided", "shelf.json", 4);
        fact.quote = turns[0].text.clone();
        let stats = write_turns(&gm, "s2", "2026-04-11", &turns, &[fact], None)
            .await
            .unwrap();
        assert!(stats.retirements.is_empty(), "{:?}", stats.retirements);
        let slots = app_slots(&gm).await;
        assert!(
            slot_named(&slots, "storage_path", "~/.local/share/x/shelf.json")["valid_until"]
                .is_null()
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn considered_to_decided_promotion() {
        let (_tmp, gm) = open_store().await;
        let mut considered = decided_slot("port", "8080", 1);
        considered.status = "considered".into();
        write_session(&gm, "s1", "2026-04-03", &[considered])
            .await
            .unwrap();
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let stats = write_session(&gm, "s2", "2026-04-11", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        assert_eq!(stats.retirements.len(), 1);
        assert_eq!(stats.retirements[0].reason, "promoted");
        let slots = app_slots(&gm).await;
        let old = slots
            .iter()
            .find(|slot| slot["status"] == "considered")
            .unwrap();
        let live = slots
            .iter()
            .find(|slot| slot["status"] == "decided")
            .unwrap();
        assert_eq!(old["ended_by"], "s2:promoted");
        assert_eq!(old["valid_until"], "2026-04-11T00:00:00Z");
        assert!((old["alpha"].as_f64().unwrap() - prior.alpha()).abs() < 1e-9);
        assert!((old["beta"].as_f64().unwrap() - prior.beta()).abs() < 1e-9);
        assert_eq!(old["evidence"].as_array().unwrap().len(), 1);
        assert!(live["valid_until"].is_null());
        let retry = write_session(&gm, "s2", "2026-04-11", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        assert!(retry.retirements.is_empty());
        assert_eq!(retry.already_seen, 1);
        let slots = app_slots(&gm).await;
        let old = slots
            .iter()
            .find(|slot| slot["status"] == "considered")
            .unwrap();
        assert_eq!(old["ended_by"], "s2:promoted");
        assert_eq!(old["evidence"].as_array().unwrap().len(), 1);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn retired_record_is_never_reinforced() {
        let (_tmp, gm) = open_store().await;
        write_session(&gm, "s1", "2026-04-03", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        write_session(&gm, "s2", "2026-04-11", &[decided_slot("port", "8765", 1)])
            .await
            .unwrap();
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let stats = write_session(&gm, "s3", "2026-05-01", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        assert_eq!(stats.reinforced, 0);
        assert_eq!(stats.slots, 1);
        let slots = app_slots(&gm).await;
        let retired: Vec<_> = slots
            .iter()
            .filter(|slot| slot["value"] == "8080" && !slot["valid_until"].is_null())
            .collect();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0]["ended_by"], "s2:superseded");
        assert_eq!(retired[0]["valid_until"], "2026-04-11T00:00:00Z");
        assert_eq!(retired[0]["evidence"].as_array().unwrap().len(), 1);
        assert!((retired[0]["alpha"].as_f64().unwrap() - prior.alpha()).abs() < 1e-9);
        let revived: Vec<_> = slots
            .iter()
            .filter(|slot| slot["value"] == "8080" && slot["valid_until"].is_null())
            .collect();
        assert_eq!(revived.len(), 1);
        assert_eq!(revived[0]["evidence"][0]["session"], "s3");
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn later_turn_does_not_retire_a_fact_from_this_session() {
        let (_tmp, gm) = open_store().await;
        let stats = write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                decided_slot("port", "8080", 0),
                decided_slot("port", "8765", 1),
            ],
        )
        .await
        .unwrap();
        assert!(stats.retirements.is_empty(), "{:?}", stats.retirements);
        let slots = app_slots(&gm).await;
        assert!(slot_named(&slots, "port", "8080")["valid_until"].is_null());
        assert!(slot_named(&slots, "port", "8765")["valid_until"].is_null());
        write_session(&gm, "s2", "2026-04-11", &[decided_slot("port", "9000", 1)])
            .await
            .unwrap();
        let slots = app_slots(&gm).await;
        assert_eq!(
            slot_named(&slots, "port", "8080")["ended_by"],
            "s2:superseded"
        );
        assert_eq!(
            slot_named(&slots, "port", "8765")["ended_by"],
            "s2:superseded"
        );
        assert!(slot_named(&slots, "port", "9000")["valid_until"].is_null());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn commit_conflict_retries_once_and_retires_once() {
        let (_tmp, gm) = open_store().await;
        write_session(&gm, "s03", "2026-04-03", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        let budget = AtomicU32::new(1);
        let stats = write_turns(
            &gm,
            "s11",
            "2026-04-11",
            &[],
            &[decided_slot("port", "8765", 1)],
            Some(&budget),
        )
        .await
        .unwrap();
        assert_eq!(budget.load(Ordering::SeqCst), 0);
        assert_eq!(stats.retirements.len(), 1);
        assert_eq!(stats.slots, 1);
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let slots = app_slots(&gm).await;
        let old = slot_named(&slots, "port", "8080");
        let live = slot_named(&slots, "port", "8765");
        assert_eq!(old["ended_by"], "s11:superseded");
        assert_eq!(old["valid_until"], "2026-04-11T00:00:00Z");
        assert!(live["valid_until"].is_null());
        assert!((live["alpha"].as_f64().unwrap() - prior.alpha()).abs() < 1e-9);
        assert_eq!(live["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(
            slots.iter().filter(|slot| slot["value"] == "8765").count(),
            1
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn commit_conflict_twice_leaves_prior_session() {
        let (_tmp, gm) = open_store().await;
        write_session(&gm, "s03", "2026-04-03", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        let budget = AtomicU32::new(2);
        let err = write_turns(
            &gm,
            "s11",
            "2026-04-11",
            &[],
            &[decided_slot("port", "8765", 1)],
            Some(&budget),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("conflict"), "{err}");
        assert_eq!(budget.load(Ordering::SeqCst), 0);
        let slots = app_slots(&gm).await;
        assert_eq!(slots.len(), 1);
        assert!(slot_named(&slots, "port", "8080")["valid_until"].is_null());
        assert!(slots.iter().all(|slot| slot["value"] != "8765"));
    }

    #[test]
    fn folded_values_keep_sign_version_and_path_marks() {
        assert_eq!(
            session_midnight("2026-04-11").as_deref(),
            Some("2026-04-11T00:00:00Z")
        );
        assert_eq!(
            session_midnight("2026-04-11T15:04:05Z").as_deref(),
            Some("2026-04-11T00:00:00Z")
        );
        assert_eq!(session_midnight("2026-04-11 10:00"), None);
        assert_eq!(session_midnight(""), None);
        assert_eq!(session_midnight("2026-02-29"), None);
        assert_eq!(
            session_midnight("2024-02-29").as_deref(),
            Some("2024-02-29T00:00:00Z")
        );

        assert!(values_equal("true", "True"));
        assert!(values_equal("Port", "port"));
        assert!(!values_equal("1", "-1"));
        assert!(!values_equal("c", "c++"));
        assert!(!values_equal("v1.2", "v12"));
        assert!(keys_match("Port", "port"));
        assert!(keys_match("port", "port_value_setting"));
        assert!(!keys_match("storage", "tag_storage"));
        assert!(!keys_match("port", "serve_port"));

        // One token matches the whole value or one path segment, not a token
        // inside a longer value. Two tokens still match a contiguous run.
        assert!(literal_matches("json", "json"));
        assert!(literal_matches("dir/json/file", "json"));
        assert!(literal_matches("~/.local/share/x/shelf.json", "shelf.json"));
        assert!(!literal_matches("Errors are returned as JSON", "json"));
        assert!(!literal_matches("JSON logs", "json"));
        assert!(!literal_matches("json lines", "json"));
        assert!(literal_matches("please drop shelf json now", "shelf json"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn rejected_setting_keeps_a_different_key_with_the_same_value() {
        let (_tmp, gm) = open_store().await;
        let part = Fact {
            subject_name: "worker".into(),
            subject_kind: Kind::Component,
            predicate: Predicate::PartOf,
            role: None,
            key: String::new(),
            object_name: "app".into(),
            object_kind: Some(Kind::Project),
            status: "decided".into(),
            turn: 1,
            quote: "worker is part of app".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        };
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                project_setting("app", Kind::Project, "cache_enabled", "true", "decided", 1),
                project_setting("app", Kind::Project, "debug", "true", "decided", 2),
                project_setting("worker", Kind::Component, "debug", "true", "decided", 3),
                part,
            ],
        )
        .await
        .unwrap();
        let stats = write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[project_setting(
                "app",
                Kind::Project,
                "debug",
                "true",
                "rejected",
                1,
            )],
        )
        .await
        .unwrap();
        assert_eq!(stats.retirements.len(), 2, "{:?}", stats.retirements);
        assert!(stats.retirements.iter().all(|item| {
            item.reason == "rejected" && item.key == "debug" && item.old_value == "true"
        }));
        assert!(stats.retirements.iter().any(|item| item.subject == "app"));
        assert!(stats
            .retirements
            .iter()
            .any(|item| item.subject == "worker"));
        let app = slots_of(&gm, "app").await;
        let worker = slots_of(&gm, "worker").await;
        assert!(slot_named(&app, "cache_enabled", "true")["valid_until"].is_null());
        assert_eq!(slot_named(&app, "debug", "true")["ended_by"], "s2:rejected");
        assert_eq!(
            slot_named(&worker, "debug", "true")["ended_by"],
            "s2:rejected"
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn one_word_replaces_does_not_close_rules_or_preferences() {
        let (_tmp, gm) = open_store().await;
        let mut rule = project_setting(
            "app",
            Kind::Project,
            "",
            "Errors are returned as JSON",
            "decided",
            1,
        );
        rule.predicate = Predicate::Rule;
        let mut prefers = project_setting("app", Kind::Project, "", "JSON logs", "decided", 2);
        prefers.predicate = Predicate::Prefers;
        let mut exact_rule = project_setting("app", Kind::Project, "", "json", "decided", 3);
        exact_rule.predicate = Predicate::Rule;
        let mut contents = project_setting("app", Kind::Project, "", "json", "decided", 4);
        contents.predicate = Predicate::ReleaseContents;
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                rule,
                prefers,
                exact_rule,
                contents,
                project_setting("app", Kind::Project, "log_format", "json", "decided", 5),
                project_setting("app", Kind::Project, "detail", "json lines", "decided", 6),
            ],
        )
        .await
        .unwrap();
        let turns = [Turn {
            index: 4,
            role: TurnRole::User,
            text: "uses SQLite replaces json".into(),
        }];
        let mut fact = uses_named("app", Kind::Project, "SQLite", "decided", "json", 4);
        fact.quote = turns[0].text.clone();
        let stats = write_turns(&gm, "s2", "2026-04-11", &turns, &[fact], None)
            .await
            .unwrap();
        // A one-word needle closes a setting when the whole folded value is
        // that word. `log_format=json` closes for that reason. `json lines`
        // only contains the token, so it stays. A rule, a preference and
        // release contents stay even when the value is exactly `json`.
        assert_eq!(stats.retirements.len(), 1, "{:?}", stats.retirements);
        assert_eq!(stats.retirements[0].predicate, "setting");
        assert_eq!(stats.retirements[0].key, "log_format");
        assert_eq!(stats.retirements[0].old_value, "json");
        let slots = app_slots(&gm).await;
        assert!(!slot_named(&slots, "log_format", "json")["valid_until"].is_null());
        assert!(slot_named(&slots, "detail", "json lines")["valid_until"].is_null());
        for predicate in ["rule", "prefers", "release_contents"] {
            let live = slots
                .iter()
                .any(|slot| slot["predicate"] == predicate && slot["valid_until"].is_null());
            assert!(live, "{predicate} closed: {slots:?}");
        }
        assert!(slots.iter().any(|slot| {
            slot["predicate"] == "rule"
                && slot["value"] == "Errors are returned as JSON"
                && slot["valid_until"].is_null()
        }));
        assert!(slots.iter().any(|slot| {
            slot["predicate"] == "prefers"
                && slot["value"] == "JSON logs"
                && slot["valid_until"].is_null()
        }));
        assert!(slots.iter().any(|slot| {
            slot["predicate"] == "rule" && slot["value"] == "json" && slot["valid_until"].is_null()
        }));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn bad_session_date_skips_retirements_and_still_writes() {
        for date in ["2026-04-11 10:00", ""] {
            let (_tmp, gm) = open_store().await;
            write_session(
                &gm,
                "s1",
                "2026-04-03",
                &[
                    decided_slot("port", "8080", 1),
                    uses_named("app", Kind::Project, "requests", "decided", "", 2),
                ],
            )
            .await
            .unwrap();
            let facts = [
                decided_slot("port", "8765", 1),
                uses_named("app", Kind::Project, "requests", "rejected", "", 2),
            ];
            let stats = write_turns(&gm, "s2", date, &[], &facts, None)
                .await
                .unwrap();
            assert!(
                stats.retirements.is_empty(),
                "{date}: {:?}",
                stats.retirements
            );
            assert!(
                stats.retirements_skipped >= 2,
                "{date}: skipped {}",
                stats.retirements_skipped
            );
            assert!(stats.slots >= 1, "{date}");
            let slots = app_slots(&gm).await;
            assert!(slot_named(&slots, "port", "8080")["valid_until"].is_null());
            assert!(slot_named(&slots, "port", "8765")["valid_until"].is_null());
            assert!(
                closed_uses(&gm, "decided").await.is_empty(),
                "{date}: the edge was retired"
            );
            write_turns(&gm, "s2", date, &[], &facts, None)
                .await
                .expect("a bad date fails the same way on retry");
            assert!(slot_named(&app_slots(&gm).await, "port", "8080")["valid_until"].is_null());
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn case_variants_reinforce_and_marked_values_stay_different() {
        let (_tmp, gm) = open_store().await;
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                decided_slot("enabled", "true", 1),
                decided_slot("Port", "keep", 2),
            ],
        )
        .await
        .unwrap();
        let folded = write_session(
            &gm,
            "s2",
            "2026-04-11",
            &[
                decided_slot("enabled", "True", 1),
                decided_slot("port", "keep", 2),
            ],
        )
        .await
        .unwrap();
        assert_eq!(folded.reinforced, 2, "{:?}", folded.retirements);
        assert!(folded.retirements.is_empty());
        let slots = app_slots(&gm).await;
        let enabled: Vec<_> = slots
            .iter()
            .filter(|slot| slot["key"] == "enabled" && slot["valid_until"].is_null())
            .collect();
        assert_eq!(enabled.len(), 1, "{slots:?}");
        assert_eq!(enabled[0]["value"], "true");
        assert_eq!(enabled[0]["evidence"].as_array().unwrap().len(), 2);
        let ports: Vec<_> = slots
            .iter()
            .filter(|slot| slot["key"] == "port" && slot["value"] == "keep")
            .collect();
        assert_eq!(ports.len(), 1, "{slots:?}");
        assert!(slots.iter().all(|slot| slot["key"] != "Port"));

        write_session(
            &gm,
            "s3",
            "2026-05-01",
            &[
                decided_slot("num", "1", 1),
                decided_slot("lang", "c", 2),
                decided_slot("ver", "v1.2", 3),
            ],
        )
        .await
        .unwrap();
        let distinct = write_session(
            &gm,
            "s4",
            "2026-05-02",
            &[
                decided_slot("num", "-1", 1),
                decided_slot("lang", "c++", 2),
                decided_slot("ver", "v12", 3),
            ],
        )
        .await
        .unwrap();
        // `1`/`-1`, `c`/`c++` and `v1.2`/`v12` are different values, so the
        // later write supersedes. A fold that dropped the sign, the pluses
        // or the dot would reinforce instead.
        assert_eq!(distinct.reinforced, 0);
        assert_eq!(distinct.retirements.len(), 3, "{:?}", distinct.retirements);
        let slots = app_slots(&gm).await;
        assert_eq!(slot_named(&slots, "num", "1")["ended_by"], "s4:superseded");
        assert!(slot_named(&slots, "num", "-1")["valid_until"].is_null());
        assert_eq!(slot_named(&slots, "lang", "c")["ended_by"], "s4:superseded");
        assert!(slot_named(&slots, "lang", "c++")["valid_until"].is_null());
        assert_eq!(
            slot_named(&slots, "ver", "v1.2")["ended_by"],
            "s4:superseded"
        );
        assert!(slot_named(&slots, "ver", "v12")["valid_until"].is_null());
    }

    /// Another writer commits a slot after attempt 1 rolls back. Attempt 2
    /// re-reads the entity inside its transaction and keeps that slot.
    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn retry_rereads_attributes_committed_between_attempts() {
        let (_tmp, gm) = open_store().await;
        write_session(&gm, "s1", "2026-04-03", &[decided_slot("port", "8080", 1)])
            .await
            .unwrap();
        let budget = AtomicU32::new(1);
        let hook = InsertOwner;
        let project = ProjectIdentity {
            name: String::new(),
            aliases: Vec::new(),
        };
        let llm = Quiet;
        let date = "2026-04-11";
        let ctx = WriteCtx {
            llm: &llm,
            project: &project,
            session_id: "s2",
            date,
            turns: &[],
            conflict_budget: &budget,
            valid_until: session_midnight(date),
            on_conflict: Some(&hook),
        };
        let stats = write_facts(&gm, &ctx, &[decided_slot("theme", "dark", 1)])
            .await
            .unwrap();
        assert_eq!(stats.slots, 1);
        assert_eq!(budget.load(Ordering::SeqCst), 0);
        let slots = app_slots(&gm).await;
        assert!(slot_named(&slots, "port", "8080")["valid_until"].is_null());
        assert!(slot_named(&slots, "theme", "dark")["valid_until"].is_null());
        assert!(
            slot_named(&slots, "owner", "ada")["valid_until"].is_null(),
            "the slot written between attempts was overwritten: {slots:?}"
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn live_edge_lookup_is_a_graph_scan_of_one_subject() {
        let (_tmp, gm) = open_store().await;
        write_session(
            &gm,
            "s1",
            "2026-04-03",
            &[
                uses_named("app", Kind::Project, "redis", "decided", "", 1),
                uses_named("other", Kind::Project, "postgres", "decided", "", 2),
            ],
        )
        .await
        .unwrap();
        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        let app = entities.iter().find(|entity| entity.name == "app").unwrap();
        let app_id = app.id_string();
        let mut response = gm
            .db()
            .query(LIVE_EDGES_OF)
            .bind(("id", app_id.clone()))
            .await
            .unwrap();
        let rows: Vec<StoredEdge> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        let all = crud::list_all_relationships(gm.db()).await.unwrap();
        assert_eq!(all.len(), 2, "the store holds an unrelated edge");
        assert_eq!(
            rows.len(),
            1,
            "the lookup returned unrelated edges: {rows:?}"
        );
        assert_eq!(rows[0].rel_type, "uses");

        let plan = explain_full(gm.db(), LIVE_EDGES_OF, &app_id).await;
        assert!(
            plan_has_operator(&plan, "GraphEdgeScan"),
            "live edges must be a graph-edge scan. Plan: {plan}"
        );
        assert!(
            !plan_has_operator(&plan, "TableScan"),
            "live edges scanned the table. Plan: {plan}"
        );
        let plan = explain_full(gm.db(), SCOPE_PART_OF, &app_id).await;
        assert!(
            plan_has_operator(&plan, "GraphEdgeScan"),
            "scope must be a graph-edge scan. Plan: {plan}"
        );
        assert!(
            !plan_has_operator(&plan, "TableScan"),
            "scope scanned the table. Plan: {plan}"
        );
    }

    #[cfg(feature = "embedded")]
    async fn explain_full(
        db: &surrealdb::Surreal<crate::graph::store::Db>,
        sql: &str,
        id: &str,
    ) -> Value {
        let mut response = db
            .query(format!("{sql} EXPLAIN FULL"))
            .bind(("id", id.to_string()))
            .await
            .expect("explain");
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).expect("plan");
        Value::Array(rows)
    }

    fn plan_has_operator(node: &Value, operator: &str) -> bool {
        match node {
            Value::Array(items) => items.iter().any(|item| plan_has_operator(item, operator)),
            Value::Object(map) => {
                map.get("operator").and_then(Value::as_str) == Some(operator)
                    || map.values().any(|value| plan_has_operator(value, operator))
            }
            _ => false,
        }
    }

    fn project_setting(
        subject: &str,
        kind: Kind,
        key: &str,
        value: &str,
        status: &str,
        turn: usize,
    ) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind: kind,
            predicate: Predicate::Setting,
            role: None,
            key: key.into(),
            object_name: value.into(),
            object_kind: None,
            status: status.into(),
            turn,
            quote: format!("{subject} {key} is {value}"),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    #[cfg(feature = "embedded")]
    struct InsertOwner;

    #[cfg(feature = "embedded")]
    impl ConflictHook for InsertOwner {
        fn between_attempts<'a>(
            &'a self,
            gm: &'a GraphMemory,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), GraphError>> + Send + 'a>>
        {
            Box::pin(async move { self.insert(gm).await })
        }
    }

    #[cfg(feature = "embedded")]
    impl InsertOwner {
        async fn insert(&self, gm: &GraphMemory) -> Result<(), GraphError> {
            let entities = crud::list_entities(gm.db(), None).await?;
            let app = entities
                .iter()
                .find(|entity| entity.name == "app")
                .expect("app");
            let mut attributes = app.attributes.clone().unwrap_or_else(|| json!({}));
            let v2 = attributes
                .get_mut("v2")
                .and_then(Value::as_object_mut)
                .expect("v2");
            let slots = v2
                .get_mut("slots")
                .and_then(Value::as_array_mut)
                .expect("slots");
            slots.push(json!({
                "predicate": "setting",
                "key": "owner",
                "value": "ada",
                "status": "decided",
                "valid_from": "2026-04-10",
                "valid_until": Value::Null,
                "ended_by": Value::Null,
                "alpha": 1.0,
                "beta": 1.0,
                "self_reinforcements": 0,
                "evidence": [{
                    "session": "other",
                    "turn": 0,
                    "quote": "owner is ada",
                    "role": "user"
                }]
            }));
            gm.db()
                .query("UPDATE type::record($id) SET attributes = $attributes")
                .bind(("id", app.id_string()))
                .bind(("attributes", attributes))
                .await?
                .check()?;
            Ok(())
        }
    }

    #[cfg(feature = "embedded")]
    async fn slots_of(gm: &GraphMemory, name: &str) -> Vec<Value> {
        let entities = crud::list_entities(gm.db(), None).await.unwrap();
        let entity = entities
            .iter()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        entity.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[cfg(feature = "embedded")]
    async fn live_uses(gm: &GraphMemory) -> Vec<String> {
        let mut response = gm
            .db()
            .query(
                "SELECT out, valid_until FROM relates_to WHERE rel_type = 'uses' AND valid_until = NONE",
            )
            .await
            .unwrap();
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        rows.iter()
            .map(|row| record_id(row.get("out").unwrap_or(&Value::Null)))
            .collect()
    }

    #[cfg(feature = "embedded")]
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ClosedUse {
        until: String,
        ended_by: Option<String>,
    }

    #[cfg(feature = "embedded")]
    async fn closed_uses(gm: &GraphMemory, status: &str) -> Vec<ClosedUse> {
        let mut response = gm
            .db()
            .query("SELECT status, valid_until, ended_by FROM relates_to WHERE rel_type = 'uses'")
            .await
            .unwrap();
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        let mut closed = Vec::new();
        for row in rows {
            if row.get("status").and_then(Value::as_str) != Some(status) {
                continue;
            }
            let until = row
                .get("valid_until")
                .map(ToString::to_string)
                .unwrap_or_default();
            if until.is_empty() || until == "null" {
                continue;
            }
            closed.push(ClosedUse {
                until,
                ended_by: row
                    .get("ended_by")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        closed.sort_by(|left, right| left.ended_by.cmp(&right.ended_by));
        closed
    }

    #[cfg(feature = "embedded")]
    async fn edge_beliefs(gm: &GraphMemory) -> Vec<(String, String)> {
        let mut response = gm
            .db()
            .query("SELECT alpha, beta, status FROM relates_to WHERE rel_type = 'uses'")
            .await
            .unwrap();
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        let mut beliefs = rows
            .iter()
            .filter(|row| row.get("status").and_then(Value::as_str) == Some("decided"))
            .map(|row| {
                (
                    row.get("alpha")
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    row.get("beta").map(ToString::to_string).unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        beliefs.sort();
        beliefs
    }
}
