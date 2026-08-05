//! Ingestion orchestrator — chunk → episode → extract → dedup → relationships.

use std::collections::HashMap;

use futures::stream::{self, StreamExt};

use super::confidence::{ExtractionContext, Provenance};
use super::crud;
use super::dedup::{self, ResolvedEntity};
use super::error::GraphError;
use super::extract;
use super::llm::LlmProvider;
use super::types::*;
use super::utility;
use super::GraphMemory;

/// Maximum number of concurrent LLM calls during extraction and dedup.
const LLM_CONCURRENCY: usize = 10;

/// Role headings written by the archive pipeline, lower-cased.
const USER_TURN_HEADING: &str = "### user";
const ASSISTANT_TURN_HEADING: &str = "### assistant";

/// How one ingestion run assigns a provenance class to what it writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProvenancePolicy {
    /// Read the class off each chunk's conversation turn roles. Text with no
    /// visible human-only turn is treated as the agent's own.
    #[default]
    FromTurnRoles,
    /// Stamp every episode in the run with one class — document ingestion
    /// (`--external`), and any caller that knows better than the heuristic.
    Fixed(Provenance),
}

impl ProvenancePolicy {
    /// The class this policy assigns to one chunk of archive text.
    #[must_use]
    pub fn classify(self, chunk: &str) -> Provenance {
        match self {
            Self::Fixed(provenance) => provenance,
            Self::FromTurnRoles => infer_from_turn_roles(chunk),
        }
    }
}

/// Where a run of ingestion is reading from, and what that makes its output.
///
/// Carried as one value rather than three parameters because every write the
/// run performs — episodes and confidence updates alike — must agree on it.
#[derive(Debug, Clone)]
pub struct IngestContext {
    session_id: String,
    log_number: Option<u32>,
    provenance: ProvenancePolicy,
}

impl IngestContext {
    /// Context for a conversation archive: provenance is read off turn roles.
    #[must_use]
    pub fn new(session_id: impl Into<String>, log_number: Option<u32>) -> Self {
        Self {
            session_id: session_id.into(),
            log_number,
            provenance: ProvenancePolicy::default(),
        }
    }

    /// Override the class assignment for the whole run.
    #[must_use]
    pub fn with_provenance(mut self, provenance: ProvenancePolicy) -> Self {
        self.provenance = provenance;
        self
    }

    /// Force every episode in the run to one class, or infer per chunk when
    /// `provenance` is `None`. The shape a CLI `--external` flag arrives in.
    #[must_use]
    pub fn with_override(self, provenance: Option<Provenance>) -> Self {
        match provenance {
            Some(class) => self.with_provenance(ProvenancePolicy::Fixed(class)),
            None => self,
        }
    }

    /// Session this text belongs to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Archive log number, when the text came from a numbered archive.
    #[must_use]
    pub fn log_number(&self) -> Option<u32> {
        self.log_number
    }
}

/// Infer authorship from the role headings the archive pipeline writes.
///
/// A chunk is credited to the human only when every role heading in it is a
/// user turn. Anything else — mixed turns, assistant turns, or text with no
/// headings at all (pipeline documents, summaries) — is the agent's own, per
/// the conservative default: never over-credit.
fn infer_from_turn_roles(chunk: &str) -> Provenance {
    let mut saw_user = false;
    for line in chunk.lines() {
        let heading = line.trim().to_lowercase();
        if heading == ASSISTANT_TURN_HEADING {
            return Provenance::SelfGenerated;
        }
        if heading == USER_TURN_HEADING {
            saw_user = true;
        }
    }

    if saw_user {
        Provenance::User
    } else {
        Provenance::SelfGenerated
    }
}

/// Ingest a conversation archive into the knowledge graph.
///
/// Flow:
/// 1. Chunk the conversation text
/// 2. Create an Episode for each chunk, stamped with its provenance (always,
///    even without LLM)
/// 3. If LLM provided: extract entities/relationships, dedup, store
/// 4. Return a report of what was created/merged/skipped
pub async fn ingest_archive(
    gm: &GraphMemory,
    archive_text: &str,
    context: &IngestContext,
    llm: Option<&dyn LlmProvider>,
) -> Result<IngestionReport, GraphError> {
    let mut report = IngestionReport::default();

    let chunks = extract::chunk_conversation(archive_text, 500);
    if chunks.is_empty() {
        return Ok(report);
    }

    // Create episodes for each chunk, each stamped with its own authorship —
    // one archive can hold both the human's words and the agent's.
    for (i, chunk) in chunks.iter().enumerate() {
        let abstract_text = build_episode_abstract(chunk);
        let episode = NewEpisode {
            session_id: context.session_id.clone(),
            abstract_text,
            overview: None,
            content: Some(chunk.clone()),
            log_number: context.log_number,
        };

        match gm
            .add_episode_from(episode, context.provenance.classify(chunk))
            .await
        {
            Ok(_) => report.episodes_created += 1,
            Err(e) => {
                report.errors.push(format!("episode chunk {i}: {e}"));
            }
        }
    }

    // If LLM provided, run extraction on all chunks
    if let Some(llm) = llm {
        process_extraction(gm, &chunks, context, llm, &mut report).await?;
    }

    Ok(report)
}

/// Run LLM extraction on an archive text without creating episodes.
///
/// Use this when episodes already exist (e.g., backfill extraction on
/// previously-ingested archives).
pub async fn extract_from_archive(
    gm: &GraphMemory,
    archive_text: &str,
    context: &IngestContext,
    llm: &dyn LlmProvider,
) -> Result<IngestionReport, GraphError> {
    let mut report = IngestionReport::default();

    let chunks = extract::chunk_conversation(archive_text, 500);
    if chunks.is_empty() {
        return Ok(report);
    }

    process_extraction(gm, &chunks, context, llm, &mut report).await?;

    Ok(report)
}

/// Extract one chunk, tagged with its index.
///
/// A named async fn rather than an inline `async move` block: the inline form
/// makes the resulting stream non-`Send` (the closure would have to implement
/// `FnOnce` for any two lifetimes), and the serve daemon runs ingestion inside
/// a spawned tokio task.
async fn extract_indexed(
    llm: &dyn LlmProvider,
    chunk: &str,
    session_id: &str,
    log_number: Option<u32>,
    index: usize,
) -> (usize, Result<ExtractionResult, GraphError>) {
    let result = extract::extract_from_chunk(llm, chunk, session_id, log_number).await;
    (index, result)
}

/// Shared extraction logic — parallel extraction, sequential dedup.
///
/// Five phases:
/// 1. Extract all chunks in parallel (up to LLM_CONCURRENCY)
/// 2. Local pre-dedup: merge same-name entities from different chunks
/// 3. Dedup sequentially against the DB (each call sees prior results)
/// 4. Create relationships sequentially (fast, no LLM)
/// 5. Record which entities the session touched (passive was-used signal)
async fn process_extraction(
    gm: &GraphMemory,
    chunks: &[String],
    context: &IngestContext,
    llm: &dyn LlmProvider,
    report: &mut IngestionReport,
) -> Result<(), GraphError> {
    let session_id = context.session_id.as_str();
    let log_number = context.log_number;
    // Phase 1: Extract all chunks in parallel.
    // The per-chunk futures are built by the iterator, not by a stream
    // combinator: a closure applied inside the stream would have to be
    // higher-ranked over the item lifetime, which makes the whole stream
    // non-`Send` and would bar ingestion from the serve daemon's tasks.
    let pending: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| extract_indexed(llm, chunk, session_id, log_number, i))
        .collect();
    let extraction_results: Vec<(usize, Result<ExtractionResult, GraphError>)> =
        stream::iter(pending)
            .buffer_unordered(LLM_CONCURRENCY)
            .collect()
            .await;

    // Collect entities and relationships from successful extractions. A
    // relationship keeps the class of the chunk it came out of: the evidence
    // is only ever as independent as the text that produced it.
    let mut all_entities: Vec<ExtractedEntity> = Vec::new();
    let mut all_relationships: Vec<(Provenance, ExtractedRelationship)> = Vec::new();

    for (i, result) in extraction_results {
        match result {
            Ok(extraction) => {
                let provenance = context.provenance.classify(&chunks[i]);
                all_entities.extend(extract::flatten_extraction(&extraction));
                all_relationships.extend(
                    extraction
                        .relationships
                        .into_iter()
                        .map(|rel| (provenance, rel)),
                );
                // Estimate ~2500 tokens per extracted chunk (system prompt + chunk input + output)
                report.estimated_tokens += 2500;
            }
            Err(e) => {
                report.errors.push(format!("extraction chunk {i}: {e}"));
            }
        }
    }

    // Phase 2: Local pre-dedup — merge same-name entities before hitting the DB
    let deduplicated = local_merge_entities(all_entities);

    // Phase 3: Dedup sequentially — each resolve_entity sees the full DB state
    let mut name_map: HashMap<String, String> = HashMap::new();

    for candidate in &deduplicated {
        // Estimate ~600 tokens per dedup call (vector search + LLM decision)
        report.estimated_tokens += 600;
        match dedup::resolve_entity(gm, llm, candidate, session_id).await {
            Ok(ResolvedEntity::Created(entity)) => {
                name_map.insert(candidate.name.clone(), entity.name.clone());
                report.entity_ids.push(entity.id_string());
                report.entities_created += 1;
            }
            Ok(ResolvedEntity::Merged(entity)) => {
                name_map.insert(candidate.name.clone(), entity.name.clone());
                report.entity_ids.push(entity.id_string());
                report.entities_merged += 1;
            }
            Ok(ResolvedEntity::Skipped) => {
                name_map.insert(candidate.name.clone(), candidate.name.clone());
                report.entities_skipped += 1;
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("dedup '{}': {}", candidate.name, e));
            }
        }
    }

    // Phase 4: Create relationships or Bayesian-update existing ones
    for (provenance, rel) in &all_relationships {
        let from_name = name_map.get(&rel.source).unwrap_or(&rel.source);
        let to_name = name_map.get(&rel.target).unwrap_or(&rel.target);

        // Check if a relationship of the same type already exists
        if let Some(existing) =
            find_existing_relationship(gm, from_name, to_name, &rel.rel_type).await
        {
            // Re-extraction is corroborating evidence — worth what its source
            // is worth. Self-authored corroboration also lands in the edge's
            // coherence tally, where it stays visible instead of passing for
            // independent support.
            let mut evidence = existing.edge_evidence();
            evidence.corroborate(*provenance, gm.provenance_weights());
            if let Err(e) =
                crud::reinforce_relationship(gm.db(), &existing.id_string(), evidence).await
            {
                report
                    .errors
                    .push(format!("confidence update {from_name} -> {to_name}: {e}"));
            }
            report.relationships_skipped += 1;
            continue;
        }

        // Parse extraction context from LLM output, default to Inferred
        let context: ExtractionContext = rel
            .confidence
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(ExtractionContext::Inferred);

        let new_rel = NewRelationship {
            from_entity: from_name.clone(),
            to_entity: to_name.clone(),
            rel_type: rel.rel_type.clone(),
            description: rel.description.clone(),
            confidence: Some(context.prior() as f32),
            source: Some(session_id.to_string()),
        };

        match gm.add_relationship(new_rel).await {
            Ok(_) => report.relationships_created += 1,
            Err(e) => {
                report
                    .errors
                    .push(format!("relationship {from_name} -> {to_name}: {e}"));
            }
        }
    }

    // Phase 5: link the session to the entities it touched, so a later
    // outcome knows what it applies to. Bookkeeping, never fatal: a failed
    // link costs the feedback loop one session, not the ingestion.
    if let Err(e) = utility::record_session_use(gm.db(), session_id, &report.entity_ids).await {
        report
            .errors
            .push(format!("session use record for {session_id}: {e}"));
    }

    Ok(())
}

/// Merge extracted entities that share the same name (case-insensitive).
///
/// When multiple chunks extract the same entity, combine their data:
/// - Keep the longest abstract_text
/// - Concatenate overviews
/// - Concatenate content
/// - Deep-merge attributes (later wins on conflict)
/// - First occurrence's entity_type wins
fn local_merge_entities(entities: Vec<ExtractedEntity>) -> Vec<ExtractedEntity> {
    let mut seen: HashMap<String, ExtractedEntity> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for entity in entities {
        let key = entity.name.to_lowercase();
        if let Some(existing) = seen.get_mut(&key) {
            // Keep longer abstract
            if entity.abstract_text.len() > existing.abstract_text.len() {
                existing.abstract_text = entity.abstract_text;
            }
            // Concatenate overviews
            if let Some(new_overview) = entity.overview {
                existing.overview = Some(match &existing.overview {
                    Some(o) => format!("{o}\n\n{new_overview}"),
                    None => new_overview,
                });
            }
            // Concatenate content
            if let Some(new_content) = entity.content {
                existing.content = Some(match &existing.content {
                    Some(c) => format!("{c}\n\n{new_content}"),
                    None => new_content,
                });
            }
            // Merge attributes
            if let Some(new_attrs) = entity.attributes {
                existing.attributes = Some(match &existing.attributes {
                    Some(a) => merge_json(a, &new_attrs),
                    None => new_attrs,
                });
            }
        } else {
            order.push(key.clone());
            seen.insert(key, entity);
        }
    }

    // Preserve insertion order
    order.into_iter().filter_map(|k| seen.remove(&k)).collect()
}

use super::util::merge_json_objects as merge_json;

/// Maximum length of an episode abstract, in characters.
///
/// The abstract is not a label: it is the text that gets embedded, the text
/// episode search returns, and the text that reaches an answer prompt. The
/// previous 200 severed facts mid-word and used a tenth of BGE-Small's 512-token
/// window. 1,000 characters is half the 500-token ingest chunk — still a
/// summary rather than a copy of the chunk — and about 250 tokens, so the whole
/// abstract is inside the embedding window instead of being truncated by it.
const EPISODE_ABSTRACT_MAX_CHARS: usize = 1_000;

/// A boundary earlier than this share of the window discards more text than the
/// tidiness is worth; fall back to a coarser boundary instead.
const MIN_BOUNDARY_FRACTION: f64 = 0.6;

/// Build a short abstract for an episode chunk, cut at a sentence or word
/// boundary so a fact is never severed mid-word.
fn build_episode_abstract(chunk: &str) -> String {
    let trimmed = chunk.trim();
    if trimmed.chars().count() <= EPISODE_ABSTRACT_MAX_CHARS {
        return trimmed.to_string();
    }

    let window: String = trimmed.chars().take(EPISODE_ABSTRACT_MAX_CHARS).collect();
    let cut = truncation_point(&window);
    format!("{}...", window[..cut].trim_end())
}

/// Byte offset to cut `window` at: just past the last sentence terminator if
/// one falls late enough to keep most of the window, else the last word
/// boundary, else the whole window.
fn truncation_point(window: &str) -> usize {
    let floor = (window.len() as f64 * MIN_BOUNDARY_FRACTION) as usize;

    let after_sentence = window
        .char_indices()
        .rev()
        .find(|(_, c)| matches!(c, '.' | '!' | '?' | '\n'))
        .map(|(i, c)| i + c.len_utf8());
    if let Some(cut) = after_sentence.filter(|&cut| cut >= floor) {
        return cut;
    }

    window
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .filter(|&cut| cut >= floor)
        .unwrap_or(window.len())
}

/// Find an existing relationship of the same type between two entities.
/// Returns the full Relationship if found (for Bayesian update).
async fn find_existing_relationship(
    gm: &GraphMemory,
    from_name: &str,
    to_name: &str,
    rel_type: &str,
) -> Option<Relationship> {
    let rels = gm
        .get_relationships(from_name, Direction::Outgoing)
        .await
        .ok()?;
    let to_entity = gm.get_entity(to_name).await.ok()??;
    let to_id = to_entity.id_string();

    rels.into_iter().find(|r| {
        r.rel_type == rel_type && {
            let out_id = match &r.to_id {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out_id == to_id
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_abstract_truncates_at_the_cap() {
        let long = "x".repeat(EPISODE_ABSTRACT_MAX_CHARS * 2);
        let abs = build_episode_abstract(&long);
        assert!(abs.chars().count() <= EPISODE_ABSTRACT_MAX_CHARS + 3);
        assert!(abs.ends_with("..."));
    }

    #[test]
    fn episode_abstract_short_unchanged() {
        let short = "Hello world";
        let abs = build_episode_abstract(short);
        assert_eq!(abs, "Hello world");
    }

    /// A chunk that would have been cut at 200 characters now survives whole.
    #[test]
    fn episode_abstract_keeps_text_the_old_cap_would_have_cut() {
        let chunk = format!(
            "{} Currently, my favourite is Kansas City Masterpiece.",
            "Padding sentence about barbecue. ".repeat(8)
        );
        assert!(chunk.chars().count() > 200);
        let abs = build_episode_abstract(&chunk);
        assert!(abs.contains("Kansas City Masterpiece"));
    }

    /// The defect this pins: the old cap severed `Kansas City Masterpiece`
    /// after `Ka`. Cuts must land on a sentence boundary when one is available.
    #[test]
    fn episode_abstract_cuts_at_a_sentence_boundary() {
        let chunk = format!(
            "{}My favourite is Kansas City Masterpiece and nothing else comes close at all",
            "The barbecue discussion continued at length. ".repeat(22)
        );
        let abs = build_episode_abstract(&chunk);
        assert!(abs.ends_with("at length...."), "unexpected tail: {abs:?}");
        assert!(!abs.contains("Kansas"));
    }

    /// With no sentence terminator in reach, the cut still lands between words.
    #[test]
    fn episode_abstract_never_cuts_mid_word() {
        let chunk = "barbecue ".repeat(300);
        let abs = build_episode_abstract(&chunk);
        let body = abs.strip_suffix("...").expect("truncated");
        assert!(
            body.ends_with("barbecue"),
            "cut landed mid-word: {:?}",
            &body[body.len().saturating_sub(20)..]
        );
    }

    #[test]
    fn user_only_chunk_is_credited_to_the_human() {
        let chunk = "### User\n\nI moved the repo to /opt/recall-echo.";
        assert_eq!(infer_from_turn_roles(chunk), Provenance::User);
    }

    #[test]
    fn assistant_turns_make_a_chunk_self_authored() {
        let chunk = "### Assistant\n\nThe repo now lives at /opt/recall-echo.";
        assert_eq!(infer_from_turn_roles(chunk), Provenance::SelfGenerated);
    }

    #[test]
    fn mixed_chunk_is_self_authored() {
        // The conservative half of the rule: a chunk the agent contributed to
        // cannot be counted as independent testimony.
        let chunk = "### User\n\nWhere does it live?\n\n---\n\n### Assistant\n\n/opt.";
        assert_eq!(infer_from_turn_roles(chunk), Provenance::SelfGenerated);
    }

    #[test]
    fn text_without_role_headings_is_self_authored() {
        let chunk = "A pipeline document with no conversation structure at all.";
        assert_eq!(infer_from_turn_roles(chunk), Provenance::SelfGenerated);
    }

    #[test]
    fn heading_matching_is_exact() {
        // "### Users of the system" is a topic, not a turn.
        let chunk = "### Users of the system\n\nThey prefer NeoVim.";
        assert_eq!(infer_from_turn_roles(chunk), Provenance::SelfGenerated);
    }

    #[test]
    fn fixed_policy_overrides_turn_roles() {
        let chunk = "### User\n\nA quote from a paper.";
        let policy = ProvenancePolicy::Fixed(Provenance::External);
        assert_eq!(policy.classify(chunk), Provenance::External);
        assert_eq!(
            ProvenancePolicy::FromTurnRoles.classify(chunk),
            Provenance::User
        );
    }

    #[test]
    fn context_override_is_applied_only_when_present() {
        let context = IngestContext::new("s1", Some(7));
        assert_eq!(context.session_id(), "s1");
        assert_eq!(context.log_number(), Some(7));

        let inferring = context.clone().with_override(None);
        assert_eq!(inferring.provenance, ProvenancePolicy::FromTurnRoles);

        let forced = context.with_override(Some(Provenance::External));
        assert_eq!(
            forced.provenance,
            ProvenancePolicy::Fixed(Provenance::External)
        );
    }
}
