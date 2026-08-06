// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Relationships as a person reads them.
//!
//! A stored [`Relationship`] names its endpoints by record id and reports one
//! number. Neither is what a human needs to decide whether the memory is
//! right: they need the two entity names, and they need to know whether the
//! number rests on thirty independent observations or on the agent having said
//! the same thing thirty times.
//!
//! [`EdgeView`] is that shape, and it is shared by both human-facing surfaces —
//! correction ([`super::correct`]) and inspection ([`super::inspect`]) — so the
//! two never disagree about how an edge reads.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use surrealdb::Surreal;

use super::error::GraphError;
use super::store::Db;
use super::types::Relationship;

/// One relationship, with its endpoints resolved and its evidence exposed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeView {
    /// Record id, so a caller can act on exactly this edge.
    pub id: String,
    /// Name of the entity the edge points from.
    pub from: String,
    /// Name of the entity the edge points to.
    pub to: String,
    pub rel_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Posterior mean — what retrieval scores on.
    pub confidence: f64,
    /// Total evidence weight behind `confidence` (the Beta concentration).
    /// The difference between "believed at 0.9" and "believed at 0.9 for good
    /// reason".
    pub evidence: f64,
    /// Corroborations the agent produced itself, counted but never laundered
    /// into `confidence`. Non-zero is the "I might be talking to myself"
    /// signal, and it is exactly what a human should see.
    pub self_reinforcements: i64,
    /// A later relationship replaced this one.
    pub superseded: bool,
}

impl EdgeView {
    /// The edge in the notation every recall-echo surface writes it in.
    #[must_use]
    pub fn arrow(&self) -> String {
        format!("{} —[{}]→ {}", self.from, self.rel_type, self.to)
    }

    /// The view of `relationship` with the two names already resolved.
    #[must_use]
    pub fn of(relationship: &Relationship, from: String, to: String) -> Self {
        let evidence = relationship.evidence();
        Self {
            id: relationship.id_string(),
            from,
            to,
            rel_type: relationship.rel_type.clone(),
            description: relationship.description.clone(),
            confidence: relationship.confidence,
            evidence: evidence.concentration(),
            self_reinforcements: relationship.self_reinforcements.unwrap_or(0).max(0),
            superseded: relationship.valid_until.is_some(),
        }
    }
}

/// The record id of a stored link, as a string.
#[must_use]
pub fn record_id(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(id) => id.clone(),
        other => other.to_string(),
    }
}

/// Resolves record ids to entity names, once per id.
///
/// Rendering the edges of one entity asks for the same neighbour repeatedly;
/// rendering a whole overview asks for the same hub over and over. One lookup
/// each is enough.
#[derive(Debug, Default)]
pub struct NameCache {
    names: HashMap<String, String>,
}

impl NameCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entity's name, or its record id when no entity is stored under it.
    ///
    /// A dangling edge is still worth showing — it is precisely the sort of
    /// thing a person inspecting their memory should be able to see and delete.
    pub async fn name_of(&mut self, db: &Surreal<Db>, id: &str) -> Result<String, GraphError> {
        if let Some(name) = self.names.get(id) {
            return Ok(name.clone());
        }
        let name = super::crud::get_entity_summary(db, id)
            .await?
            .map_or_else(|| id.to_string(), |summary| summary.name);
        self.names.insert(id.to_string(), name.clone());
        Ok(name)
    }
}

/// Render `relationships` for a human, sharing one name cache.
pub async fn views(
    db: &Surreal<Db>,
    cache: &mut NameCache,
    relationships: &[Relationship],
) -> Result<Vec<EdgeView>, GraphError> {
    let mut views = Vec::with_capacity(relationships.len());
    for relationship in relationships {
        let from = cache.name_of(db, &record_id(&relationship.from_id)).await?;
        let to = cache.name_of(db, &record_id(&relationship.to_id)).await?;
        views.push(EdgeView::of(relationship, from, to));
    }
    Ok(views)
}

/// Every relationship of an entity that has not been superseded.
pub async fn live_edges_of(
    db: &Surreal<Db>,
    entity_id: &str,
) -> Result<Vec<Relationship>, GraphError> {
    let mut response = db
        .query(
            r#"SELECT * FROM relates_to
               WHERE (in = type::record($id) OR out = type::record($id))
                 AND valid_until IS NONE
               ORDER BY confidence DESC"#,
        )
        .bind(("id", entity_id.to_string()))
        .await?;
    super::deserialize_take(&mut response, 0)
}

/// Every relationship of an entity, superseded ones included.
///
/// What deleting the entity would actually take with it.
pub async fn all_edges_of(
    db: &Surreal<Db>,
    entity_id: &str,
) -> Result<Vec<Relationship>, GraphError> {
    let mut response = db
        .query(
            r#"SELECT * FROM relates_to
               WHERE in = type::record($id) OR out = type::record($id)
               ORDER BY confidence DESC"#,
        )
        .bind(("id", entity_id.to_string()))
        .await?;
    super::deserialize_take(&mut response, 0)
}

/// Live relationships between two entities, in either direction.
///
/// Direction is not required to match what the user typed: someone correcting
/// a memory says what they mean, not which way round the graph stored it.
pub async fn live_edges_between(
    db: &Surreal<Db>,
    one: &str,
    other: &str,
) -> Result<Vec<Relationship>, GraphError> {
    let mut response = db
        .query(
            r#"SELECT * FROM relates_to
               WHERE valid_until IS NONE
                 AND ((in = type::record($one) AND out = type::record($other))
                   OR (in = type::record($other) AND out = type::record($one)))
               ORDER BY confidence DESC"#,
        )
        .bind(("one", one.to_string()))
        .bind(("other", other.to_string()))
        .await?;
    super::deserialize_take(&mut response, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn relationship(confidence: f64, self_reinforcements: Option<i64>) -> Relationship {
        Relationship {
            id: json!("relates_to:abc"),
            from_id: json!("entity:rust"),
            to_id: json!("entity:cargo"),
            rel_type: "USES".into(),
            description: Some("build tool".into()),
            valid_from: json!("2026-01-01T00:00:00Z"),
            valid_until: None,
            confidence,
            alpha: Some(18.0),
            beta: Some(2.0),
            self_reinforcements,
            last_reinforced: None,
            source: Some("archive-log-007".into()),
        }
    }

    #[test]
    fn a_view_carries_the_evidence_and_not_only_the_number() {
        let view = EdgeView::of(&relationship(0.9, Some(4)), "Rust".into(), "Cargo".into());
        assert_eq!(view.arrow(), "Rust —[USES]→ Cargo");
        assert_eq!(view.confidence, 0.9);
        assert_eq!(view.evidence, 20.0);
        assert_eq!(view.self_reinforcements, 4);
        assert!(!view.superseded);
    }

    #[test]
    fn an_edge_predating_the_coherence_tally_reads_as_zero() {
        let view = EdgeView::of(&relationship(0.9, None), "Rust".into(), "Cargo".into());
        assert_eq!(view.self_reinforcements, 0);
    }

    #[test]
    fn a_hand_edited_negative_tally_is_clamped() {
        let view = EdgeView::of(&relationship(0.9, Some(-3)), "Rust".into(), "Cargo".into());
        assert_eq!(view.self_reinforcements, 0);
    }

    #[test]
    fn a_superseded_edge_says_so() {
        let mut relationship = relationship(0.9, Some(0));
        relationship.valid_until = Some(json!("2026-02-01T00:00:00Z"));
        let view = EdgeView::of(&relationship, "Rust".into(), "Cargo".into());
        assert!(view.superseded);
    }

    #[test]
    fn record_ids_survive_both_stored_shapes() {
        assert_eq!(record_id(&json!("entity:rust")), "entity:rust");
        assert_eq!(record_id(&json!(7)), "7");
    }
}
