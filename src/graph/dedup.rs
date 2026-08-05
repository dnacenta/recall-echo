// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Entity deduplication — skip, create, or merge decisions.
//!
//! Most candidates are decided locally: an existing entity with the same name
//! and type is the same entity, a nearest neighbour far below the review
//! threshold is a new one. Only the band in between is worth a model call, and
//! only a bounded number of neighbours are ever shown to it — see
//! [`crate::config::GraphDedupConfig`].

use std::fmt::Write as _;

use super::error::GraphError;
use super::llm::LlmProvider;
use super::types::*;
use super::GraphMemory;
use crate::config::DedupBand;

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

/// Which gate decided a candidate — the cost record of one resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionPath {
    /// An existing entity carried the same name and type.
    NameMatch,
    /// The nearest neighbour was similar enough to be the same entity.
    SameEntityBand,
    /// No neighbour was similar enough to be worth comparing.
    NewEntityBand,
    /// The ambiguous band — a model call decided it.
    LlmDecision,
}

impl ResolutionPath {
    /// Whether this resolution paid for a model call.
    #[must_use]
    pub fn used_llm(self) -> bool {
        matches!(self, Self::LlmDecision)
    }
}

/// What dedup did with a candidate, and what it cost.
pub struct Resolution {
    pub entity: ResolvedEntity,
    pub path: ResolutionPath,
}

impl Resolution {
    fn new(entity: ResolvedEntity, path: ResolutionPath) -> Self {
        Self { entity, path }
    }
}

/// Run the dedup pipeline for one extracted entity.
///
/// Three bands of raw cosine similarity, only one of which costs a model call:
///
/// 1. An existing entity of the same name and type — the same entity, resolved
///    on an index lookup alone.
/// 2. Nearest neighbour at or above `certain_similarity` — the same entity.
/// 3. Nearest neighbour below `review_similarity` — a new entity, CREATEd.
/// 4. Anything left is genuinely ambiguous: the model sees at most
///    `max_candidates` neighbours and returns skip / create / merge.
///
/// Merging into an immutable type falls back to CREATE on every path.
pub async fn resolve_entity(
    gm: &GraphMemory,
    llm: &dyn LlmProvider,
    candidate: &ExtractedEntity,
    session_id: &str,
) -> Result<Resolution, GraphError> {
    let config = gm.dedup_config();

    if let Some(existing) = same_named_entity(gm, candidate).await? {
        let resolved = absorb(gm, &existing, candidate, session_id).await?;
        return Ok(Resolution::new(resolved, ResolutionPath::NameMatch));
    }

    // Ordered by distance ascending — nearest first. The limit is the cap:
    // neither the prompt nor the comparison count can grow with the store.
    let nearest = gm
        .search(&candidate.abstract_text, config.candidate_limit())
        .await?;

    let Some(closest) = nearest.first() else {
        return Ok(Resolution::new(
            create(gm, candidate, session_id).await?,
            ResolutionPath::NewEntityBand,
        ));
    };

    match config.band(closest.similarity()) {
        // Same meaning and same kind of thing: nothing for a model to weigh.
        DedupBand::SameEntity if closest.entity.entity_type == candidate.entity_type => {
            let resolved = absorb(gm, &closest.entity, candidate, session_id).await?;
            Ok(Resolution::new(resolved, ResolutionPath::SameEntityBand))
        }
        DedupBand::NewEntity => Ok(Resolution::new(
            create(gm, candidate, session_id).await?,
            ResolutionPath::NewEntityBand,
        )),
        // Ambiguous, or near-identical text under a different type — the one
        // case worth paying for.
        _ => {
            let resolved = resolve_with_llm(gm, llm, candidate, session_id, &nearest).await?;
            Ok(Resolution::new(resolved, ResolutionPath::LlmDecision))
        }
    }
}

/// Ask the model to decide between the candidate and its comparable neighbours.
async fn resolve_with_llm(
    gm: &GraphMemory,
    llm: &dyn LlmProvider,
    candidate: &ExtractedEntity,
    session_id: &str,
    nearest: &[SearchResult],
) -> Result<ResolvedEntity, GraphError> {
    let config = gm.dedup_config();
    let comparable: Vec<&SearchResult> = nearest
        .iter()
        .filter(|r| config.band(r.similarity()) != DedupBand::NewEntity)
        .collect();

    let user_message = build_dedup_message(candidate, &comparable);
    let response = llm
        .complete(DEDUP_SYSTEM_PROMPT, &user_message, 300)
        .await?;

    match parse_dedup_response(&response)? {
        DedupDecision::Skip => Ok(ResolvedEntity::Skipped),

        DedupDecision::Create => create(gm, candidate, session_id).await,

        DedupDecision::Merge { target } => match gm.get_entity(&target).await? {
            Some(target_entity) => absorb(gm, &target_entity, candidate, session_id).await,
            // Hallucinated target — create rather than lose the candidate.
            None => create(gm, candidate, session_id).await,
        },
    }
}

/// An existing entity with the same name and type as the candidate.
///
/// Exact match on the indexed `name` field: an index lookup, not a scan, so it
/// stays flat as the store grows. Case and spacing variants ("ElevenLabs" /
/// "Eleven Labs") are left to the similarity bands — SurrealDB has no
/// case-insensitive index here, and normalising in the query would turn every
/// dedup into a full table scan. A same-name entity of a *different* type is
/// not the same thing (the event "Release" is not the project "Release"), so it
/// falls through to the bands too.
async fn same_named_entity(
    gm: &GraphMemory,
    candidate: &ExtractedEntity,
) -> Result<Option<Entity>, GraphError> {
    let Some(existing) = gm.get_entity(candidate.name.trim()).await? else {
        return Ok(None);
    };
    Ok((existing.entity_type == candidate.entity_type).then_some(existing))
}

/// Fold a candidate into an entity already known to be the same thing.
///
/// Immutable types cannot absorb — a decision or an event records one moment —
/// so the candidate becomes its own entity, exactly as a model-issued merge
/// onto an immutable target already did. A candidate that carries nothing new
/// is skipped rather than appended: re-reading the same archive must not grow
/// the entity it describes.
async fn absorb(
    gm: &GraphMemory,
    target: &Entity,
    candidate: &ExtractedEntity,
    session_id: &str,
) -> Result<ResolvedEntity, GraphError> {
    if !target.mutable {
        return create(gm, candidate, session_id).await;
    }
    if !adds_information(target, candidate) {
        return Ok(ResolvedEntity::Skipped);
    }
    Ok(ResolvedEntity::Merged(
        merge_entity(gm, target, candidate).await?,
    ))
}

/// Whether the candidate carries anything the target does not already hold —
/// the deterministic half of the prompt's "candidate is less detailed: skip".
fn adds_information(target: &Entity, candidate: &ExtractedEntity) -> bool {
    let longer_abstract = candidate.abstract_text.len() > target.abstract_text.len();
    let new_overview = candidate
        .overview
        .as_ref()
        .is_some_and(|overview| !target.overview.contains(overview.as_str()));
    let new_content = candidate.content.as_ref().is_some_and(|content| {
        target
            .content
            .as_ref()
            .is_none_or(|held| !held.contains(content.as_str()))
    });
    let new_attributes = candidate
        .attributes
        .as_ref()
        .is_some_and(|attrs| target.attributes.as_ref() != Some(attrs));

    longer_abstract || new_overview || new_content || new_attributes
}

async fn create(
    gm: &GraphMemory,
    candidate: &ExtractedEntity,
    session_id: &str,
) -> Result<ResolvedEntity, GraphError> {
    let entity = gm.add_entity(candidate.to_new_entity(session_id)).await?;
    Ok(ResolvedEntity::Created(entity))
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
        Some(tc) => format!("{tc}\n\n{cc}"),
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
        // Similarity, not the blended retrieval score: how popular an entity is
        // says nothing about whether it is this one.
        let _ = write!(
            msg,
            "\n{}. Name: {} (similarity: {:.3})\n   Type: {}\n   Abstract: {}\n",
            i + 1,
            r.entity.name,
            r.similarity(),
            r.entity.entity_type,
            r.entity.abstract_text
        );
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
        GraphError::Parse(format!("dedup response not valid JSON: {e}"))
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
        other => Err(GraphError::Parse(format!("unknown decision: {other}"))),
    }
}

use super::util::{extract_json_object, merge_json_objects, strip_markdown_fencing};

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

    fn stored(abstract_text: &str, overview: &str, content: Option<&str>) -> Entity {
        Entity {
            id: serde_json::json!("entity:one"),
            name: "Biscuit".into(),
            entity_type: EntityType::Person,
            abstract_text: abstract_text.into(),
            overview: overview.into(),
            content: content.map(String::from),
            attributes: None,
            embedding: None,
            mutable: true,
            access_count: 0,
            utility_score: 0.5,
            utility_updates: 0,
            created_at: serde_json::json!("2026-01-01T00:00:00Z"),
            updated_at: serde_json::json!("2026-01-01T00:00:00Z"),
            source: None,
        }
    }

    fn extracted(abstract_text: &str, overview: Option<&str>) -> ExtractedEntity {
        ExtractedEntity {
            name: "Biscuit".into(),
            entity_type: EntityType::Person,
            abstract_text: abstract_text.into(),
            overview: overview.map(String::from),
            content: None,
            attributes: None,
        }
    }

    /// Re-reading the same archive must not append the same text again.
    #[test]
    fn a_candidate_repeating_what_is_stored_adds_nothing() {
        let target = stored("A golden retriever", "Adopted in May 2023", None);
        let candidate = extracted("A golden retriever", Some("Adopted in May 2023"));
        assert!(!adds_information(&target, &candidate));
    }

    #[test]
    fn a_longer_abstract_is_information() {
        let target = stored("A dog", "", None);
        let candidate = extracted("A golden retriever named Biscuit", None);
        assert!(adds_information(&target, &candidate));
    }

    #[test]
    fn an_unseen_overview_is_information() {
        let target = stored("A golden retriever", "Adopted in May 2023", None);
        let candidate = extracted("A golden retriever", Some("Finished puppy class"));
        assert!(adds_information(&target, &candidate));
    }

    #[test]
    fn content_the_target_already_holds_is_not_information() {
        let target = stored("A golden retriever", "", Some("Session 1\n\nSession 2"));
        let mut candidate = extracted("A golden retriever", None);
        candidate.content = Some("Session 2".into());
        assert!(!adds_information(&target, &candidate));

        candidate.content = Some("Session 3".into());
        assert!(adds_information(&target, &candidate));
    }

    #[test]
    fn only_the_llm_path_counts_as_a_model_call() {
        assert!(ResolutionPath::LlmDecision.used_llm());
        assert!(!ResolutionPath::NameMatch.used_llm());
        assert!(!ResolutionPath::SameEntityBand.used_llm());
        assert!(!ResolutionPath::NewEntityBand.used_llm());
    }

    /// The model must weigh meaning, not popularity: the prompt carries raw
    /// similarity so a hot entity does not read as a likelier duplicate.
    #[test]
    fn the_dedup_prompt_shows_similarity_not_the_blended_score() {
        let result = SearchResult {
            entity: stored("A golden retriever", "", None),
            score: 0.71,
            distance: 0.2,
        };
        let msg = build_dedup_message(&extracted("A dog", None), &[&result]);
        assert!(msg.contains("similarity: 0.800"), "{msg}");
        assert!(!msg.contains("0.710"), "{msg}");
    }

    #[test]
    fn merge_json_objects_test() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let overlay = serde_json::json!({"b": 3, "c": 4});
        let merged = merge_json_objects(&base, &overlay);
        assert_eq!(merged, serde_json::json!({"a": 1, "b": 3, "c": 4}));
    }
}
