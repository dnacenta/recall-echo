//! Vigil-pulse sync engine — ingest metacognitive signals and outcomes into the graph.
//!
//! Reads vigil JSON files directly (no vigil-pulse dependency).
//! Idempotent — deduplicates by timestamp for signals, by (task_id, timestamp) for outcomes.

use std::path::Path;

use serde::Deserialize;

use super::error::GraphError;
use super::types::*;
use super::GraphMemory;

// ── Signal types (deserialized from vigil/signals.json) ──────────────

#[derive(Deserialize, Debug)]
struct SignalVector {
    timestamp: String,
    trigger: String,
    signals: Signals,
}

#[derive(Deserialize, Debug)]
struct Signals {
    vocabulary_diversity: Option<f64>,
    question_generation: Option<f64>,
    thought_lifecycle: Option<f64>,
    evidence_grounding: Option<f64>,
}

// ── Outcome types (deserialized from caliber/outcomes.json) ──────────

#[derive(Deserialize, Debug)]
struct OutcomeRecord {
    task_id: String,
    timestamp: String,
    domain: String,
    task_type: String,
    description: String,
    outcome: String,
    tokens_used: u32,
    tool_rounds: u32,
}

// ── Sync functions ───────────────────────────────────────────────────

/// Sync vigil signal vectors into the graph as Measurement entities.
pub async fn sync_vigil_signals(
    gm: &GraphMemory,
    signals_path: &Path,
) -> Result<VigilSyncReport, GraphError> {
    let mut report = VigilSyncReport::default();

    if !signals_path.exists() {
        return Ok(report);
    }

    let content = std::fs::read_to_string(signals_path).map_err(GraphError::Io)?;

    let signals: Vec<SignalVector> =
        serde_json::from_str(&content).map_err(GraphError::Json)?;

    if signals.is_empty() {
        return Ok(report);
    }

    // Get existing measurements to dedup by timestamp
    let existing = get_vigil_entities(gm, "measurement").await?;
    let existing_timestamps: Vec<String> = existing
        .iter()
        .filter_map(|e| {
            e.attributes
                .as_ref()
                .and_then(|a| a.get("timestamp"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();

    for signal in &signals {
        if existing_timestamps.contains(&signal.timestamp) {
            report.skipped += 1;
            continue;
        }

        let mut attrs = serde_json::Map::new();
        attrs.insert(
            "timestamp".into(),
            serde_json::Value::String(signal.timestamp.clone()),
        );
        attrs.insert(
            "trigger".into(),
            serde_json::Value::String(signal.trigger.clone()),
        );
        attrs.insert(
            "source_type".into(),
            serde_json::Value::String("vigil_signal".into()),
        );

        if let Some(v) = signal.signals.vocabulary_diversity {
            attrs.insert("vocabulary_diversity".into(), serde_json::json!(v));
        }
        if let Some(v) = signal.signals.question_generation {
            attrs.insert("question_generation".into(), serde_json::json!(v));
        }
        if let Some(v) = signal.signals.thought_lifecycle {
            attrs.insert("thought_lifecycle".into(), serde_json::json!(v));
        }
        if let Some(v) = signal.signals.evidence_grounding {
            attrs.insert("evidence_grounding".into(), serde_json::json!(v));
        }

        // Build a concise abstract from the signal values
        let abstract_text = format_signal_abstract(&signal.signals, &signal.trigger);

        let entity = NewEntity {
            name: format!("signal-{}", signal.timestamp),
            entity_type: EntityType::Measurement,
            abstract_text,
            overview: None,
            content: None,
            attributes: Some(serde_json::Value::Object(attrs)),
            source: Some("vigil:signals".into()),
        };

        match gm.add_entity(entity).await {
            Ok(_) => report.measurements_created += 1,
            Err(e) => report
                .errors
                .push(format!("signal {}: {}", signal.timestamp, e)),
        }
    }

    Ok(report)
}

/// Sync outcome records into the graph as Outcome entities.
pub async fn sync_outcomes(
    gm: &GraphMemory,
    outcomes_path: &Path,
) -> Result<VigilSyncReport, GraphError> {
    let mut report = VigilSyncReport::default();

    if !outcomes_path.exists() {
        return Ok(report);
    }

    let content = std::fs::read_to_string(outcomes_path).map_err(GraphError::Io)?;

    let outcomes: Vec<OutcomeRecord> =
        serde_json::from_str(&content).map_err(GraphError::Json)?;

    if outcomes.is_empty() {
        return Ok(report);
    }

    // Get existing outcomes to dedup by (task_id, timestamp)
    let existing = get_vigil_entities(gm, "outcome").await?;
    let existing_keys: Vec<String> = existing
        .iter()
        .filter_map(|e| {
            let attrs = e.attributes.as_ref()?;
            let task_id = attrs.get("task_id")?.as_str()?;
            let ts = attrs.get("timestamp")?.as_str()?;
            Some(format!("{}:{}", task_id, ts))
        })
        .collect();

    for outcome in &outcomes {
        let key = format!("{}:{}", outcome.task_id, outcome.timestamp);
        if existing_keys.contains(&key) {
            report.skipped += 1;
            continue;
        }

        let mut attrs = serde_json::Map::new();
        attrs.insert(
            "task_id".into(),
            serde_json::Value::String(outcome.task_id.clone()),
        );
        attrs.insert(
            "timestamp".into(),
            serde_json::Value::String(outcome.timestamp.clone()),
        );
        attrs.insert(
            "domain".into(),
            serde_json::Value::String(outcome.domain.clone()),
        );
        attrs.insert(
            "task_type".into(),
            serde_json::Value::String(outcome.task_type.clone()),
        );
        attrs.insert(
            "outcome_result".into(),
            serde_json::Value::String(outcome.outcome.clone()),
        );
        attrs.insert("tokens_used".into(), serde_json::json!(outcome.tokens_used));
        attrs.insert("tool_rounds".into(), serde_json::json!(outcome.tool_rounds));
        attrs.insert(
            "source_type".into(),
            serde_json::Value::String("vigil_outcome".into()),
        );

        let abstract_text = format!(
            "{} task '{}' — {} ({}, {} tokens, {} tool rounds)",
            outcome.outcome,
            outcome.task_id,
            outcome.description,
            outcome.domain,
            outcome.tokens_used,
            outcome.tool_rounds,
        );

        let entity = NewEntity {
            name: format!("outcome-{}-{}", outcome.task_id, outcome.timestamp),
            entity_type: EntityType::Outcome,
            abstract_text,
            overview: Some(outcome.description.clone()),
            content: None,
            attributes: Some(serde_json::Value::Object(attrs)),
            source: Some("vigil:outcomes".into()),
        };

        match gm.add_entity(entity).await {
            Ok(created) => {
                report.outcomes_created += 1;

                // Try to link outcome to a domain concept entity if one exists
                if let Ok(Some(_)) = gm.get_entity(&outcome.domain).await {
                    let rel = NewRelationship {
                        from_entity: created.name.clone(),
                        to_entity: outcome.domain.clone(),
                        rel_type: vigil_rels::RESULTED_IN.to_string(),
                        description: Some(format!("{} in {}", outcome.outcome, outcome.domain)),
                        confidence: Some(1.0),
                        source: Some("vigil:sync".into()),
                    };
                    match gm.add_relationship(rel).await {
                        Ok(_) => report.relationships_created += 1,
                        Err(e) => report.errors.push(format!("rel outcome->domain: {}", e)),
                    }
                }
            }
            Err(e) => report
                .errors
                .push(format!("outcome {}: {}", outcome.task_id, e)),
        }
    }

    Ok(report)
}

/// Sync both signals and outcomes in one call.
pub async fn sync_vigil(
    gm: &GraphMemory,
    signals_path: &Path,
    outcomes_path: &Path,
) -> Result<VigilSyncReport, GraphError> {
    let mut report = VigilSyncReport::default();

    let signals_report = sync_vigil_signals(gm, signals_path).await?;
    report.measurements_created = signals_report.measurements_created;
    report.skipped += signals_report.skipped;
    report.errors.extend(signals_report.errors);

    let outcomes_report = sync_outcomes(gm, outcomes_path).await?;
    report.outcomes_created = outcomes_report.outcomes_created;
    report.relationships_created = outcomes_report.relationships_created;
    report.skipped += outcomes_report.skipped;
    report.errors.extend(outcomes_report.errors);

    Ok(report)
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Query entities by type from the graph.
async fn get_vigil_entities(
    gm: &GraphMemory,
    entity_type: &str,
) -> Result<Vec<Entity>, GraphError> {
    let mut response = gm
        .db()
        .query("SELECT * FROM entity WHERE entity_type = $type")
        .bind(("type", entity_type.to_string()))
        .await?;

    super::deserialize_take(&mut response, 0)
}

/// Format a human-readable abstract from signal values.
fn format_signal_abstract(signals: &Signals, trigger: &str) -> String {
    let mut parts = Vec::new();
    if let Some(v) = signals.vocabulary_diversity {
        parts.push(format!("vocab={:.2}", v));
    }
    if let Some(v) = signals.question_generation {
        parts.push(format!("questions={:.1}", v));
    }
    if let Some(v) = signals.thought_lifecycle {
        parts.push(format!("thought_lifecycle={:.2}", v));
    }
    if let Some(v) = signals.evidence_grounding {
        parts.push(format!("evidence={:.2}", v));
    }
    if parts.is_empty() {
        format!("Signal measurement (trigger: {})", trigger)
    } else {
        format!(
            "Signal measurement: {} (trigger: {})",
            parts.join(", "),
            trigger
        )
    }
}
