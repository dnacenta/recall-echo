//! LLM-powered entity deduplication — skip, create, or merge decisions.

use super::error::GraphError;
use super::llm::LlmProvider;
use super::types::*;
use super::GraphMemory;

const DEDUP_SYSTEM_PROMPT: &str = r#"You are a deduplication system for a knowledge graph. Given a candidate entity and existing similar entities, decide:

1. "skip" — The candidate is a duplicate. It adds no new information.
2. "create" — The candidate is genuinely new despite surface similarity.
3. "merge" — The candidate adds new information to an existing entity. Specify which one.

Return EXACTLY this JSON (no markdown fencing, no explanation):

{
  "decision": "skip" | "create" | "merge",
  "target": "Name of existing entity to merge into (only if merge)",
  "reason": "Brief explanation"
}

Rules:
- Same entity with minor name variations (e.g., "ElevenLabs" vs "Eleven Labs"): merge
- Same concept but genuinely different instances: create
- Candidate adds meaningful new detail to an existing entity: merge
- Candidate is less detailed than existing: skip
- When in doubt between create and merge: prefer create (avoid data loss)"#;

/// Resolved entity after dedup — either newly created or existing (merged/skipped).
pub enum ResolvedEntity {
    Created(Entity),
    Merged(Entity),
    Skipped,
}

/// Run the full dedup pipeline for one extracted entity.
///
/// 1. Vector search for similar entities
/// 2. If none similar: CREATE directly
/// 3. If similar found: ask LLM for skip/create/merge decision
/// 4. For merge on immutable types: fall back to CREATE
pub async fn resolve_entity(
    gm: &GraphMemory,
    llm: &dyn LlmProvider,
    candidate: &ExtractedEntity,
    session_id: &str,
) -> Result<ResolvedEntity, GraphError> {
    // Search for similar entities
    let similar = gm.search(&candidate.abstract_text, 5).await?;

    // Filter to meaningful similarity (> 0.7 blended score)
    let relevant: Vec<_> = similar.iter().filter(|r| r.score > 0.7).collect();

    if relevant.is_empty() {
        // No similar entities — create directly
        let entity = gm
            .add_entity(NewEntity {
                name: candidate.name.clone(),
                entity_type: candidate.entity_type.clone(),
                abstract_text: candidate.abstract_text.clone(),
                overview: candidate.overview.clone(),
                content: candidate.content.clone(),
                attributes: candidate.attributes.clone(),
                source: Some(session_id.to_string()),
            })
            .await?;
        return Ok(ResolvedEntity::Created(entity));
    }

    // Ask LLM for dedup decision
    let user_message = build_dedup_message(candidate, &relevant);
    let response = llm
        .complete(DEDUP_SYSTEM_PROMPT, &user_message, 300)
        .await?;

    let decision = parse_dedup_response(&response)?;

    match decision {
        DedupDecision::Skip => Ok(ResolvedEntity::Skipped),

        DedupDecision::Create => {
            let entity = gm
                .add_entity(NewEntity {
                    name: candidate.name.clone(),
                    entity_type: candidate.entity_type.clone(),
                    abstract_text: candidate.abstract_text.clone(),
                    overview: candidate.overview.clone(),
                    content: candidate.content.clone(),
                    attributes: candidate.attributes.clone(),
                    source: Some(session_id.to_string()),
                })
                .await?;
            Ok(ResolvedEntity::Created(entity))
        }

        DedupDecision::Merge { target } => {
            // Find the target entity
            let target_entity = gm.get_entity(&target).await?;
            let Some(target_entity) = target_entity else {
                // Target not found — fall back to create
                let entity = gm
                    .add_entity(NewEntity {
                        name: candidate.name.clone(),
                        entity_type: candidate.entity_type.clone(),
                        abstract_text: candidate.abstract_text.clone(),
                        overview: candidate.overview.clone(),
                        content: candidate.content.clone(),
                        attributes: candidate.attributes.clone(),
                        source: Some(session_id.to_string()),
                    })
                    .await?;
                return Ok(ResolvedEntity::Created(entity));
            };

            // Check mutability
            if !target_entity.mutable {
                // Immutable — can't merge, create instead
                let entity = gm
                    .add_entity(NewEntity {
                        name: candidate.name.clone(),
                        entity_type: candidate.entity_type.clone(),
                        abstract_text: candidate.abstract_text.clone(),
                        overview: candidate.overview.clone(),
                        content: candidate.content.clone(),
                        attributes: candidate.attributes.clone(),
                        source: Some(session_id.to_string()),
                    })
                    .await?;
                return Ok(ResolvedEntity::Created(entity));
            }

            let merged = merge_entity(gm, &target_entity, candidate).await?;
            Ok(ResolvedEntity::Merged(merged))
        }
    }
}

/// Merge candidate data into an existing entity.
///
/// Rules:
/// - Abstract: use longer/more detailed version
/// - Overview: concatenate if both exist
/// - Content: append candidate content
/// - Attributes: deep-merge (candidate wins on conflict)
async fn merge_entity(
    gm: &GraphMemory,
    target: &Entity,
    candidate: &ExtractedEntity,
) -> Result<Entity, GraphError> {
    let new_abstract = if candidate.abstract_text.len() > target.abstract_text.len() {
        Some(candidate.abstract_text.clone())
    } else {
        None
    };

    let new_overview = candidate.overview.as_ref().map(|co| {
        if target.overview.is_empty() {
            co.clone()
        } else {
            format!("{}\n\n{}", target.overview, co)
        }
    });

    let new_content = candidate.content.as_ref().map(|cc| match &target.content {
        Some(tc) => format!("{}\n\n{}", tc, cc),
        None => cc.clone(),
    });

    let new_attributes = candidate
        .attributes
        .as_ref()
        .map(|ca| match &target.attributes {
            Some(ta) => merge_json_objects(ta, ca),
            None => ca.clone(),
        });

    let updates = EntityUpdate {
        abstract_text: new_abstract,
        overview: new_overview,
        content: new_content,
        attributes: new_attributes,
    };

    gm.update_entity(&target.id_string(), updates).await
}

fn build_dedup_message(candidate: &ExtractedEntity, similar: &[&SearchResult]) -> String {
    let mut msg = format!(
        "CANDIDATE:\n  Name: {}\n  Type: {}\n  Abstract: {}\n\nEXISTING SIMILAR ENTITIES:\n",
        candidate.name, candidate.entity_type, candidate.abstract_text
    );
    for (i, r) in similar.iter().enumerate() {
        msg.push_str(&format!(
            "\n{}. Name: {} (score: {:.3})\n   Type: {}\n   Abstract: {}\n",
            i + 1,
            r.entity.name,
            r.score,
            r.entity.entity_type,
            r.entity.abstract_text
        ));
    }
    msg
}

/// Parse the LLM's dedup decision from JSON.
pub fn parse_dedup_response(text: &str) -> Result<DedupDecision, GraphError> {
    let cleaned = strip_markdown_fencing(text);

    let v: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        // Try extracting JSON from surrounding text
        if let Some(json_str) = extract_json_object(&cleaned) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                return parse_decision_value(&v)
                    .err()
                    .unwrap_or_else(|| GraphError::Parse(e.to_string()));
            }
        }
        GraphError::Parse(format!("dedup response not valid JSON: {}", e))
    })?;

    parse_decision_value(&v)
}

fn parse_decision_value(v: &serde_json::Value) -> Result<DedupDecision, GraphError> {
    let decision = v
        .get("decision")
        .and_then(|d| d.as_str())
        .ok_or_else(|| GraphError::Parse("missing 'decision' field".into()))?;

    match decision {
        "skip" => Ok(DedupDecision::Skip),
        "create" => Ok(DedupDecision::Create),
        "merge" => {
            let target = v
                .get("target")
                .and_then(|t| t.as_str())
                .ok_or_else(|| GraphError::Parse("merge decision missing 'target' field".into()))?;
            Ok(DedupDecision::Merge {
                target: target.to_string(),
            })
        }
        other => Err(GraphError::Parse(format!("unknown decision: {}", other))),
    }
}

fn strip_markdown_fencing(text: &str) -> String {
    let trimmed = text.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or(trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let stripped = stripped.strip_suffix("```").unwrap_or(stripped);
    stripped.trim().to_string()
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes[start..].iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

fn merge_json_objects(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            let mut merged = b.clone();
            for (k, v) in o {
                merged.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(merged)
        }
        _ => overlay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skip_decision() {
        let json = r#"{"decision": "skip", "reason": "duplicate"}"#;
        let decision = parse_dedup_response(json).unwrap();
        assert_eq!(decision, DedupDecision::Skip);
    }

    #[test]
    fn parse_create_decision() {
        let json = r#"{"decision": "create", "reason": "genuinely new"}"#;
        let decision = parse_dedup_response(json).unwrap();
        assert_eq!(decision, DedupDecision::Create);
    }

    #[test]
    fn parse_merge_decision() {
        let json = r#"{"decision": "merge", "target": "Rust", "reason": "same entity"}"#;
        let decision = parse_dedup_response(json).unwrap();
        assert_eq!(
            decision,
            DedupDecision::Merge {
                target: "Rust".into()
            }
        );
    }

    #[test]
    fn parse_with_fencing() {
        let json = "```json\n{\"decision\": \"skip\", \"reason\": \"dup\"}\n```";
        let decision = parse_dedup_response(json).unwrap();
        assert_eq!(decision, DedupDecision::Skip);
    }

    #[test]
    fn merge_json_objects_test() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let overlay = serde_json::json!({"b": 3, "c": 4});
        let merged = merge_json_objects(&base, &overlay);
        assert_eq!(merged, serde_json::json!({"a": 1, "b": 3, "c": 4}));
    }
}
