// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a fact script into archives, ingest them, and grade the store.
//!
//! Archives go through the same renderer ingestion already reads, so turn
//! headings survive and provenance is classified the way a real session is.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;

use super::grade::{self, ExtractorLabel, ObservedEdge, SessionObservation, WriteReport};
use super::script::{Claim, ClaimOp, FactScript, ScriptFact, ScriptSession, Side, Speaker};
use crate::archive::{append_index, highest_conversation_number};
use crate::conversation::{self, conversation_to_markdown, Conversation, ConversationEntry};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
use crate::graph::llm::{Completion, LlmProvider};
use crate::graph::types::{ExtractedEntity, ExtractedRelationship, ExtractionResult};
use crate::graph::{GraphMemory, IngestContext};
use crate::search;
use crate::tags;

const DURATION: &str = "session";
const SOURCE: &str = "writes";

/// Where the claims to ingest come from.
pub enum Extractor<'a> {
    /// Replay the claims in the script. Makes no network call.
    Scripted,
    /// A caller-supplied provider. The test run does not use this.
    Live(&'a dyn LlmProvider),
}

/// Refuse `path` unless it is missing or an empty directory.
///
/// The harness writes archives and a graph. A directory that already holds
/// anything is left untouched, so a real store cannot be graded by accident.
pub fn require_scratch(path: &Path) -> Result<(), RecallError> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Err(RecallError::Config(format!(
            "{} is not a directory; bench writes needs a new or empty directory",
            path.display()
        )));
    }
    if fs::read_dir(path)?.next().is_some() {
        return Err(RecallError::Config(format!(
            "{} is not empty; bench writes needs a new or empty directory",
            path.display()
        )));
    }
    Ok(())
}

/// Ingest `script` into `work_dir` and grade the resulting graph.
///
/// `work_dir` must be missing or empty. The function creates `memory/` under it.
pub async fn run(
    script: &FactScript,
    work_dir: &Path,
    extractor: Extractor<'_>,
) -> Result<WriteReport, RecallError> {
    require_scratch(work_dir)?;
    let memory_dir = work_dir.join("memory");
    let written = write_archives(&memory_dir, script)?;
    let gm = GraphMemory::open(&memory_dir.join("graph")).await?;
    let weights = *gm.provenance_weights();
    let (label, provider) = provider_for(script, extractor)?;

    let mut before = snapshot(&gm).await?;
    let mut sessions = Vec::with_capacity(written.len());
    for archive in &written {
        let text = fs::read_to_string(&archive.path)?;
        let calls_before = provider.calls();
        let started = Instant::now();
        let report = gm
            .ingest_archive(
                &text,
                &IngestContext::new(&archive.id, Some(archive.log_number)),
                Some(provider.as_provider()),
            )
            .await?;
        let calls = provider.calls().saturating_sub(calls_before);
        let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let after = snapshot(&gm).await?;
        sessions.push(SessionObservation {
            id: archive.id.clone(),
            before,
            after: after.clone(),
            calls,
            millis,
            warnings: report.errors,
        });
        before = after;
    }

    let keyword_hits = script
        .questions
        .iter()
        .map(|question| keyword_hit(&memory_dir, &question.text, &question.answer))
        .collect::<Result<Vec<_>, _>>()?;
    grade::grade(script, &sessions, &weights, &keyword_hits, label)
}

/// Whether keyword search returns an archive that contains `answer`.
pub(crate) fn keyword_hit(
    memory_dir: &Path,
    question: &str,
    answer: &str,
) -> Result<bool, RecallError> {
    let ranked = search::ranked_search(question, memory_dir, 10)?;
    let answer = answer.to_lowercase();
    let conversations = memory_dir.join("conversations");
    for hit in ranked {
        let text = fs::read_to_string(conversations.join(&hit.file))?;
        if text.to_lowercase().contains(&answer) {
            return Ok(true);
        }
    }
    Ok(false)
}

struct WrittenArchive {
    id: String,
    log_number: u32,
    path: PathBuf,
}

fn write_archives(
    memory_dir: &Path,
    script: &FactScript,
) -> Result<Vec<WrittenArchive>, RecallError> {
    let conversations = memory_dir.join("conversations");
    fs::create_dir_all(&conversations)?;
    fs::create_dir_all(memory_dir.join("graph"))?;
    let memory_md = memory_dir.join("MEMORY.md");
    if !memory_md.exists() {
        fs::write(&memory_md, "")?;
    }

    let mut written = Vec::with_capacity(script.sessions.len());
    for session in &script.sessions {
        written.push(write_session(memory_dir, script, session)?);
    }
    Ok(written)
}

fn write_session(
    memory_dir: &Path,
    script: &FactScript,
    session: &ScriptSession,
) -> Result<WrittenArchive, RecallError> {
    let conversations = memory_dir.join("conversations");
    let log_number = highest_conversation_number(&conversations) + 1;
    let conversation = to_conversation(session);
    let message_count = conversation.total_messages();
    let topics = vec![script.name.clone(), session.id.clone()];
    let frontmatter = Frontmatter {
        log: log_number,
        date: session.date.clone(),
        session_id: session.id.clone(),
        message_count,
        duration: DURATION.to_string(),
        source: SOURCE.to_string(),
        pulse: None,
        topics: topics.clone(),
    };
    let body = conversation_to_markdown(&conversation, log_number);
    let tags = tags::format_tags_section(&tags::extract_tags(&conversation.entries));
    let content = format!("{}\n\n{}\n{}", frontmatter.render(), body, tags);
    let path = conversations.join(format!("conversation-{log_number:03}.md"));
    fs::write(&path, &content)?;

    let date = conversation::date_from_timestamp(&session.date);
    append_index(
        &memory_dir.join("ARCHIVE.md"),
        log_number,
        &date,
        &session.id,
        &topics,
        message_count,
        DURATION,
    )?;
    ephemeral::append_entry(
        &memory_dir.join("EPHEMERAL.md"),
        &EphemeralEntry {
            session_id: session.id.clone(),
            date: session.date.clone(),
            duration: DURATION.to_string(),
            message_count,
            archive_file: format!("conversation-{log_number:03}.md"),
            summary: format!("session {}", session.id),
        },
    )?;
    let cfg = crate::config::load_from_dir(memory_dir);
    ephemeral::trim_to_limit(&memory_dir.join("EPHEMERAL.md"), cfg.ephemeral.max_entries)?;

    Ok(WrittenArchive {
        id: session.id.clone(),
        log_number,
        path,
    })
}

fn to_conversation(session: &ScriptSession) -> Conversation {
    let mut user_count = 0;
    let mut assistant_count = 0;
    let mut entries = Vec::with_capacity(session.turns.len());
    for turn in &session.turns {
        match turn.role {
            Speaker::User => {
                user_count += 1;
                entries.push(ConversationEntry::UserMessage(turn.text.clone()));
            }
            Speaker::Assistant => {
                assistant_count += 1;
                entries.push(ConversationEntry::AssistantText(turn.text.clone()));
            }
        }
    }
    Conversation {
        session_id: session.id.clone(),
        first_timestamp: Some(session.date.clone()),
        last_timestamp: Some(session.date.clone()),
        user_message_count: user_count,
        assistant_message_count: assistant_count,
        entries,
    }
}

async fn snapshot(gm: &GraphMemory) -> Result<Vec<ObservedEdge>, RecallError> {
    // Names live on the entity records, not on the edge. The same lookup the
    // inspect path uses keeps a dangling id visible instead of dropping it.
    let relationships = crate::graph::crud::list_all_relationships(gm.db()).await?;
    let mut names = crate::graph::edge_view::NameCache::new();
    let mut edges = Vec::with_capacity(relationships.len());
    for relationship in &relationships {
        let from_id = crate::graph::edge_view::record_id(&relationship.from_id);
        let to_id = crate::graph::edge_view::record_id(&relationship.to_id);
        let evidence = relationship.evidence();
        edges.push(ObservedEdge {
            id: relationship.id_string(),
            subject: names.name_of(gm.db(), &from_id).await?,
            relation: relationship.rel_type.clone(),
            object: names.name_of(gm.db(), &to_id).await?,
            live: relationship.valid_until.is_none(),
            alpha: evidence.alpha(),
            beta: evidence.beta(),
            self_reinforcements: relationship.self_reinforcements.unwrap_or(0).max(0),
        });
    }
    Ok(edges)
}

fn provider_for<'a>(
    script: &FactScript,
    extractor: Extractor<'a>,
) -> Result<(ExtractorLabel, Counting<'a>), RecallError> {
    let (label, inner): (ExtractorLabel, Box<dyn LlmProvider + 'a>) = match extractor {
        Extractor::Scripted => (
            ExtractorLabel::Scripted,
            Box::new(ScriptedExtractor::new(script)?),
        ),
        Extractor::Live(provider) => (
            ExtractorLabel::Provider,
            Box::new(BorrowedProvider(provider)),
        ),
    };
    Ok((label, Counting::new(inner)))
}

struct Counting<'a> {
    inner: Box<dyn LlmProvider + 'a>,
    calls: AtomicU64,
}

impl<'a> Counting<'a> {
    fn new(inner: Box<dyn LlmProvider + 'a>) -> Self {
        Self {
            inner,
            calls: AtomicU64::new(0),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn as_provider(&self) -> &(dyn LlmProvider + 'a) {
        self
    }
}

#[async_trait]
impl LlmProvider for Counting<'_> {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, crate::graph::error::GraphError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner
            .complete(system_prompt, user_message, max_tokens)
            .await
    }

    async fn complete_measured(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<Completion, crate::graph::error::GraphError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner
            .complete_measured(system_prompt, user_message, max_tokens)
            .await
    }
}

struct BorrowedProvider<'a>(&'a dyn LlmProvider);

#[async_trait]
impl LlmProvider for BorrowedProvider<'_> {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String, crate::graph::error::GraphError> {
        self.0
            .complete(system_prompt, user_message, max_tokens)
            .await
    }

    async fn complete_measured(
        &self,
        system_prompt: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<Completion, crate::graph::error::GraphError> {
        self.0
            .complete_measured(system_prompt, user_message, max_tokens)
            .await
    }
}

/// Replays the script's claims.
///
/// The provider trait only receives prompt text, so a dedup prompt is told
/// apart by the sentence `graph::dedup` opens with. Anything else is treated
/// as extraction and answered from the turn whose text appears in the prompt.
struct ScriptedExtractor {
    prepared: Vec<Prepared>,
}

struct Prepared {
    needle: String,
    body: ExtractionResult,
}

impl ScriptedExtractor {
    fn new(script: &FactScript) -> Result<Self, RecallError> {
        let mut prepared = Vec::new();
        for session in &script.sessions {
            for turn in &session.turns {
                if turn.claims.is_empty() {
                    continue;
                }
                let mut body = ExtractionResult::default();
                for claim in &turn.claims {
                    body.append(extraction_for(script, claim)?);
                }
                prepared.push(Prepared {
                    needle: turn.text.clone(),
                    body,
                });
            }
        }
        Ok(Self { prepared })
    }

    fn extraction_json(
        &self,
        user_message: &str,
    ) -> Result<String, crate::graph::error::GraphError> {
        let mut result = ExtractionResult::default();
        for prepared in &self.prepared {
            if user_message.contains(&prepared.needle) {
                result.append(prepared.body.clone());
            }
        }
        serde_json::to_string(&result).map_err(crate::graph::error::GraphError::from)
    }
}

#[async_trait]
impl LlmProvider for ScriptedExtractor {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        _max_tokens: u32,
    ) -> Result<String, crate::graph::error::GraphError> {
        if system_prompt.contains(crate::graph::dedup::DEDUP_SYSTEM_PROMPT) {
            return Ok(r#"{"decision":"create","reason":"distinct"}"#.to_string());
        }
        self.extraction_json(user_message)
    }
}

fn extraction_for(script: &FactScript, claim: &Claim) -> Result<ExtractionResult, RecallError> {
    let fact_id = match claim.op {
        ClaimOp::Supersede => claim.replacement.as_deref().ok_or_else(|| {
            RecallError::Config(format!("supersede of '{}' has no replacement", claim.fact))
        })?,
        _ => claim.fact.as_str(),
    };
    let fact = script.fact(fact_id)?;
    Ok(ExtractionResult {
        entities: vec![
            extracted_entity(fact, Side::Subject)?,
            extracted_entity(fact, Side::Object)?,
        ],
        relationships: vec![ExtractedRelationship {
            source: fact.subject.clone(),
            target: fact.object.clone(),
            rel_type: fact.relation.clone(),
            description: Some(fact.id.clone()),
            confidence: Some("explicit".to_string()),
        }],
        ..ExtractionResult::default()
    })
}

fn extracted_entity(fact: &ScriptFact, side: Side) -> Result<ExtractedEntity, RecallError> {
    let name = match side {
        Side::Subject => fact.subject.clone(),
        Side::Object => fact.object.clone(),
    };
    Ok(ExtractedEntity {
        name,
        entity_type: fact.type_for(side)?,
        abstract_text: fact.abstract_for(side),
        overview: None,
        content: None,
        attributes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::extract::chunk_conversation;
    use crate::graph::{Provenance, ProvenancePolicy};

    fn history() -> FactScript {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/writes/history.toml");
        crate::bench::writes::load_script(&path).unwrap()
    }

    #[test]
    fn a_missing_or_empty_directory_is_a_scratch() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(require_scratch(tmp.path()).is_ok());
        assert!(require_scratch(&tmp.path().join("absent")).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_populated_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("keep"), "real").unwrap();
        let err = run(&history(), tmp.path(), Extractor::Scripted)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
        assert!(!tmp.path().join("memory").exists());

        let file = tmp.path().join("keep");
        let err = require_scratch(&file).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn archives_keep_the_headings_ingestion_classifies() {
        let script = history();
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        let written = write_archives(&memory, &script).unwrap();
        assert_eq!(written.len(), script.sessions.len());

        let user = fs::read_to_string(&written[0].path).unwrap();
        let user_chunk = chunk_conversation(&user, 500)
            .into_iter()
            .find(|chunk| chunk.contains("The deploy host is pine."))
            .expect("the turn survives chunking");
        assert_eq!(
            ProvenancePolicy::default().classify(&user_chunk),
            Provenance::User
        );
        let frontmatter = crate::frontmatter::parse(&user).expect("frontmatter");
        assert_eq!(frontmatter.session_id, "s1");
        assert_eq!(frontmatter.source, SOURCE);

        let assistant = fs::read_to_string(&written[2].path).unwrap();
        let assistant_chunk = chunk_conversation(&assistant, 500)
            .into_iter()
            .find(|chunk| chunk.contains("Noted that the deploy host is pine."))
            .expect("the turn survives chunking");
        assert_eq!(
            ProvenancePolicy::default().classify(&assistant_chunk),
            Provenance::SelfGenerated
        );
        assert!(assistant.contains("### Assistant"));
    }

    #[test]
    fn keyword_hit_requires_the_answer_in_a_returned_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        let dir = memory.join("conversations");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("conversation-001.md"),
            "# Log\n\nThe deploy host changed to oak.\n",
        )
        .unwrap();
        fs::write(
            dir.join("conversation-002.md"),
            "# Log\n\nThe kettle boiled.\n",
        )
        .unwrap();
        assert!(keyword_hit(memory, "what changed about the host", "oak").unwrap());
        assert!(!keyword_hit(memory, "what changed about the host", "marble").unwrap());

        fs::write(
            dir.join("conversation-001.md"),
            "# Log\n\nThe weather changed overnight.\n",
        )
        .unwrap();
        fs::write(dir.join("conversation-002.md"), "# Log\n\noak is unread.\n").unwrap();
        assert!(!keyword_hit(memory, "what changed about the weather", "oak").unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_extractor_replays_the_claim_and_refuses_a_merge() {
        let script = history();
        let extractor = ScriptedExtractor::new(&script).unwrap();
        let dedup = extractor
            .complete(
                crate::graph::dedup::DEDUP_SYSTEM_PROMPT,
                "The deploy host is pine.",
                300,
            )
            .await
            .unwrap();
        assert!(dedup.contains("\"create\""), "{dedup}");

        // A reworded prompt that still says the old phrase is extraction, not dedup.
        let loose = extractor
            .complete("deduplication system", "The deploy host is pine.", 300)
            .await
            .unwrap();
        assert!(loose.contains("\"source\""), "{loose}");
        assert!(!loose.contains("\"decision\""), "{loose}");

        let extracted = extractor
            .complete(
                "Extract entities and relationships.",
                "before The deploy host is pine. after",
                800,
            )
            .await
            .unwrap();
        let parsed: ExtractionResult = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed.relationships.len(), 1);
        assert_eq!(parsed.relationships[0].source, "deploy");
        assert_eq!(parsed.relationships[0].target, "pine");
        assert_eq!(parsed.relationships[0].rel_type, "setting");

        let empty = extractor
            .complete("Extract entities.", "The kettle is unrelated.", 800)
            .await
            .unwrap();
        let parsed: ExtractionResult = serde_json::from_str(&empty).unwrap();
        assert!(parsed.relationships.is_empty());
        assert!(parsed.entities.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_run_classifies_every_claim() {
        let script = history();
        let tmp = tempfile::tempdir().unwrap();
        let report = run(&script, tmp.path(), Extractor::Scripted).await.unwrap();

        assert_eq!(report.extractor, ExtractorLabel::Scripted);
        assert_eq!(report.sessions.len(), script.sessions.len());
        assert_eq!(report.steps.len(), 7);
        assert_eq!(report.questions.len(), 2);
        assert!(report.sessions.iter().all(|session| session.calls >= 1));
        assert_eq!(
            report.calls,
            report
                .sessions
                .iter()
                .map(|session| session.calls)
                .sum::<u64>()
        );
        assert_eq!(
            report.millis,
            report
                .sessions
                .iter()
                .map(|session| session.millis)
                .sum::<u64>()
        );
        // Pre-redesign baseline for history.toml. Supersede, contradict and
        // reject do not land on today's write path. These counts are expected
        // to change when that path changes; update them in the same commit.
        assert_eq!(report.operations.create, counted(1, 1, 0));
        assert_eq!(report.operations.corroborate, counted(2, 1, 0));
        assert_eq!(report.operations.supersede, counted(0, 0, 1));
        assert_eq!(report.operations.contradict, counted(0, 0, 1));
        assert_eq!(report.operations.no_op, counted(1, 0, 1));
        assert_eq!(report.operations.removed, counted(0, 0, 0));
        assert_eq!(report.operations.revived, counted(0, 0, 0));
        assert_eq!(
            report.wrong_facts,
            vec![
                grade::WrongFact {
                    fact: "deploy-host".into(),
                    reason: grade::WrongReason::StillLive,
                },
                grade::WrongFact {
                    fact: "deploy-host-birch".into(),
                    reason: grade::WrongReason::StillLive,
                },
            ]
        );
        assert!(report.junk_edges.is_empty());
        assert_eq!(
            report
                .steps
                .iter()
                .map(|step| (step.expected, step.actual, step.matched))
                .collect::<Vec<_>>(),
            vec![
                (grade::WriteOp::Create, grade::WriteOp::Create, true),
                (
                    grade::WriteOp::Corroborate,
                    grade::WriteOp::Corroborate,
                    true,
                ),
                (
                    grade::WriteOp::Corroborate,
                    grade::WriteOp::Corroborate,
                    true,
                ),
                (grade::WriteOp::Supersede, grade::WriteOp::Create, false),
                (
                    grade::WriteOp::Contradict,
                    grade::WriteOp::Corroborate,
                    false
                ),
                (grade::WriteOp::NoOp, grade::WriteOp::Create, false),
                (grade::WriteOp::NoOp, grade::WriteOp::NoOp, true),
            ]
        );
        assert!(!report.questions[0].graph_hit);
        assert!(report.questions[0].keyword_hit);
        assert!(report.questions[1].graph_hit);
        assert!(report.questions[1].keyword_hit);
    }

    fn counted(tp: u32, fp: u32, missed: u32) -> grade::OpScore {
        grade::OpScore {
            tp,
            fp,
            missed,
            precision: (tp + fp > 0).then(|| f64::from(tp) / f64::from(tp + fp)),
            recall: (tp + missed > 0).then(|| f64::from(tp) / f64::from(tp + missed)),
        }
    }
}
