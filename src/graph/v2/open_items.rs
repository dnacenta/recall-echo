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

    use super::super::schema::{Fact, Kind, Predicate};
    use super::*;
    use crate::bench::writes::{Claim, FactScript};
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

    /// Fresh names and exact repeats only, so the resolver is never asked.
    struct NoResolver;

    #[async_trait::async_trait]
    impl crate::graph::llm::LlmProvider for NoResolver {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            panic!("resolver called");
        }
    }

    /// An inferred issue status, the way pass 1 writes one.
    fn issue_status(subject: &str, value: &str, turn: usize, quote: &str) -> Fact {
        Fact {
            subject_name: subject.to_string(),
            subject_kind: Kind::Issue,
            predicate: Predicate::IssueStatus,
            role: None,
            key: String::new(),
            object_name: value.to_string(),
            object_kind: None,
            status: "decided".into(),
            turn,
            quote: quote.to_string(),
            confidence: 0.9,
            provenance: crate::graph::Provenance::SelfGenerated,
            replaces: String::new(),
        }
    }

    /// The fact a claim leaves in place: the replacement of a supersede,
    /// otherwise the claimed fact.
    fn written_fact(script: &FactScript, claim: &Claim, turn: usize, quote: &str) -> Fact {
        let id = claim.replacement.as_deref().unwrap_or(&claim.fact);
        let fact = script.fact(id).unwrap();
        let mut written = issue_status(&fact.subject, &fact.object, turn, quote);
        if let Some(key) = fact.relation.strip_prefix("setting:") {
            written.subject_kind = Kind::Project;
            written.predicate = Predicate::Setting;
            written.key = key.to_string();
            written.provenance = crate::graph::Provenance::User;
        }
        written
    }

    /// Write the open-loops fixture session by session. A turn that claims
    /// nothing but names an item gets the `open` pass 1 infers for a bug with
    /// no fix.
    async fn write_open_loops(gm: &GraphMemory, open_loops: bool) {
        use super::super::input::{ProjectIdentity, Turn, TurnRole};
        use super::super::write::{write_facts, WriteCtx};
        use crate::bench::writes::{load_script, Speaker};

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/open-loops/script.toml");
        let script = load_script(&path).unwrap();
        let items: Vec<&str> = script
            .facts
            .iter()
            .filter(|fact| fact.relation == "issue_status")
            .map(|fact| fact.subject.as_str())
            .collect();
        let project = ProjectIdentity {
            name: "timber".into(),
            aliases: Vec::new(),
        };
        let llm = NoResolver;
        for session in &script.sessions {
            let turns: Vec<Turn> = session
                .turns
                .iter()
                .enumerate()
                .map(|(index, turn)| Turn {
                    index,
                    role: match turn.role {
                        Speaker::User => TurnRole::User,
                        Speaker::Assistant => TurnRole::Assistant,
                    },
                    text: turn.text.clone(),
                })
                .collect();
            let mut facts = Vec::new();
            for (index, turn) in session.turns.iter().enumerate() {
                for claim in &turn.claims {
                    facts.push(written_fact(&script, claim, index, &turn.text));
                }
                if turn.claims.is_empty() {
                    if let Some(item) = items.iter().find(|item| turn.text.contains(**item)) {
                        facts.push(issue_status(item, "open", index, &turn.text));
                    }
                }
            }
            let mut ctx = WriteCtx::new(&llm, &project, &session.id, &session.date, &turns);
            ctx.open_loops_on = open_loops;
            write_facts(gm, &ctx, &facts).await.unwrap();
        }
    }

    fn names(items: &[OpenItem]) -> Vec<(&str, &str)> {
        items
            .iter()
            .map(|item| (item.name.as_str(), item.status.as_str()))
            .collect()
    }

    /// The fixture through the writer. With the flag on, only the item never
    /// closed and the reopened one are open, oldest first.
    #[tokio::test]
    #[cfg_attr(
        not(embed_tests),
        ignore = "embeds: set RECALL_ECHO_MODELS_DIR to a seeded models dir"
    )]
    async fn the_open_loops_fixture_lists_only_the_never_closed_and_reopened_items() {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        write_open_loops(&gm, true).await;
        assert_eq!(
            names(&open_items(gm.db(), false).await.unwrap()),
            [("image cache size", "open"), ("migration guide", "open")]
        );
        assert_eq!(
            names(&open_items(gm.db(), true).await.unwrap()),
            [
                ("image cache size", "open"),
                ("flaky login test", "fixed"),
                ("backup schedule", "fixed"),
                ("migration guide", "open"),
            ]
        );

        // With the flag off, the restatement closes the fixed login test.
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        write_open_loops(&gm, false).await;
        assert_eq!(
            names(&open_items(gm.db(), false).await.unwrap()),
            [
                ("image cache size", "open"),
                ("flaky login test", "open"),
                ("migration guide", "open"),
            ]
        );
    }
}
