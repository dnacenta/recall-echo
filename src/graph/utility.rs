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

/// Report from a feedback recording operation.
#[derive(Debug, Clone, Default)]
pub struct FeedbackReport {
    pub outcome_entity_id: String,
    pub edges_created: u32,
    pub entities_updated: u32,
    pub errors: Vec<String>,
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

    let outcome_id = create_outcome_entity(db, session_id, outcome).await?;
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
                    outcome,
                    was_used,
                    session_id,
                )
                .await;
                let utility_result =
                    update_utility_score(db, entity_id, effective_reward, alpha).await;
                (entity_id, edge_result, utility_result)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    for (entity_id, edge_result, utility_result) in results {
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
    }

    Ok(report)
}

async fn create_outcome_entity(
    db: &Surreal<Db>,
    session_id: &str,
    outcome: OutcomeKind,
) -> Result<String, GraphError> {
    let abstract_text = format!("Session {session_id} outcome: {outcome}");

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
        .bind((
            "attributes",
            serde_json::json!({
                "outcome_result": outcome.to_string(),
                "session_id": session_id,
            }),
        ))
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

async fn create_contribution_edge(
    db: &Surreal<Db>,
    entity_id: &str,
    outcome_id: &str,
    outcome: OutcomeKind,
    was_used: bool,
    session_id: &str,
) -> Result<(), GraphError> {
    db.query(
        r#"
        LET $from = type::record($from_id);
        LET $to = type::record($to_id);
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
    // SurrealDB doesn't have math::clamp, so we use nested IF expressions.
    db.query(
        r#"
        LET $raw = (1.0 - $alpha) * type::record($id).utility_score + $alpha * $reward;
        LET $clamped = IF $raw < 0.0 THEN 0.0 ELSE IF $raw > 1.0 THEN 1.0 ELSE $raw END END;
        UPDATE type::record($id) SET
            utility_score = $clamped,
            utility_updates += 1,
            updated_at = time::now()
        "#,
    )
    .bind(("id", entity_id.to_string()))
    .bind(("alpha", alpha))
    .bind(("reward", reward))
    .await?;

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
    fn unused_entity_gets_weaker_signal() {
        let current = 0.5;
        let used_step = (1.0 - USED_ALPHA) * current + USED_ALPHA * 1.0;
        let unused_step = (1.0 - UNUSED_ALPHA) * current + UNUSED_ALPHA * UNUSED_REWARD;

        assert!(used_step > current);
        assert!(unused_step < current);
    }
}
