// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Outcome feedback loop for adaptive entity learning.
//!
//! Tracks which graph entities contributed to session outcomes (success/partial/failure)
//! and adjusts their `utility_score` via exponential moving average.
//!
//! Phase 1 of Adaptive Entity Learning v2.

use serde::{Deserialize, Serialize};
use surrealdb::Surreal;

use super::error::GraphError;
use super::store::Db;

/// The result of a task or session outcome.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Success,
    Partial,
    Failed,
}

impl OutcomeKind {
    /// Numeric reward signal for EMA update.
    #[must_use]
    pub fn reward(self) -> f64 {
        match self {
            Self::Success => 1.0,
            Self::Partial => 0.5,
            Self::Failed => 0.0,
        }
    }
}

impl std::fmt::Display for OutcomeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Partial => write!(f, "partial"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl std::str::FromStr for OutcomeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "success" => Ok(Self::Success),
            "partial" => Ok(Self::Partial),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown outcome kind: {other}")),
        }
    }
}

/// Default utility score for new entities.
pub const DEFAULT_UTILITY: f64 = 0.5;

/// EMA alpha for entities that were retrieved AND used.
const USED_ALPHA: f64 = 0.1;

/// Smaller EMA alpha for entities that were retrieved but not used.
const UNUSED_ALPHA: f64 = 0.05;

/// Reward override for "retrieved but not used" — slight negative signal.
const UNUSED_REWARD: f64 = 0.3;

/// One entity's utility score after an outcome was applied to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityUtility {
    pub entity_id: String,
    pub utility_score: f64,
}

/// Report from a feedback recording operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedbackReport {
    pub outcome_entity_id: String,
    pub edges_created: u32,
    pub entities_updated: u32,
    /// Post-update utility of every entity the outcome reached — the
    /// observable half of the feedback loop.
    #[serde(default)]
    pub utilities: Vec<EntityUtility>,
    pub errors: Vec<String>,
}

/// The entities one session is known to have touched.
///
/// `retrieved` is everything linked to the session; `used` is the subset the
/// session's own records say it actually leaned on. Entities in `retrieved`
/// but not `used` get the muted "retrieved and ignored" signal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionEntities {
    pub retrieved: Vec<String>,
    pub used: Vec<String>,
}

impl SessionEntities {
    /// True when the session has no entities to apply an outcome to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.retrieved.is_empty()
    }
}

/// Record outcome feedback: link retrieved entities to an outcome and update utility scores.
pub async fn record_outcome_feedback(
    db: &Surreal<Db>,
    session_id: &str,
    outcome: OutcomeKind,
    retrieved_entity_ids: &[String],
    used_entity_ids: Option<&[String]>,
) -> Result<FeedbackReport, GraphError> {
    let mut report = FeedbackReport::default();

    if retrieved_entity_ids.is_empty() {
        return Ok(report);
    }

    let result = ContributionResult::Resolved(outcome);
    let outcome_id = outcome_entity_for_session(db, session_id, result).await?;
    report.outcome_entity_id = outcome_id.clone();

    let reward = outcome.reward();

    // Build a HashSet for O(1) "was used" lookups instead of O(n) per entity
    let used_set: Option<std::collections::HashSet<&str>> =
        used_entity_ids.map(|ids| ids.iter().map(|s| s.as_str()).collect());

    // Process all entities concurrently — each entity's feedback is independent
    let outcome_id_ref = &outcome_id;
    let futures: Vec<_> = retrieved_entity_ids
        .iter()
        .map(|entity_id| {
            let was_used = used_set
                .as_ref()
                .map(|s| s.contains(entity_id.as_str()))
                .unwrap_or(true);
            let (alpha, effective_reward) = if was_used {
                (USED_ALPHA, reward)
            } else {
                (UNUSED_ALPHA, UNUSED_REWARD)
            };

            async move {
                let edge_result = create_contribution_edge(
                    db,
                    entity_id,
                    outcome_id_ref,
                    result,
                    was_used,
                    session_id,
                )
                .await;
                let utility_result =
                    update_utility_score(db, entity_id, effective_reward, alpha).await;
                let score = get_utility_score(db, entity_id).await;
                (entity_id, edge_result, utility_result, score)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    for (entity_id, edge_result, utility_result, score) in results {
        match edge_result {
            Ok(()) => report.edges_created += 1,
            Err(e) => {
                report
                    .errors
                    .push(format!("edge {entity_id} -> {outcome_id}: {e}"));
            }
        }
        match utility_result {
            Ok(()) => report.entities_updated += 1,
            Err(e) => {
                report
                    .errors
                    .push(format!("utility update {entity_id}: {e}"));
            }
        }
        if let Ok(utility_score) = score {
            report.utilities.push(EntityUtility {
                entity_id: entity_id.clone(),
                utility_score,
            });
        }
    }

    Ok(report)
}

/// Record that a session touched these entities, without judging the outcome.
///
/// The passive half of the feedback loop: ingestion knows which entities a
/// session produced or reinforced, and says so here. A later
/// `graph feedback <session>` supplies the outcome and this is what tells it
/// which entities the outcome applies to.
///
/// Idempotent per session: re-ingesting a session rewrites its records rather
/// than accumulating duplicates. Utility scores are untouched — an
/// unadjudicated session is not evidence of usefulness.
pub async fn record_session_use(
    db: &Surreal<Db>,
    session_id: &str,
    entity_ids: &[String],
) -> Result<u32, GraphError> {
    if entity_ids.is_empty() {
        return Ok(0);
    }

    let outcome_id =
        outcome_entity_for_session(db, session_id, ContributionResult::Pending).await?;

    let mut recorded = 0;
    for entity_id in entity_ids {
        create_contribution_edge(
            db,
            entity_id,
            &outcome_id,
            ContributionResult::Pending,
            true,
            session_id,
        )
        .await?;
        recorded += 1;
    }

    Ok(recorded)
}

/// The entities a session touched, as its `contributed_to` records tell it.
///
/// Falls back to the entities the session authored (`source = session_id`)
/// when no records exist — the shape of a store whose sessions were ingested
/// before passive recording, where authorship is the only session link.
pub async fn session_entities(
    db: &Surreal<Db>,
    session_id: &str,
) -> Result<SessionEntities, GraphError> {
    #[derive(Deserialize)]
    struct EdgeRow {
        #[serde(rename = "in")]
        entity: serde_json::Value,
        #[serde(default = "default_was_used")]
        was_used: bool,
    }

    fn default_was_used() -> bool {
        true
    }

    let mut response = db
        .query("SELECT in, was_used FROM contributed_to WHERE session_id = $sid")
        .bind(("sid", session_id.to_string()))
        .await?;

    let rows: Vec<EdgeRow> = super::deserialize_take(&mut response, 0)?;
    if !rows.is_empty() {
        let mut session = SessionEntities::default();
        for row in rows {
            let id = record_id_string(&row.entity);
            if session.retrieved.contains(&id) {
                continue;
            }
            if row.was_used {
                session.used.push(id.clone());
            }
            session.retrieved.push(id);
        }
        return Ok(session);
    }

    let mut response = db
        .query("SELECT id FROM entity WHERE source = $sid")
        .bind(("sid", session_id.to_string()))
        .await?;

    #[derive(Deserialize)]
    struct IdRow {
        id: serde_json::Value,
    }

    let rows: Vec<IdRow> = super::deserialize_take(&mut response, 0)?;
    let retrieved: Vec<String> = rows.iter().map(|r| record_id_string(&r.id)).collect();

    Ok(SessionEntities {
        used: retrieved.clone(),
        retrieved,
    })
}

/// Render a record ID value as `table:id`.
fn record_id_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What a contribution record says about its session so far.
///
/// A session is linked to its entities the moment ingestion knows about them,
/// which is before anyone has judged how it went. `Pending` is that state; it
/// carries no reward and never moves a utility score.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ContributionResult {
    Pending,
    Resolved(OutcomeKind),
}

impl std::fmt::Display for ContributionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Resolved(outcome) => write!(f, "{outcome}"),
        }
    }
}

/// The session's outcome entity, created on first use and reused after.
///
/// One outcome record per session, whatever order the passive linkage and the
/// adjudicated outcome arrive in: re-running feedback for a session corrects
/// its record rather than growing a second one.
async fn outcome_entity_for_session(
    db: &Surreal<Db>,
    session_id: &str,
    result: ContributionResult,
) -> Result<String, GraphError> {
    match find_outcome_entity(db, session_id).await? {
        Some(id) => {
            update_outcome_entity(db, &id, session_id, result).await?;
            Ok(id)
        }
        None => create_outcome_entity(db, session_id, result).await,
    }
}

async fn find_outcome_entity(
    db: &Surreal<Db>,
    session_id: &str,
) -> Result<Option<String>, GraphError> {
    #[derive(Deserialize)]
    struct IdRow {
        id: serde_json::Value,
    }

    let mut response = db
        .query(
            r#"SELECT id FROM entity
               WHERE entity_type = "outcome" AND attributes.session_id = $sid
               LIMIT 1"#,
        )
        .bind(("sid", session_id.to_string()))
        .await?;

    let rows: Vec<IdRow> = super::deserialize_take(&mut response, 0)?;
    Ok(rows.first().map(|r| record_id_string(&r.id)))
}

async fn update_outcome_entity(
    db: &Surreal<Db>,
    outcome_id: &str,
    session_id: &str,
    result: ContributionResult,
) -> Result<(), GraphError> {
    db.query(
        r#"UPDATE type::record($id) SET
               abstract = $abstract,
               attributes = $attributes,
               updated_at = time::now()"#,
    )
    .bind(("id", outcome_id.to_string()))
    .bind(("abstract", outcome_abstract(session_id, result)))
    .bind(("attributes", outcome_attributes(session_id, result)))
    .await?
    .check()?;

    Ok(())
}

fn outcome_abstract(session_id: &str, result: ContributionResult) -> String {
    format!("Session {session_id} outcome: {result}")
}

fn outcome_attributes(session_id: &str, result: ContributionResult) -> serde_json::Value {
    serde_json::json!({
        "outcome_result": result.to_string(),
        "session_id": session_id,
    })
}

async fn create_outcome_entity(
    db: &Surreal<Db>,
    session_id: &str,
    outcome: ContributionResult,
) -> Result<String, GraphError> {
    let abstract_text = outcome_abstract(session_id, outcome);

    let mut response = db
        .query(
            r#"
            CREATE entity SET
                name = $name,
                entity_type = "outcome",
                abstract = $abstract,
                overview = "",
                content = NONE,
                attributes = $attributes,
                embedding = NONE,
                mutable = false,
                access_count = 0,
                utility_score = $utility,
                utility_updates = 0,
                created_at = time::now(),
                updated_at = time::now(),
                source = $source
            "#,
        )
        .bind(("name", format!("outcome-{session_id}")))
        .bind(("abstract", abstract_text))
        .bind(("attributes", outcome_attributes(session_id, outcome)))
        .bind(("utility", DEFAULT_UTILITY))
        .bind(("source", format!("caliber:{session_id}")))
        .await?;

    let entity: Option<super::types::Entity> = super::deserialize_take_opt(&mut response, 0)?;
    let entity = entity.ok_or_else(|| {
        GraphError::Db(surrealdb::Error::thrown(
            "failed to create outcome entity".into(),
        ))
    })?;

    Ok(entity.id_string())
}

/// Write one entity's contribution record for a session, replacing any
/// earlier record for the same pair.
///
/// One record per entity per session: the passive "this session touched it"
/// marker and the adjudicated outcome are the same fact learned twice, not
/// two contributions.
async fn create_contribution_edge(
    db: &Surreal<Db>,
    entity_id: &str,
    outcome_id: &str,
    outcome: ContributionResult,
    was_used: bool,
    session_id: &str,
) -> Result<(), GraphError> {
    db.query(
        r#"
        LET $from = type::record($from_id);
        LET $to = type::record($to_id);
        DELETE contributed_to WHERE in = $from AND session_id = $session_id;
        RELATE $from -> contributed_to -> $to SET
            outcome_result = $outcome_result,
            was_used = $was_used,
            session_id = $session_id,
            timestamp = time::now()
        "#,
    )
    .bind(("from_id", entity_id.to_string()))
    .bind(("to_id", outcome_id.to_string()))
    .bind(("outcome_result", outcome.to_string()))
    .bind(("was_used", was_used))
    .bind(("session_id", session_id.to_string()))
    .await?
    .check()?;

    Ok(())
}

/// Atomic EMA update — single query, no read-modify-write race.
async fn update_utility_score(
    db: &Surreal<Db>,
    entity_id: &str,
    reward: f64,
    alpha: f64,
) -> Result<(), GraphError> {
    // Inline EMA: new = (1 - alpha) * current + alpha * reward, clamped to [0, 1].
    // SurrealDB has no clamp, so the bounds are an IF chain — one chain, one
    // `END`, whatever the branch count.
    db.query(
        r#"
        LET $raw = (1.0 - $alpha) * type::record($id).utility_score + $alpha * $reward;
        LET $clamped = IF $raw < 0.0 THEN 0.0 ELSE IF $raw > 1.0 THEN 1.0 ELSE $raw END;
        UPDATE type::record($id) SET
            utility_score = $clamped,
            utility_updates += 1,
            updated_at = time::now()
        "#,
    )
    .bind(("id", entity_id.to_string()))
    .bind(("alpha", alpha))
    .bind(("reward", reward))
    .await?
    .check()?;

    Ok(())
}

/// Get the current utility score for an entity.
pub async fn get_utility_score(db: &Surreal<Db>, entity_id: &str) -> Result<f64, GraphError> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(default = "default_util")]
        utility_score: f64,
    }

    fn default_util() -> f64 {
        DEFAULT_UTILITY
    }

    let mut response = db
        .query("SELECT utility_score FROM type::record($id)")
        .bind(("id", entity_id.to_string()))
        .await?;

    let rows: Vec<Row> = super::deserialize_take(&mut response, 0)?;

    Ok(rows
        .first()
        .map(|r| r.utility_score)
        .unwrap_or(DEFAULT_UTILITY))
}

/// Get aggregate contribution stats for an entity.
#[derive(Debug, Clone, Default)]
pub struct ContributionStats {
    pub total_contributions: u32,
    pub successes: u32,
    pub partials: u32,
    pub failures: u32,
    pub times_used: u32,
    pub times_ignored: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_kind_reward_values() {
        assert_eq!(OutcomeKind::Success.reward(), 1.0);
        assert_eq!(OutcomeKind::Partial.reward(), 0.5);
        assert_eq!(OutcomeKind::Failed.reward(), 0.0);
    }

    #[test]
    fn outcome_kind_roundtrip() {
        for kind in [
            OutcomeKind::Success,
            OutcomeKind::Partial,
            OutcomeKind::Failed,
        ] {
            let s = kind.to_string();
            let parsed: OutcomeKind = s.parse().unwrap();
            assert_eq!(parsed, kind);
        }
        assert!("unknown".parse::<OutcomeKind>().is_err());
    }

    #[test]
    fn ema_update_math() {
        let current: f64 = 0.5;
        let alpha: f64 = 0.1;

        let success = (1.0 - alpha) * current + alpha * 1.0;
        assert!((success - 0.55).abs() < 0.001);

        let partial = (1.0 - alpha) * current + alpha * 0.5;
        assert!((partial - 0.5).abs() < 0.001);

        let failed = (1.0 - alpha) * current + alpha * 0.0;
        assert!((failed - 0.45).abs() < 0.001);
    }

    #[test]
    fn ema_converges() {
        let mut score = 0.5;
        for _ in 0..50 {
            score = (1.0 - USED_ALPHA) * score + USED_ALPHA * 1.0;
        }
        assert!(score > 0.99);

        let mut score = 0.5;
        for _ in 0..50 {
            score = (1.0 - USED_ALPHA) * score + USED_ALPHA * 0.0;
        }
        assert!(score < 0.01);
    }

    #[test]
    fn pending_contribution_reads_as_unadjudicated() {
        assert_eq!(ContributionResult::Pending.to_string(), "pending");
        assert_eq!(
            ContributionResult::Resolved(OutcomeKind::Success).to_string(),
            "success"
        );
        assert_eq!(
            outcome_abstract("s1", ContributionResult::Pending),
            "Session s1 outcome: pending"
        );
        assert_eq!(
            outcome_attributes("s1", ContributionResult::Resolved(OutcomeKind::Failed)),
            serde_json::json!({"outcome_result": "failed", "session_id": "s1"})
        );
    }

    #[test]
    fn session_with_no_entities_is_empty() {
        assert!(SessionEntities::default().is_empty());
        assert!(!SessionEntities {
            retrieved: vec!["entity:a".into()],
            used: vec![],
        }
        .is_empty());
    }

    #[test]
    fn record_ids_render_as_table_colon_id() {
        assert_eq!(
            record_id_string(&serde_json::json!("entity:abc")),
            "entity:abc"
        );
        assert_eq!(record_id_string(&serde_json::json!(42)), "42");
    }

    #[test]
    fn feedback_report_crosses_the_wire() {
        let report = FeedbackReport {
            outcome_entity_id: "entity:outcome".into(),
            edges_created: 2,
            entities_updated: 2,
            utilities: vec![EntityUtility {
                entity_id: "entity:a".into(),
                utility_score: 0.55,
            }],
            errors: vec![],
        };
        let json = serde_json::to_value(&report).expect("serialize");
        let parsed: FeedbackReport = serde_json::from_value(json).expect("deserialize");
        assert_eq!(parsed.utilities, report.utilities);
        assert_eq!(parsed.entities_updated, 2);
    }

    #[test]
    fn unused_entity_gets_weaker_signal() {
        let current = 0.5;
        let used_step = (1.0 - USED_ALPHA) * current + USED_ALPHA * 1.0;
        let unused_step = (1.0 - UNUSED_ALPHA) * current + UNUSED_ALPHA * UNUSED_REWARD;

        assert!(used_step > current);
        assert!(unused_step < current);
    }
}
