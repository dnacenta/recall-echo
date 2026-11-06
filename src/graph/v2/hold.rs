// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolve a contradiction that was stored pending.
//!
//! A later session confirms, drops, or leaves the pair for the consolidation
//! batch. This pass does not call the provider.

use serde_json::{json, Value};

use super::super::crud;
use super::super::error::GraphError;
use super::super::types::{IngestionReport, Retirement};
use super::super::{deserialize_take, GraphMemory};
use super::known_match::{departure_near, mention, mentioned_with_cue};
use super::schema::{normalise, Fact};
use super::write::{
    is_pending, keys_match, session_midnight, stamp_slot, values_equal, WriteCtx, CLOSE_EDGE,
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
/// A store error is `Err(())`. The caller counts it. The session still commits.
pub(crate) async fn settle(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
    report: &mut IngestionReport,
) -> Result<Vec<OpenHold>, ()> {
    let entities = crud::list_entities(gm.db(), None).await.map_err(|_| ())?;
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
                Err(()) => {
                    report.hold.errors += 1;
                    continue;
                }
            };
            let subject = entity.name.clone();
            let pred = text_at(slot, "predicate");
            let key = text_at(slot, "key");
            consider(
                gm, ctx, facts, report, &mut open, &subject, &pred, &key, &new_value, &old_value,
                &new_ref, old_ref,
            )
            .await;
        }
    }
    if let Ok(edges) = pending_edges(gm).await {
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
            let (Ok(Some(new_value)), Ok(Some(old_value)), Ok(subject)) = (
                value_at(gm, &new_ref).await,
                value_at(gm, old_ref).await,
                name_of(gm, &id_text(edge.get("in"))).await,
            ) else {
                report.hold.errors += 1;
                continue;
            };
            let pred = text_at(&edge, "rel_type");
            consider(
                gm, ctx, facts, report, &mut open, &subject, &pred, "", &new_value, &old_value,
                &new_ref, old_ref,
            )
            .await;
        }
    } else {
        report.hold.errors += 1;
    }
    Ok(open)
}

#[allow(clippy::too_many_arguments)]
async fn consider(
    gm: &GraphMemory,
    ctx: &WriteCtx<'_>,
    facts: &[Fact],
    report: &mut IngestionReport,
    open: &mut Vec<OpenHold>,
    subject: &str,
    pred: &str,
    key: &str,
    new_value: &str,
    old_value: &str,
    new_ref: &str,
    old_ref: &str,
) {
    let until = session_midnight(ctx.date);
    if confirms(ctx, facts, old_value, new_value) {
        let Some(until) = until else {
            report.hold.errors += 1;
            return;
        };
        let ended = format!("{}:superseded", ctx.session_id);
        // Decide before writing. Clear the pending row first so a failed
        // stamp cannot leave the slot with no live value.
        let already = ref_already_closed(gm, old_ref, old_value)
            .await
            .unwrap_or(false);
        let occupied = if already {
            match other_live(gm, new_ref, pred, key, new_value).await {
                Ok(occupied) => occupied,
                Err(_) => {
                    report.hold.errors += 1;
                    return;
                }
            }
        } else {
            false
        };
        if occupied {
            let dropped = format!("{}:hold_dropped", ctx.session_id);
            if stamp_ref(gm, new_ref, &until, &dropped, new_value)
                .await
                .unwrap_or(false)
            {
                report.hold.holds_dropped += 1;
            } else {
                report.hold.errors += 1;
            }
            return;
        }
        if !clear_ref(gm, new_ref).await.unwrap_or(false) {
            report.hold.errors += 1;
            return;
        }
        let stamped = if already {
            false
        } else {
            stamp_ref(gm, old_ref, &until, &ended, old_value)
                .await
                .unwrap_or(false)
        };
        if !already && !stamped {
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
        if stamp_ref(gm, new_ref, &until, &ended, new_value)
            .await
            .unwrap_or(false)
        {
            report.hold.holds_dropped += 1;
        } else {
            report.hold.errors += 1;
        }
        return;
    }
    open.push(OpenHold {
        subject: subject.to_string(),
        pred: pred.to_string(),
        key: key.to_string(),
        new_value: new_value.to_string(),
        old_value: old_value.to_string(),
        new_ref: new_ref.to_string(),
        old_ref: old_ref.to_string(),
    });
}

/// Remove `pending` so the row is live. The value and the weights stay.
pub(crate) async fn clear_ref(gm: &GraphMemory, reference: &str) -> Result<bool, GraphError> {
    if let Some((entity, index)) = split_slot(reference) {
        return edit_slot(gm, entity, index, |slot| {
            slot.as_object_mut()
                .is_some_and(|object| object.remove("pending").is_some())
        })
        .await;
    }
    gm.db()
        .query("UPDATE type::record($id) SET pending = NONE WHERE pending != NONE")
        .bind(("id", reference.to_string()))
        .await?
        .check()?;
    Ok(true)
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
    let mut response = gm
        .db()
        .query("SELECT attributes FROM type::record($id)")
        .bind(("id", entity.to_string()))
        .await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    let Some(slots) = rows.into_iter().next().and_then(|row| {
        row.get("attributes")
            .and_then(|attrs| attrs.get("v2"))
            .and_then(|v2| v2.get("slots"))
            .and_then(Value::as_array)
            .cloned()
    }) else {
        return Ok(false);
    };
    Ok(slots.iter().enumerate().any(|(at, slot)| {
        at != index
            && !closed(slot.get("valid_until"))
            && !is_pending(slot)
            && text_at(slot, "predicate") == pred
            && same_key(&text_at(slot, "key"), key)
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
    let Some(role) = edge_role(mine.get("description")) else {
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
            && edge_role(edge.get("description")).as_deref() == Some(role.as_str())
    }))
}

/// Role stored on an edge, or nothing when the edge names none.
fn edge_role(description: Option<&Value>) -> Option<String> {
    let text = description.and_then(Value::as_str).map(str::trim)?;
    if text.is_empty() {
        return None;
    }
    let role = text.strip_prefix("role=").unwrap_or(text).trim();
    if role.is_empty() {
        None
    } else {
        Some(role.to_string())
    }
}

fn same_key(left: &str, right: &str) -> bool {
    if left.is_empty() && right.is_empty() {
        true
    } else {
        keys_match(left, right)
    }
}

async fn ref_already_closed(
    gm: &GraphMemory,
    reference: &str,
    expect: &str,
) -> Result<bool, GraphError> {
    if let Some((entity, index)) = split_slot(reference) {
        let mut response = gm
            .db()
            .query("SELECT attributes FROM type::record($id)")
            .bind(("id", entity.to_string()))
            .await?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
        let Some(slot) = rows.into_iter().next().and_then(|row| {
            row.get("attributes")
                .and_then(|attrs| attrs.get("v2"))
                .and_then(|v2| v2.get("slots"))
                .and_then(Value::as_array)
                .and_then(|slots| slots.get(index))
                .cloned()
        }) else {
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
        return edit_slot(gm, entity, index, |slot| {
            slot.get("value").and_then(Value::as_str) == Some(expect)
                && stamp_slot(slot, until, ended)
        })
        .await;
    }
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
    edit: impl FnOnce(&mut Value) -> bool,
) -> Result<bool, GraphError> {
    let mut response = gm
        .db()
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
    gm.db()
        .query("UPDATE type::record($id) SET attributes = $attributes")
        .bind(("id", entity.to_string()))
        .bind(("attributes", attributes))
        .await?
        .check()?;
    Ok(true)
}

fn confirms(ctx: &WriteCtx<'_>, facts: &[Fact], old: &str, new_value: &str) -> bool {
    let text = session_text(ctx);
    if !new_value.is_empty() && mention(new_value, &text) && departs(&text, old, "") {
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

fn departs(turn: &str, old: &str, replaces: &str) -> bool {
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

fn session_text(ctx: &WriteCtx<'_>) -> String {
    let mut text = String::new();
    for turn in ctx.turns {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&turn.text);
    }
    text
}

fn slots_of(entity: &super::super::types::Entity) -> Option<Vec<Value>> {
    entity
        .attributes
        .as_ref()
        .and_then(|attrs| attrs.get("v2"))
        .and_then(|v2| v2.get("slots"))
        .and_then(Value::as_array)
        .cloned()
}

async fn pending_edges(gm: &GraphMemory) -> Result<Vec<Value>, ()> {
    let mut response = gm
        .db()
        .query(
            "SELECT id, in, out, rel_type, pending, status, valid_until FROM relates_to WHERE pending != NONE AND valid_until = NONE",
        )
        .await
        .map_err(|_| ())?;
    deserialize_take(&mut response, 0).map_err(|_| ())
}

async fn value_at(gm: &GraphMemory, reference: &str) -> Result<Option<String>, ()> {
    if let Some((entity, index)) = split_slot(reference) {
        let mut response = gm
            .db()
            .query("SELECT attributes FROM type::record($id)")
            .bind(("id", entity.to_string()))
            .await
            .map_err(|_| ())?;
        let rows: Vec<Value> = deserialize_take(&mut response, 0).map_err(|_| ())?;
        return Ok(rows.into_iter().next().and_then(|row| {
            row.get("attributes")
                .and_then(|attrs| attrs.get("v2"))
                .and_then(|v2| v2.get("slots"))
                .and_then(Value::as_array)
                .and_then(|slots| slots.get(index))
                .and_then(|slot| slot.get("value"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }));
    }
    let mut response = gm
        .db()
        .query("SELECT out FROM type::record($id)")
        .bind(("id", reference.to_string()))
        .await
        .map_err(|_| ())?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0).map_err(|_| ())?;
    let Some(out) = rows.into_iter().next().map(|row| id_text(row.get("out"))) else {
        return Ok(None);
    };
    name_of(gm, &out).await.map(Some)
}

async fn name_of(gm: &GraphMemory, id: &str) -> Result<String, ()> {
    if id.is_empty() {
        return Ok(String::new());
    }
    let mut response = gm
        .db()
        .query("SELECT name FROM type::record($id)")
        .bind(("id", id.to_string()))
        .await
        .map_err(|_| ())?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0).map_err(|_| ())?;
    Ok(rows
        .into_iter()
        .find_map(|row| row.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default())
}

fn split_slot(reference: &str) -> Option<(&str, usize)> {
    let (entity, index) = reference.rsplit_once('#')?;
    Some((entity, index.parse().ok()?))
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

fn id_text(id: Option<&Value>) -> String {
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
