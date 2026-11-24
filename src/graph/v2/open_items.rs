// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Open items: issue records whose live status is `open`.
//!
//! One slot read over issue entities, for the session-start overview and
//! `graph open-items`. It only reads, and it never calls a model.

use serde_json::Value;
use surrealdb::Surreal;

use super::super::deserialize_take;
use super::super::edge_view::record_id;
use super::super::error::GraphError;
use super::super::inspect::OpenItem;
use super::super::store::Db;
use super::write::is_pending;

const ISSUE_SQL: &str =
    "SELECT id, name, attributes.v2.slots AS slots FROM entity WHERE attributes.v2.kind = 'issue'";

/// Live decided `issue_status = open` rows, oldest first.
///
/// Oldest is the date the status was recorded, then the session and turn of
/// its first evidence. `include_closed` adds the live `fixed` rows. A pending
/// row is never listed.
pub(crate) async fn open_items(
    db: &Surreal<Db>,
    include_closed: bool,
) -> Result<Vec<OpenItem>, GraphError> {
    let mut response = db.query(ISSUE_SQL).await?;
    let rows: Vec<Value> = deserialize_take(&mut response, 0)?;
    let mut items: Vec<OpenItem> = rows
        .iter()
        .flat_map(|row| row_items(row, include_closed))
        .collect();
    items.sort_by(|left, right| {
        left.since
            .cmp(&right.since)
            .then_with(|| left.session.cmp(&right.session))
            .then_with(|| left.turn.cmp(&right.turn))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(items)
}

fn row_items(row: &Value, include_closed: bool) -> Vec<OpenItem> {
    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let id = row.get("id").map(record_id).unwrap_or_default();
    let Some(slots) = row.get("slots").and_then(Value::as_array) else {
        return Vec::new();
    };
    slots
        .iter()
        .filter_map(|slot| {
            let status = listed_status(slot, include_closed)?;
            let first = slot
                .get("evidence")
                .and_then(Value::as_array)
                .and_then(|items| items.first());
            Some(OpenItem {
                id: id.clone(),
                name: name.to_string(),
                status: status.to_string(),
                since: text(slot.get("valid_from")),
                session: text(first.and_then(|item| item.get("session"))),
                turn: first
                    .and_then(|item| item.get("turn"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// `open`, or `fixed` when closed items are asked for, on a live decided row.
fn listed_status(slot: &Value, include_closed: bool) -> Option<&'static str> {
    let live = slot.get("valid_until").is_none_or(Value::is_null);
    if !live
        || is_pending(slot)
        || slot.get("predicate").and_then(Value::as_str) != Some("issue_status")
        || slot.get("status").and_then(Value::as_str) != Some("decided")
    {
        return None;
    }
    let value = slot.get("value").and_then(Value::as_str)?;
    match value.trim().to_lowercase().as_str() {
        "open" => Some("open"),
        "fixed" if include_closed => Some("fixed"),
        _ => None,
    }
}

fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or("").to_string()
}

#[cfg(all(test, feature = "embedded"))]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::graph::GraphMemory;

    fn status(value: &str, since: &str, session: &str, turn: u64) -> Value {
        json!({
            "predicate": "issue_status",
            "key": "",
            "value": value,
            "status": "decided",
            "provenance": "self",
            "valid_from": since,
            "valid_until": null,
            "ended_by": null,
            "evidence": [{"session": session, "turn": turn, "quote": "q", "role": "self"}],
        })
    }

    fn closed(mut slot: Value, until: &str, session: &str) -> Value {
        slot["valid_until"] = json!(until);
        slot["ended_by"] = json!(format!("{session}:superseded"));
        slot
    }

    async fn entity(gm: &GraphMemory, id: &str, name: &str, kind: &str, slots: Vec<Value>) {
        gm.db()
            .query(format!(
                "CREATE entity:{id} SET name = $name, entity_type = $kind, abstract = '', overview = '', attributes = $attributes"
            ))
            .bind(("name", name.to_string()))
            .bind(("kind", kind.to_string()))
            .bind(("attributes", json!({"v2": {"kind": kind, "slots": slots}})))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    /// Two live opens, one closed item, one pending open, and an open-looking
    /// value on another kind and another predicate.
    async fn seeded() -> (tempfile::TempDir, GraphMemory) {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        entity(
            &gm,
            "search",
            "search times out",
            "issue",
            vec![status("open", "2026-03-02", "s2", 1)],
        )
        .await;
        entity(
            &gm,
            "login",
            "login fails",
            "issue",
            vec![status("open", "2026-03-01", "s1", 3)],
        )
        .await;
        entity(
            &gm,
            "export",
            "export drops rows",
            "issue",
            vec![
                closed(
                    status("open", "2026-03-01", "s1", 1),
                    "2026-03-03T00:00:00Z",
                    "s3",
                ),
                status("fixed", "2026-03-03", "s3", 2),
            ],
        )
        .await;
        let mut pending = status("open", "2026-02-01", "s0", 1);
        pending["pending"] = json!({"since": "s0", "conflicts": "entity:cache#0"});
        let mut setting = status("open", "2026-02-01", "s0", 1);
        setting["predicate"] = json!("setting");
        entity(
            &gm,
            "cache",
            "cache is stale",
            "issue",
            vec![pending, setting],
        )
        .await;
        entity(
            &gm,
            "app",
            "app",
            "project",
            vec![status("open", "2026-01-01", "s0", 0)],
        )
        .await;
        (tmp, gm)
    }

    fn listed(items: &[OpenItem]) -> Vec<(&str, &str, &str, &str, u64)> {
        items
            .iter()
            .map(|item| {
                (
                    item.name.as_str(),
                    item.status.as_str(),
                    item.since.as_str(),
                    item.session.as_str(),
                    item.turn,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn only_live_opens_are_listed_oldest_first() {
        let (_tmp, gm) = seeded().await;
        let items = open_items(gm.db(), false).await.unwrap();
        assert_eq!(
            listed(&items),
            [
                ("login fails", "open", "2026-03-01", "s1", 3),
                ("search times out", "open", "2026-03-02", "s2", 1),
            ]
        );
        assert_eq!(items[0].id, "entity:login");
    }

    #[tokio::test]
    async fn closed_items_are_added_only_when_asked() {
        let (_tmp, gm) = seeded().await;
        let items = open_items(gm.db(), true).await.unwrap();
        assert_eq!(
            listed(&items),
            [
                ("login fails", "open", "2026-03-01", "s1", 3),
                ("search times out", "open", "2026-03-02", "s2", 1),
                ("export drops rows", "fixed", "2026-03-03", "s3", 2),
            ]
        );
    }

    #[tokio::test]
    async fn an_empty_store_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        assert!(open_items(gm.db(), true).await.unwrap().is_empty());
    }
}
