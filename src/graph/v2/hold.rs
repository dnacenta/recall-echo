// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolve a contradiction that was stored pending.
//!
//! A later session confirms, drops, or leaves the pair for the consolidation
//! batch. This pass does not call the provider.

use serde_json::{json, Value};
use surrealdb::method::Transaction;

use super::super::store::Db;

use super::super::crud;
use super::super::error::GraphError;
use super::super::types::{IngestionReport, Retirement};
use super::super::{deserialize_take, GraphMemory};
use super::known_match::{departure_near, mention, mentioned_with_cue};
use super::schema::{normalise, Fact};
use super::write::{
    is_pending, keys_agree, keys_match, session_midnight, stamp_slot, stored_role, values_equal,
    WriteCtx, CLOSE_EDGE,
};

/// A pending pair the judge still has to see.
#[derive(Clone)]
pub(crate) struct OpenHold {
    pub subject: String,
    pub pred: String,
    pub key: String,
    pub new_value: String,
    pub old_value: String,
    pub new_ref: String,
    pub old_ref: String,
}

/// Confirm, drop, or return the pairs that stay pending.
///
/// A store error is returned. The caller counts it and the session still commits.
pub(crate) async fn settle(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
    report: &mut IngestionReport,
) -> Result<Vec<OpenHold>, GraphError> {
    if !any_pending(gm).await? {
        return Ok(Vec::new());
    }
    #[cfg(test)]
    FULL_LOADS.with(|loads| loads.set(loads.get().saturating_add(1)));
    let entities = load_entities(gm).await?;
    let mut open = Vec::new();
    for entity in &entities {
        let Some(slots) = slots_of(entity) else {
            continue;
        };
        for (index, slot) in slots.iter().enumerate() {
            if !is_pending(slot) || closed(slot.get("valid_until")) {
                continue;
            }
            if since(slot) == Some(ctx.session_id) {
                continue;
            }
            let Some(old_ref) = conflicts(slot) else {
                report.hold.errors += 1;
                continue;
            };
            let new_ref = format!("{}#{index}", entity.id_string());
            let new_value = text_at(slot, "value");
            let old_value = match value_at(gm, old_ref).await {
                Ok(Some(value)) => value,
                Ok(None) => {
                    report.hold.errors += 1;
                    continue;
                }
                Err(err) => {
                    eprintln!("hold error: {err}");
                    report.hold.errors += 1;
                    continue;
                }
            };
            let subject = entity.name.clone();
            let pred = text_at(slot, "predicate");
            let key = text_at(slot, "key");
            consider(
                gm,
                ctx,
                facts,
                report,
                &mut open,
                &OpenHold {
                    subject,
                    pred,
                    key,
                    new_value,
                    old_value,
                    new_ref,
                    old_ref: old_ref.to_string(),
                },
            )
            .await;
        }
    }
    let edges = match pending_edges(gm).await {
        Ok(edges) => edges,
        Err(err) => {
            eprintln!("hold error: {err}");
            report.hold.errors += 1;
            return Ok(open);
        }
    };
    for edge in edges {
        if since(&edge) == Some(ctx.session_id) || closed(edge.get("valid_until")) {
            continue;
        }
        let Some(old_ref) = conflicts(&edge) else {
            report.hold.errors += 1;
            continue;
        };
        let new_ref = id_text(edge.get("id"));
        if new_ref.is_empty() {
            report.hold.errors += 1;
            continue;
        }
        let new_value = match value_at(gm, &new_ref).await {
            Ok(Some(value)) => value,
            Ok(None) => {
                report.hold.errors += 1;
                continue;
            }
            Err(err) => {
                eprintln!("hold error: {err}");
                report.hold.errors += 1;
                continue;
            }
        };
        let old_value = match value_at(gm, old_ref).await {
            Ok(Some(value)) => value,
            Ok(None) => {
                report.hold.errors += 1;
                continue;
            }
            Err(err) => {
                eprintln!("hold error: {err}");
                report.hold.errors += 1;
                continue;
            }
        };
        let subject = match name_of(gm, &id_text(edge.get("in"))).await {
            Ok(subject) => subject,
            Err(err) => {
                eprintln!("hold error: {err}");
                report.hold.errors += 1;
                continue;
            }
        };
        let pred = text_at(&edge, "rel_type");
        consider(
            gm,
            ctx,
            facts,
            report,
            &mut open,
            &OpenHold {
                subject,
                pred,
                key: String::new(),
                new_value,
                old_value,
                new_ref,
                old_ref: old_ref.to_string(),
            },
        )
        .await;
    }
    Ok(open)
}

async fn consider(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
    report: &mut IngestionReport,
    open: &mut Vec<OpenHold>,
    hold: &OpenHold,
) {
    let subject = hold.subject.as_str();
    let pred = hold.pred.as_str();
    let key = hold.key.as_str();
    let new_value = hold.new_value.as_str();
    let old_value = hold.old_value.as_str();
    let new_ref = hold.new_ref.as_str();
    let old_ref = hold.old_ref.as_str();
    let until = session_midnight(ctx.date);
    if confirms(ctx, facts, old_value, new_value) {
        let Some(until) = until else {
            report.hold.errors += 1;
            return;
        };
        let ended = format!("{}:superseded", ctx.session_id);
        // Decide before writing. Clear the pending row first so a failed
        // stamp cannot leave the slot with no live value.
        let already = noted(ref_already_closed(gm, old_ref, old_value).await);
        let occupied = if already {
            match other_live(gm, new_ref, pred, key, new_value).await {
                Ok(occupied) => occupied,
                Err(err) => {
                    eprintln!("hold error: {err}");
                    report.hold.errors += 1;
                    return;
                }
            }
        } else {
            false
        };
        if occupied {
            let dropped = format!("{}:hold_dropped", ctx.session_id);
            if noted(stamp_ref(gm, new_ref, &until, &dropped, new_value).await) {
                report.hold.holds_dropped += 1;
            } else {
                report.hold.errors += 1;
            }
            return;
        }
        let removed = match take_pending(gm, new_ref).await {
            Ok(Some(value)) => value,
            Ok(None) => {
                report.hold.errors += 1;
                return;
            }
            Err(err) => {
                eprintln!("hold error: {err}");
                report.hold.errors += 1;
                return;
            }
        };
        let stamped = if already {
            false
        } else {
            noted(stamp_ref(gm, old_ref, &until, &ended, old_value).await)
        };
        if !already && !stamped {
            // The old row is still live. Put the hold back so both values
            // are not live with nothing left to settle.
            if let Err(err) = restore_pending(gm, new_ref, &removed).await {
                eprintln!("hold error: {err}");
            }
            report.hold.errors += 1;
            return;
        }
        report.hold.holds_confirmed += 1;
        if stamped {
            report.retirements.push(Retirement {
                reason: "superseded".into(),
                subject: subject.to_string(),
                predicate: pred.to_string(),
                key: key.to_string(),
                old_value: old_value.to_string(),
                new_value: new_value.to_string(),
            });
        }
        return;
    }
    if reasserts(facts, subject, pred, key, old_value) {
        let Some(until) = until else {
            report.hold.errors += 1;
            return;
        };
        let ended = format!("{}:hold_dropped", ctx.session_id);
        if noted(stamp_ref(gm, new_ref, &until, &ended, new_value).await) {
            report.hold.holds_dropped += 1;
        } else {
            report.hold.errors += 1;
        }
        return;
    }
    open.push(hold.clone());
}

/// Remove `pending` and return the object that was stored there.
pub(crate) async fn take_pending(
    gm: &GraphMemory,
    reference: &str,
) -> Result<Option<Value>, GraphError> {
    if let Some((entity, index)) = split_slot(reference) {
        entity_slot(entity)?;
        let mut removed = None;
        let changed = edit_slot(gm, entity, index, |slot| {
            let Some(object) = slot.as_object_mut() else {
                return false;
            };
            removed = object.remove("pending");
            removed.is_some()
        })
        .await?;
        return Ok(changed.then_some(removed).flatten());
    }
    edge_record(reference)?;
    let mut response = gm
        .db()
        .query("UPDATE type::record($id) SET pending = NONE WHERE pending != NONE RETURN BEFORE")
        .bind(("id", reference.to_string()))
        .await?;
    let cleared: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(cleared.into_iter().next().and_then(|row| {
        let pending = row.get("pending").cloned()?;
        pending.is_object().then_some(pending)
    }))
}

/// Put a removed `pending` object back on a row that is still live.
pub(crate) async fn restore_pending(
    gm: &GraphMemory,
    reference: &str,
    pending: &Value,
) -> Result<bool, GraphError> {
    if !pending.is_object() {
        return Ok(false);
    }
    if let Some((entity, index)) = split_slot(reference) {
        entity_slot(entity)?;
        return edit_slot(gm, entity, index, |slot| {
            let Some(object) = slot.as_object_mut() else {
                return false;
            };
            if object.get("pending").is_some_and(Value::is_object) {
                return false;
            }
            if crud::edge_already_closed(object.get("valid_until")) {
                return false;
            }
            object.insert("pending".to_string(), pending.clone());
            true
        })
        .await;
    }
    edge_record(reference)?;
    let mut response = gm
        .db()
        .query(
            "UPDATE type::record($id) SET pending = $pending WHERE pending = NONE AND valid_until = NONE RETURN AFTER",
        )
        .bind(("id", reference.to_string()))
        .bind(("pending", pending.clone()))
        .await?;
    let restored: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(!restored.is_empty())
}

/// Another live, non-pending value already holds this slot.
///
/// Slots match subject, predicate, and key. An edge occupies the slot only
/// when another live edge has the same type and the same role. A pending
/// edge with no role does not treat other edges of that type as occupants.
async fn other_live(
    gm: &GraphMemory,
    new_ref: &str,
    pred: &str,
    key: &str,
    new_value: &str,
) -> Result<bool, GraphError> {
    if let Some((entity, index)) = split_slot(new_ref) {
        return other_slot(gm, entity, index, pred, key, new_value).await;
    }
    other_edge(gm, new_ref, pred).await
}

async fn other_slot(
    gm: &GraphMemory,
    entity: &str,
    index: usize,
    pred: &str,
    key: &str,
    _new_value: &str,
) -> Result<bool, GraphError> {
    let Some(slots) = attribute_row(gm, entity).await?.as_ref().and_then(slots_in) else {
        return Ok(false);
    };
    Ok(slots.iter().enumerate().any(|(at, slot)| {
        at != index
            && !closed(slot.get("valid_until"))
            && !is_pending(slot)
            && text_at(slot, "predicate") == pred
            && keys_agree(&text_at(slot, "key"), key)
    }))
}

async fn other_edge(gm: &GraphMemory, edge_id: &str, pred: &str) -> Result<bool, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT in, description FROM type::record($id)")
        .bind(("id", edge_id.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    let Some(mine) = rows.into_iter().next() else {
        return Ok(false);
    };
    let from = id_text(mine.get("in"));
    let Some(role) = stored_role(mine.get("description")) else {
        return Ok(false);
    };
    if from.is_empty() {
        return Ok(false);
    }
    let mut response = gm
        .db()
        .query(
            "SELECT id, pending, valid_until, description FROM type::record($id)->(relates_to WHERE valid_until = NONE AND rel_type = $rel)",
        )
        .bind(("id", from))
        .bind(("rel", pred.to_string()))
        .await?;
    let edges: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(edges.iter().any(|edge| {
        id_text(edge.get("id")) != edge_id
            && !is_pending(edge)
            && !closed(edge.get("valid_until"))
            && stored_role(edge.get("description")).as_deref() == Some(role.as_str())
    }))
}

async fn ref_already_closed(
    gm: &GraphMemory,
    reference: &str,
    expect: &str,
) -> Result<bool, GraphError> {
    if let Some((entity, index)) = split_slot(reference) {
        let Some(slot) = attribute_row(gm, entity)
            .await?
            .as_ref()
            .and_then(slots_in)
            .and_then(|slots| slots.get(index).cloned())
        else {
            return Ok(false);
        };
        let same = slot.get("value").and_then(Value::as_str) == Some(expect);
        return Ok(same && crud::edge_already_closed(slot.get("valid_until")));
    }
    let mut response = gm
        .db()
        .query("SELECT valid_until FROM type::record($id)")
        .bind(("id", reference.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(rows
        .into_iter()
        .next()
        .is_some_and(|row| crud::edge_already_closed(row.get("valid_until"))))
}

async fn stamp_ref(
    gm: &GraphMemory,
    reference: &str,
    until: &str,
    ended: &str,
    expect: &str,
) -> Result<bool, GraphError> {
    if let Some((entity, index)) = split_slot(reference) {
        entity_slot(entity)?;
        return edit_slot(gm, entity, index, |slot| {
            slot.get("value").and_then(Value::as_str) == Some(expect)
                && stamp_slot(slot, until, ended)
        })
        .await;
    }
    edge_record(reference)?;
    let mut response = gm
        .db()
        .query(CLOSE_EDGE)
        .bind(("id", reference.to_string()))
        .bind(("valid_until", until.to_string()))
        .bind(("ended_by", ended.to_string()))
        .await?;
    let closed: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(!closed.is_empty())
}

async fn edit_slot(
    gm: &GraphMemory,
    entity: &str,
    index: usize,
    mut edit: impl FnMut(&mut Value) -> bool,
) -> Result<bool, GraphError> {
    let mut attempt = 0u8;
    loop {
        match edit_slot_once(gm, entity, index, &mut edit).await {
            Ok(changed) => return Ok(changed),
            Err(err) if attempt == 0 && hold_conflict(&err) => {
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Read and write one entity's attributes in a single transaction.
/// A concurrent ingest that commits the same record conflicts, and the
/// caller reads again instead of overwriting that commit.
async fn edit_slot_once(
    gm: &GraphMemory,
    entity: &str,
    index: usize,
    edit: &mut impl FnMut(&mut Value) -> bool,
) -> Result<bool, GraphError> {
    entity_slot(entity)?;
    let tx = gm.db().clone().begin().await?;
    let changed = apply_slot_edit(&tx, entity, index, edit).await;
    match changed {
        Ok(true) => {
            tx.commit().await?;
            Ok(true)
        }
        Ok(false) => {
            let _ = tx.cancel().await;
            Ok(false)
        }
        Err(err) => {
            let _ = tx.cancel().await;
            Err(err)
        }
    }
}

async fn apply_slot_edit(
    tx: &Transaction<Db>,
    entity: &str,
    index: usize,
    edit: &mut impl FnMut(&mut Value) -> bool,
) -> Result<bool, GraphError> {
    let mut response = tx
        .query("SELECT attributes FROM type::record($id)")
        .bind(("id", entity.to_string()))
        .await?;
    let found: Vec<Value> = deserialize_take(&mut response, 0)?;
    let Some(mut attributes) = found
        .into_iter()
        .next()
        .and_then(|row| row.get("attributes").cloned())
    else {
        return Ok(false);
    };
    let Some(slot) = attributes
        .get_mut("v2")
        .and_then(Value::as_object_mut)
        .and_then(|v2| v2.get_mut("slots"))
        .and_then(Value::as_array_mut)
        .and_then(|slots| slots.get_mut(index))
    else {
        return Ok(false);
    };
    if !edit(slot) {
        return Ok(false);
    }
    tx.query("UPDATE type::record($id) SET attributes = $attributes")
        .bind(("id", entity.to_string()))
        .bind(("attributes", attributes))
        .await?
        .check()?;
    Ok(true)
}

fn hold_conflict(err: &GraphError) -> bool {
    match err {
        GraphError::Db(inner) => {
            let message = inner.to_string().to_lowercase();
            message.contains("conflict") || message.contains("please retry")
        }
        _ => false,
    }
}

fn confirms(ctx: &WriteCtx<'_>, facts: &[Fact], old: &str, new_value: &str) -> bool {
    // The new value, the old value, and the departure cue have to share a turn.
    if !new_value.is_empty()
        && ctx
            .turns
            .iter()
            .any(|turn| mention(new_value, &turn.text) && departs(&turn.text, old, ""))
    {
        return true;
    }
    facts.iter().any(|fact| {
        fact.status == "decided"
            && values_equal(&fact.object_name, new_value)
            && !fact.replaces.trim().is_empty()
            && (values_equal(&fact.replaces, old) || mention(old, &fact.replaces))
            && departs(cited(ctx, fact), old, &fact.replaces)
    })
}

fn reasserts(facts: &[Fact], subject: &str, pred: &str, key: &str, old: &str) -> bool {
    facts.iter().any(|fact| {
        fact.status == "decided"
            && normalise(&fact.subject_name) == normalise(subject)
            && fact.predicate.as_str() == pred
            && values_equal(&fact.object_name, old)
            && (key.is_empty() || fact.key.is_empty() || keys_match(&fact.key, key))
    })
}

pub(crate) fn departs(turn: &str, old: &str, replaces: &str) -> bool {
    let hit = |value: &str| {
        !value.is_empty() && (mentioned_with_cue(value, turn) || departure_near(value, turn))
    };
    hit(old) || hit(replaces)
}

fn cited<'a>(ctx: &'a WriteCtx<'a>, fact: &'a Fact) -> &'a str {
    ctx.turns
        .iter()
        .find(|turn| turn.index == fact.turn)
        .map(|turn| turn.text.as_str())
        .unwrap_or(fact.quote.as_str())
}

/// A store failure counts as the boolean `false` after the cause is printed.
fn noted(result: Result<bool, GraphError>) -> bool {
    result.unwrap_or_else(|err| {
        eprintln!("hold error: {err}");
        false
    })
}

#[cfg(test)]
thread_local! {
    static FULL_LOADS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// How many times settle loaded entity rows on this thread.
#[cfg(test)]
pub(crate) fn full_loads() -> u32 {
    FULL_LOADS.with(std::cell::Cell::get)
}

/// Zero [`full_loads`] before the next assertion.
#[cfg(test)]
pub(crate) fn reset_full_loads() {
    FULL_LOADS.with(|loads| loads.set(0));
}

async fn any_pending(gm: &GraphMemory) -> Result<bool, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT id FROM relates_to WHERE pending != NONE LIMIT 1")
        .await?;
    let edges: Vec<Value> = deserialize_take(&mut response, 0)?;
    if !edges.is_empty() {
        return Ok(true);
    }
    let mut response = gm
        .db()
        .query(
            "SELECT id FROM entity WHERE count(attributes.v2.slots[WHERE pending != NONE]) > 0 LIMIT 1",
        )
        .await
        ?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(!rows.is_empty())
}

async fn load_entities(gm: &GraphMemory) -> Result<Vec<super::super::types::Entity>, GraphError> {
    let mut response = gm.db().query("SELECT * OMIT embedding FROM entity").await?;
    deserialize_take(&mut response, 0)
}

async fn attribute_row(gm: &GraphMemory, entity: &str) -> Result<Option<Value>, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT attributes FROM type::record($id)")
        .bind(("id", entity.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(rows.into_iter().next())
}

fn slots_in(row: &Value) -> Option<Vec<Value>> {
    row.get("attributes").and_then(slot_list)
}

fn slots_of(entity: &super::super::types::Entity) -> Option<Vec<Value>> {
    entity.attributes.as_ref().and_then(slot_list)
}

fn slot_list(attributes: &Value) -> Option<Vec<Value>> {
    attributes
        .get("v2")
        .and_then(|v2| v2.get("slots"))
        .and_then(Value::as_array)
        .cloned()
}

async fn pending_edges(gm: &GraphMemory) -> Result<Vec<Value>, GraphError> {
    let mut response = gm
        .db()
        .query(
            "SELECT id, in, out, rel_type, pending, status, valid_until FROM relates_to WHERE pending != NONE AND valid_until = NONE",
        )
        .await
        ?;
    deserialize_take(&mut response, 0)
}

async fn value_at(gm: &GraphMemory, reference: &str) -> Result<Option<String>, GraphError> {
    if let Some((entity, index)) = split_slot(reference) {
        return Ok(attribute_row(gm, entity)
            .await?
            .as_ref()
            .and_then(slots_in)
            .and_then(|slots| slots.get(index).cloned())
            .and_then(|slot| {
                slot.get("value")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }));
    }
    let mut response = gm
        .db()
        .query("SELECT out FROM type::record($id)")
        .bind(("id", reference.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    let Some(out) = rows.into_iter().next().map(|row| id_text(row.get("out"))) else {
        return Ok(None);
    };
    name_of(gm, &out).await.map(Some)
}

async fn name_of(gm: &GraphMemory, id: &str) -> Result<String, GraphError> {
    if id.is_empty() {
        return Ok(String::new());
    }
    let mut response = gm
        .db()
        .query("SELECT name FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    Ok(rows
        .into_iter()
        .find_map(|row| row.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default())
}

fn split_slot(reference: &str) -> Option<(&str, usize)> {
    let (entity, index) = reference.rsplit_once('#')?;
    Some((entity, index.parse().ok()?))
}

fn entity_slot(entity: &str) -> Result<(), GraphError> {
    if entity.starts_with("entity:") {
        Ok(())
    } else {
        Err(GraphError::Parse(
            "hold slot reference is not an entity record".into(),
        ))
    }
}

fn edge_record(reference: &str) -> Result<(), GraphError> {
    if reference.starts_with("relates_to:") {
        Ok(())
    } else {
        Err(GraphError::Parse(
            "hold edge reference is not a relates_to record".into(),
        ))
    }
}

fn conflicts(value: &Value) -> Option<&str> {
    value
        .get("pending")
        .and_then(|pending| pending.get("conflicts"))
        .and_then(Value::as_str)
}

fn since(value: &Value) -> Option<&str> {
    value
        .get("pending")
        .and_then(|pending| pending.get("since"))
        .and_then(Value::as_str)
}

fn text_at(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn closed(valid_until: Option<&Value>) -> bool {
    crud::edge_already_closed(valid_until)
}

pub(crate) fn id_text(id: Option<&Value>) -> String {
    match id {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// The object stored on a new row that conflicts with `old`.
#[must_use]
pub(crate) fn pending_value(session: &str, old: &str) -> Value {
    json!({"since": session, "conflicts": old})
}
