// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a fact script into archives, ingest them, and grade the store.
//!
//! Archives go through the same renderer ingestion already reads, so turn
//! headings survive and provenance is classified the way a real session is.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;

use super::grade::{self, ExtractorLabel, ObservedEdge, SessionObservation, WriteReport};
use super::script::{Claim, ClaimOp, FactScript, ScriptFact, ScriptSession, Side, Speaker};
use crate::archive::{append_index, highest_conversation_number};
use crate::conversation::{self, conversation_to_markdown, Conversation, ConversationEntry};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
use crate::graph::confidence::PRIOR_CONCENTRATION;
use crate::graph::llm::{Completion, LlmProvider};
use crate::graph::types::{ExtractedEntity, ExtractedRelationship, ExtractionResult};
use crate::graph::{GraphMemory, IngestContext, PipelineVersion};
use crate::search;
use crate::tags;

const DURATION: &str = "session";
const SOURCE: &str = "writes";

/// Where the claims to ingest come from.
#[derive(Clone, Copy)]
pub enum Extractor<'a> {
    /// Replay the claims in the script. Makes no provider call.
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
///
/// Session `millis` is ingest time. Model load is measured first and reported
/// as `setup_millis`, so the first session is not charged for it.
pub async fn run(
    script: &FactScript,
    work_dir: &Path,
    extractor: Extractor<'_>,
    pipeline: PipelineVersion,
) -> Result<WriteReport, RecallError> {
    require_scratch(work_dir)?;
    let memory_dir = work_dir.join("memory");
    let written = write_archives(&memory_dir, script)?;
    let gm = GraphMemory::open(&memory_dir.join("graph")).await?;
    let weights = *gm.provenance_weights();
    let (label, provider) = provider_for(script, extractor)?;

    // The first embed loads the model. Do that outside the session clock.
    let setup_started = Instant::now();
    gm.embedder()?;
    let setup_millis = u64::try_from(setup_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let sessions = collect_sessions(&gm, &memory_dir, &written, &provider, pipeline).await?;

    let keyword_hits = script
        .questions
        .iter()
        .map(|question| keyword_hit(&memory_dir, &question.text, &question.answer))
        .collect::<Result<Vec<_>, _>>()?;
    let mut report =
        grade::grade_with_pipeline(script, &sessions, &weights, &keyword_hits, label, pipeline)?;
    report.setup_millis = setup_millis;
    Ok(report)
}

/// Ingest each archive and snapshot the graph around it.
///
/// One map of β at first sight covers the whole run. Creation writes
/// α + β = `PRIOR_CONCENTRATION` and only a contradiction moves β, so that
/// first β is the starting prior on every later snapshot of the edge.
async fn collect_sessions(
    gm: &GraphMemory,
    memory_dir: &Path,
    written: &[WrittenArchive],
    provider: &Counting<'_>,
    pipeline: PipelineVersion,
) -> Result<Vec<SessionObservation>, RecallError> {
    let mut first_beta = HashMap::new();
    let mut before = snapshot(gm, &mut first_beta).await?;
    let mut sessions = Vec::with_capacity(written.len());
    for archive in written {
        let text = fs::read_to_string(&archive.path)?;
        let calls_before = provider.calls();
        let started = Instant::now();
        let context = IngestContext::new(&archive.id, Some(archive.log_number));
        let report = match pipeline {
            PipelineVersion::V1 => {
                gm.ingest_archive(&text, &context, Some(provider.as_provider()))
                    .await?
            }
            PipelineVersion::V2 => {
                // Episodes stay a separate step, with no model call. The
                // pass-1 calls are the only ones the run is billed for.
                let mut report = gm.ingest_archive(&text, &context, None).await?;
                let extracted = crate::graph::v2::extract_log(
                    gm,
                    memory_dir,
                    archive.log_number,
                    &text,
                    &context,
                    provider.as_provider(),
                    PipelineVersion::V2,
                )
                .await?;
                report.errors.extend(extracted.errors);
                report.entities_created += extracted.entities_created;
                report.relationships_created += extracted.relationships_created;
                report.slots_written += extracted.slots_written;
                report.estimated_tokens += extracted.estimated_tokens;
                report.measured_tokens += extracted.measured_tokens;
                report.llm_calls += extracted.llm_calls;
                report.windows = extracted.windows;
                report.candidates = extracted.candidates;
                report.kept = extracted.kept;
                report.drops = extracted.drops;
                report.resolve = extracted.resolve;
                report.retirements = extracted.retirements;
                report.retirements_skipped = extracted.retirements_skipped;
                report.timing = extracted.timing;
                report
            }
        };
        let calls = provider.calls().saturating_sub(calls_before);
        let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let after = snapshot(gm, &mut first_beta).await?;
        sessions.push(SessionObservation {
            id: archive.id.clone(),
            before,
            after: after.clone(),
            calls,
            millis,
            warnings: report.errors,
            windows: report.windows,
            candidates: report.candidates,
            kept: report.kept,
            drops: report.drops,
            resolve: report.resolve,
            retirements: report.retirements,
            retirements_skipped: report.retirements_skipped,
            timing: report.timing,
        });
        before = after;
    }
    Ok(sessions)
}

/// Grade `script` `repeat` times, each in a fresh `parent/run-<k>` directory.
///
/// `parent` must be missing or empty. Nothing is created when it is not.
/// `k` starts at 1. One run still uses `run-1`; the command prints a single
/// report itself when repeat is 1.
pub async fn run_repeat(
    script: &FactScript,
    parent: &Path,
    repeat: usize,
    extractor: Extractor<'_>,
    pipeline: PipelineVersion,
) -> Result<RepeatReport, RecallError> {
    if repeat == 0 {
        return Err(RecallError::Config("repeat must be at least 1".to_string()));
    }
    require_scratch(parent)?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
    }
    let mut runs = Vec::with_capacity(repeat);
    for k in 1..=repeat {
        let dir = parent.join(format!("run-{k}"));
        fs::create_dir(&dir)?;
        runs.push(run(script, &dir, extractor, pipeline).await?);
    }
    Ok(RepeatReport::from_runs(runs))
}

/// N fresh runs, plus the median and the min/max of each graded count.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepeatReport {
    pub runs: Vec<WriteReport>,
    pub median: MetricSet,
    pub range: MetricRange,
}

impl RepeatReport {
    fn from_runs(runs: Vec<WriteReport>) -> Self {
        let metrics: Vec<MetricSet> = runs.iter().map(MetricSet::from_report).collect();
        let (median, range) = summarize(&metrics);
        Self {
            runs,
            median,
            range,
        }
    }
}

/// One run's pattern counts and its per-session call and time rates.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricSet {
    pub found: f64,
    pub complete: f64,
    pub wrong: f64,
    pub missing: f64,
    pub retired: f64,
    pub premature: f64,
    pub noise_hits: f64,
    pub unmatched: f64,
    pub calls_per_session: f64,
    pub ms_per_session: f64,
}

/// Min and max of one metric across the repeated runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricSpan {
    pub min: f64,
    pub max: f64,
}

/// Min and max of every [`MetricSet`] field.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricRange {
    pub found: MetricSpan,
    pub complete: MetricSpan,
    pub wrong: MetricSpan,
    pub missing: MetricSpan,
    pub retired: MetricSpan,
    pub premature: MetricSpan,
    pub noise_hits: MetricSpan,
    pub unmatched: MetricSpan,
    pub calls_per_session: MetricSpan,
    pub ms_per_session: MetricSpan,
}

impl MetricSet {
    fn from_report(report: &WriteReport) -> Self {
        let patterns = report.patterns.as_ref();
        let count = |pick: fn(&super::grade::PatternScore) -> u32| {
            patterns.map(|score| f64::from(pick(score))).unwrap_or(0.0)
        };
        let sessions = report.sessions.len();
        let per_session = |total: u64| {
            if sessions == 0 {
                0.0
            } else {
                total as f64 / sessions as f64
            }
        };
        Self {
            found: count(|score| score.found.count),
            complete: count(|score| score.complete.count),
            wrong: count(|score| score.wrong.count),
            missing: count(|score| score.missing.count),
            retired: count(|score| score.retired.count),
            premature: count(|score| score.premature.count),
            noise_hits: count(|score| score.noise_hits.count),
            unmatched: count(|score| score.unmatched.count),
            calls_per_session: per_session(
                report.sessions.iter().map(|session| session.calls).sum(),
            ),
            ms_per_session: per_session(report.sessions.iter().map(|session| session.millis).sum()),
        }
    }
}

fn summarize(metrics: &[MetricSet]) -> (MetricSet, MetricRange) {
    (
        MetricSet {
            found: median(&column(metrics, |item| item.found)),
            complete: median(&column(metrics, |item| item.complete)),
            wrong: median(&column(metrics, |item| item.wrong)),
            missing: median(&column(metrics, |item| item.missing)),
            retired: median(&column(metrics, |item| item.retired)),
            premature: median(&column(metrics, |item| item.premature)),
            noise_hits: median(&column(metrics, |item| item.noise_hits)),
            unmatched: median(&column(metrics, |item| item.unmatched)),
            calls_per_session: median(&column(metrics, |item| item.calls_per_session)),
            ms_per_session: median(&column(metrics, |item| item.ms_per_session)),
        },
        MetricRange {
            found: span(&column(metrics, |item| item.found)),
            complete: span(&column(metrics, |item| item.complete)),
            wrong: span(&column(metrics, |item| item.wrong)),
            missing: span(&column(metrics, |item| item.missing)),
            retired: span(&column(metrics, |item| item.retired)),
            premature: span(&column(metrics, |item| item.premature)),
            noise_hits: span(&column(metrics, |item| item.noise_hits)),
            unmatched: span(&column(metrics, |item| item.unmatched)),
            calls_per_session: span(&column(metrics, |item| item.calls_per_session)),
            ms_per_session: span(&column(metrics, |item| item.ms_per_session)),
        },
    )
}

fn column(metrics: &[MetricSet], pick: impl Fn(&MetricSet) -> f64) -> Vec<f64> {
    metrics.iter().map(pick).collect()
}

/// Median. An even count averages the two central values.
fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

fn span(values: &[f64]) -> MetricSpan {
    let min = values.iter().copied().reduce(f64::min).unwrap_or(0.0);
    let max = values.iter().copied().reduce(f64::max).unwrap_or(0.0);
    MetricSpan { min, max }
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
    let conversation = load_conversation(session)?;
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
    let cwd = crate::archive::resolve_cwd(None, session.transcript_path.as_deref(), None);
    crate::archive::write_sidecar_best_effort(
        memory_dir,
        log_number,
        &session.id,
        &date,
        cwd.as_deref(),
        &conversation.entries,
    );
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

fn load_conversation(session: &ScriptSession) -> Result<Conversation, RecallError> {
    let Some(path) = &session.transcript_path else {
        if session.transcript.is_some() {
            return Err(RecallError::Config(format!(
                "session '{}' transcript was not resolved; load the script from its file",
                session.id
            )));
        }
        return Ok(to_conversation(session));
    };
    let path = path.to_str().ok_or_else(|| {
        RecallError::Config(format!(
            "session '{}' transcript path is not utf-8",
            session.id
        ))
    })?;
    crate::jsonl::parse_transcript(path, &session.id)
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

async fn snapshot(
    gm: &GraphMemory,
    first_beta: &mut HashMap<String, f64>,
) -> Result<Vec<ObservedEdge>, RecallError> {
    // Names live on the entity records, not on the edge. The same lookup the
    // inspect path uses keeps a dangling id visible instead of dropping it.
    let relationships = crate::graph::crud::list_all_relationships(gm.db()).await?;
    let edge_evidence = edge_evidence(gm).await?;
    let mut names = crate::graph::edge_view::NameCache::new();
    let mut edges = Vec::with_capacity(relationships.len());
    for relationship in &relationships {
        let from_id = crate::graph::edge_view::record_id(&relationship.from_id);
        let to_id = crate::graph::edge_view::record_id(&relationship.to_id);
        let evidence = relationship.evidence();
        let id = relationship.id_string();
        let beta = evidence.beta();
        let prior_alpha = starting_alpha(&id, beta, first_beta);
        let (assistant_steps, self_steps) = corroboration_steps(edge_evidence.get(&id));
        edges.push(ObservedEdge {
            id,
            subject: names.name_of(gm.db(), &from_id).await?,
            relation: relationship.rel_type.clone(),
            object: names.name_of(gm.db(), &to_id).await?,
            live: relationship.valid_until.is_none(),
            alpha: evidence.alpha(),
            beta,
            self_reinforcements: relationship.self_reinforcements.unwrap_or(0).max(0),
            prior_alpha,
            assistant_steps,
            self_steps,
        });
    }
    // Slots are the literal facts. Rejected rows stay in the store and out of
    // the diff, so a rejection is a no-op rather than a new record. Retired
    // decided and considered rows stay visible with live = false.
    let entities = crate::graph::crud::list_entities(gm.db(), None).await?;
    for entity in &entities {
        let Some(list) = entity
            .attributes
            .as_ref()
            .and_then(|attrs| attrs.get("v2"))
            .and_then(|v2| v2.get("slots"))
            .and_then(|slots| slots.as_array())
        else {
            continue;
        };
        let mut seen = HashMap::new();
        for slot in list {
            let status = slot
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if status != "decided" && status != "considered" {
                continue;
            }
            let predicate = slot
                .get("predicate")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let key = slot
                .get("key")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let value = slot
                .get("value")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let valid_from = slot
                .get("valid_from")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let mut id = format!(
                "{}|{predicate}|{key}|{value}|{valid_from}",
                entity.id_string()
            );
            let count = seen.entry(id.clone()).or_insert(0u32);
            *count += 1;
            if *count > 1 {
                id = format!("{id}|{count}");
            }
            let beta = json_number(slot.get("beta"));
            let prior_alpha = starting_alpha(&id, beta, first_beta);
            let until = slot.get("valid_until");
            let live = until
                .is_none_or(|value| value.is_null() || value.as_str().is_some_and(str::is_empty));
            let (assistant_steps, self_steps) = corroboration_steps(slot.get("evidence"));
            edges.push(ObservedEdge {
                id,
                subject: entity.name.clone(),
                relation: slot_relation(predicate, key),
                object: value.to_string(),
                live,
                alpha: json_number(slot.get("alpha")),
                beta,
                self_reinforcements: json_i64(slot.get("self_reinforcements")).max(0),
                prior_alpha,
                assistant_steps,
                self_steps,
            });
        }
    }
    Ok(edges)
}

/// A setting whose key is empty or `value` is the bare `setting` relation.
fn slot_relation(predicate: &str, key: &str) -> String {
    if predicate == "setting" && (key.is_empty() || key == "value") {
        "setting".to_string()
    } else if predicate == "setting" {
        format!("setting:{key}")
    } else {
        predicate.to_string()
    }
}

fn json_number(value: Option<&serde_json::Value>) -> f64 {
    let Some(value) = value else {
        return 0.0;
    };
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|number| number as f64))
        .unwrap_or(0.0)
}

fn json_i64(value: Option<&serde_json::Value>) -> i64 {
    let Some(value) = value else {
        return 0;
    };
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_f64().map(|number| number as i64))
        .unwrap_or(0)
}

/// Agent-authored corroborations after the creating sighting.
///
/// The first evidence item is the create. It does not move α, so it is not
/// a step. Missing evidence leaves both counts at zero and the grader treats
/// the whole tally as the self class.
fn corroboration_steps(evidence: Option<&serde_json::Value>) -> (u32, u32) {
    let Some(items) = evidence.and_then(serde_json::Value::as_array) else {
        return (0, 0);
    };
    let mut assistant = 0u32;
    let mut inferred = 0u32;
    for item in items.iter().skip(1) {
        match item.get("role").and_then(serde_json::Value::as_str) {
            Some("assistant") => assistant += 1,
            Some("self") => inferred += 1,
            _ => {}
        }
    }
    (assistant, inferred)
}

async fn edge_evidence(
    gm: &GraphMemory,
) -> Result<HashMap<String, serde_json::Value>, RecallError> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: serde_json::Value,
        #[serde(default)]
        evidence: Option<serde_json::Value>,
    }
    let mut response = gm
        .db()
        .query("SELECT id, evidence FROM relates_to")
        .await
        .map_err(crate::graph::error::GraphError::from)?;
    let rows: Vec<Row> = crate::graph::deserialize_take(&mut response, 0)?;
    let mut by_id = HashMap::new();
    for row in rows {
        let Some(evidence) = row.evidence else {
            continue;
        };
        by_id.insert(crate::graph::edge_view::record_id(&row.id), evidence);
    }
    Ok(by_id)
}

/// α the record was created at. β at first sight is the class prior's β, so
/// this is the creating α for every provenance class.
fn starting_alpha(id: &str, beta: f64, first_beta: &mut HashMap<String, f64>) -> f64 {
    let beta_first = match first_beta.get(id) {
        Some(seen) => *seen,
        None => {
            first_beta.insert(id.to_string(), beta);
            beta
        }
    };
    PRIOR_CONCENTRATION - beta_first
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

    /// The default would call `complete_measured` on this wrapper and drop
    /// the schema before the inner provider saw it.
    async fn complete_structured(
        &self,
        system_prompt: &str,
        user_message: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, crate::graph::error::GraphError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner
            .complete_structured(system_prompt, user_message, schema, max_tokens)
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

    async fn complete_structured(
        &self,
        system_prompt: &str,
        user_message: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, crate::graph::error::GraphError> {
        self.0
            .complete_structured(system_prompt, user_message, schema, max_tokens)
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
    v2: Vec<V2Item>,
}

struct Prepared {
    needle: String,
    body: ExtractionResult,
}

enum V2Item {
    Fact(V2Fact),
    Bug(V2Bug),
    Release(V2Release),
}

struct V2Fact {
    needle: String,
    subject: String,
    subject_type: String,
    predicate: String,
    key: String,
    object: String,
    object_type: String,
    status: &'static str,
}

struct V2Bug {
    needle: String,
    symptom: String,
    cause: String,
    fix: String,
    component: String,
}

struct V2Release {
    needle: String,
    version: String,
    status: &'static str,
    contents: String,
}

/// How a fixture relation becomes a pass-1 item. `setting:` keeps its key.
/// `rule:`, `decision:`, `feature:` and `reason:` are rules. `bug:` and
/// `fix:` are bugs, so the expander runs. A `release:` or `plan:` that names
/// a version is a release; the rest are rules.
#[derive(Debug)]
enum RelationMap {
    Setting {
        key: String,
    },
    Rule,
    Bug {
        symptom: String,
        cause: String,
        fix: String,
    },
    Release {
        version: String,
        status: &'static str,
    },
}

impl ScriptedExtractor {
    fn new(script: &FactScript) -> Result<Self, RecallError> {
        let mut prepared = Vec::new();
        let mut v2: Vec<V2Item> = Vec::new();
        for session in &script.sessions {
            if session.transcript.is_some() {
                for claim in &session.claims {
                    let needle = claim.quote.clone().ok_or_else(|| {
                        RecallError::Config(format!(
                            "claim '{}' in session '{}' has no quote",
                            claim.fact, session.id
                        ))
                    })?;
                    v2.push(v2_fact(script, claim, needle.clone())?);
                    prepared.push(Prepared {
                        needle,
                        body: extraction_for(script, claim)?,
                    });
                }
                continue;
            }
            for turn in &session.turns {
                if turn.claims.is_empty() {
                    continue;
                }
                let mut body = ExtractionResult::default();
                for claim in &turn.claims {
                    v2.push(v2_fact(script, claim, turn.text.clone())?);
                    body.append(extraction_for(script, claim)?);
                }
                prepared.push(Prepared {
                    needle: turn.text.clone(),
                    body,
                });
            }
        }
        Ok(Self { prepared, v2 })
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
        if system_prompt == crate::graph::v2::pass1::PASS1_SYSTEM {
            return self.v2_json(user_message);
        }
        if system_prompt == crate::graph::v2::resolve::PASS2_SYSTEM {
            return self.resolver_json(user_message);
        }
        if system_prompt.contains(crate::graph::dedup::DEDUP_SYSTEM_PROMPT) {
            return Ok(r#"{"decision":"create","reason":"distinct"}"#.to_string());
        }
        self.extraction_json(user_message)
    }
}

fn map_relation(relation: &str, object: &str) -> RelationMap {
    let Some((prefix, rest)) = relation.split_once(':') else {
        return if relation == "rule" {
            RelationMap::Rule
        } else {
            RelationMap::Setting { key: String::new() }
        };
    };
    match prefix {
        "setting" => RelationMap::Setting {
            key: rest.to_string(),
        },
        "rule" | "decision" | "feature" | "reason" => RelationMap::Rule,
        "bug" => RelationMap::Bug {
            symptom: symptom_label(rest),
            cause: object.to_string(),
            fix: String::new(),
        },
        "fix" => RelationMap::Bug {
            symptom: symptom_label(rest),
            cause: String::new(),
            fix: object.to_string(),
        },
        "release" if looks_like_version(rest) => RelationMap::Release {
            version: rest.to_string(),
            status: "released",
        },
        "plan" => match first_version(object) {
            Some(version) => RelationMap::Release {
                version,
                status: "planned",
            },
            None => RelationMap::Rule,
        },
        _ => RelationMap::Rule,
    }
}

fn symptom_label(suffix: &str) -> String {
    let label = suffix.replace('_', " ");
    let label = label.trim();
    if label.is_empty() {
        "bug".to_string()
    } else {
        label.to_string()
    }
}

fn looks_like_version(raw: &str) -> bool {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    !trimmed.is_empty()
        && trimmed.chars().any(|ch| ch.is_ascii_digit())
        && trimmed.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        && trimmed.contains('.')
}

fn first_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let start = if bytes[index] == b'v' || bytes[index] == b'V' {
            index + 1
        } else {
            index
        };
        if start < bytes.len() && bytes[start].is_ascii_digit() {
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
                end += 1;
            }
            let version = &text[start..end];
            if looks_like_version(version) {
                return Some(version.to_string());
            }
        }
        index += 1;
    }
    None
}

fn v2_fact(script: &FactScript, claim: &Claim, needle: String) -> Result<V2Item, RecallError> {
    let fact_id = match claim.op {
        ClaimOp::Supersede => claim.replacement.as_deref().ok_or_else(|| {
            RecallError::Config(format!("supersede of '{}' has no replacement", claim.fact))
        })?,
        _ => claim.fact.as_str(),
    };
    let fact = script.fact(fact_id)?;
    let status = if claim.op == ClaimOp::Reject {
        "rejected"
    } else {
        "decided"
    };
    Ok(match map_relation(&fact.relation, &fact.object) {
        RelationMap::Bug {
            symptom,
            cause,
            fix,
        } => V2Item::Bug(V2Bug {
            needle,
            symptom,
            cause,
            fix,
            component: fact.subject.clone(),
        }),
        RelationMap::Release {
            version,
            status: release_status,
        } => V2Item::Release(V2Release {
            needle,
            version,
            status: release_status,
            contents: fact.object.clone(),
        }),
        mapped => {
            let (predicate, key) = match mapped {
                RelationMap::Setting { key } => ("setting".to_string(), key),
                RelationMap::Rule => ("rule".to_string(), String::new()),
                RelationMap::Bug { .. } | RelationMap::Release { .. } => unreachable!(),
            };
            let object_type = if matches!(predicate.as_str(), "setting" | "rule" | "prefers") {
                "literal".to_string()
            } else {
                fact.object_type.clone()
            };
            let subject_type = match fact.subject_type.as_str() {
                "project" | "component" | "technology" | "person" => fact.subject_type.clone(),
                _ => "project".to_string(),
            };
            V2Item::Fact(V2Fact {
                needle,
                subject: fact.subject.clone(),
                subject_type,
                predicate,
                key,
                object: fact.object.clone(),
                object_type,
                status,
            })
        }
    })
}

fn turn_index(window: &str, needle: &str) -> Option<i64> {
    let mut current = None;
    let mut found = None;
    for line in window.lines() {
        if let Some(rest) = line.strip_prefix("[T") {
            if let Some((num, after)) = rest.split_once(']') {
                if after.starts_with(" USER:") || after.starts_with(" ASSISTANT:") {
                    if let Ok(index) = num.parse::<i64>() {
                        current = Some(index);
                    }
                }
            }
        }
        if found.is_none() && line.contains(needle) {
            found = current;
        }
    }
    found
}

impl V2Item {
    fn needle(&self) -> &str {
        match self {
            Self::Fact(fact) => &fact.needle,
            Self::Bug(bug) => &bug.needle,
            Self::Release(release) => &release.needle,
        }
    }
}

impl ScriptedExtractor {
    fn v2_json(&self, user_message: &str) -> Result<String, crate::graph::error::GraphError> {
        let mut facts = Vec::new();
        let mut bugs = Vec::new();
        let mut releases = Vec::new();
        for item in &self.v2 {
            let needle = item.needle();
            if !user_message.contains(needle) {
                continue;
            }
            let Some(turn) = turn_index(user_message, needle) else {
                continue;
            };
            match item {
                V2Item::Fact(item) => facts.push(serde_json::json!({
                    "subject": item.subject,
                    "subject_type": item.subject_type,
                    "predicate": item.predicate,
                    "role": "none",
                    "key": item.key,
                    "object": item.object,
                    "object_type": item.object_type,
                    "status": item.status,
                    "replaces": "",
                    "turn": turn,
                    "quote": item.needle,
                    "confidence": 0.9,
                })),
                V2Item::Bug(item) => bugs.push(serde_json::json!({
                    "symptom": item.symptom,
                    "cause": item.cause,
                    "fix": item.fix,
                    "status": "open",
                    "component": item.component,
                    "turn": turn,
                    "quote": item.needle,
                    "confidence": 0.9,
                })),
                V2Item::Release(item) => releases.push(serde_json::json!({
                    "version": item.version,
                    "status": item.status,
                    "contents": item.contents,
                    "turn": turn,
                    "quote": item.needle,
                    "confidence": 0.9,
                })),
            }
        }
        serde_json::to_string(&serde_json::json!({
            "facts": facts,
            "bugs": bugs,
            "releases": releases,
        }))
        .map_err(crate::graph::error::GraphError::from)
    }

    /// The scripted resolver declines every mention. A real match is a tier
    /// or a guard decision, never a fixture-tuned model answer.
    fn resolver_json(&self, user_message: &str) -> Result<String, crate::graph::error::GraphError> {
        let mut entities = Vec::new();
        for line in user_message.lines() {
            let Some(rest) = line.trim().strip_prefix("- \"") else {
                continue;
            };
            let Some(end) = rest.find('"') else {
                continue;
            };
            entities.push(serde_json::json!({
                "mention": &rest[..end],
                "match_id": "",
            }));
        }
        serde_json::to_string(&serde_json::json!({ "entities": entities }))
            .map_err(crate::graph::error::GraphError::from)
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
    use super::super::script::ScriptTurn;
    use super::*;
    use super::{map_relation, RelationMap};
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
        let err = run(
            &history(),
            tmp.path(),
            Extractor::Scripted,
            PipelineVersion::V1,
        )
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
    fn write_session_writes_a_turns_sidecar() {
        let script = history();
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        write_archives(&memory, &script).unwrap();

        let raw = fs::read_to_string(memory.join("turns/conversation-001.json")).unwrap();
        let sidecar: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(sidecar["date"], "2026-01-01");
        assert_eq!(sidecar["session_id"], "s1");
        assert_eq!(sidecar["turns"][0]["role"], "user");
        assert_eq!(sidecar["turns"][0]["text"], "The deploy host is pine.");
        assert!(sidecar["cwd"].is_null());
        assert_eq!(sidecar["tools"], serde_json::json!([]));
        let conversations = fs::read_dir(memory.join("conversations"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(conversations.iter().all(|name| name.ends_with(".md")));
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

    // Opening a store needs the embedded engine. Without it the binary
    // compiles and this test is skipped; the same engine gap already fails
    // the graph scan tests on a `--no-default-features` run.
    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn scripted_run_classifies_every_claim() {
        let script = history();
        let tmp = tempfile::tempdir().unwrap();
        let report = run(
            &script,
            tmp.path(),
            Extractor::Scripted,
            PipelineVersion::V1,
        )
        .await
        .unwrap();

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
        assert!(report.patterns.is_none());
        assert!(report
            .sessions
            .iter()
            .all(|session| session.timing.is_none()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeat_refuses_a_non_empty_work_dir_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("keep"), "real").unwrap();
        let err = run_repeat(
            &history(),
            tmp.path(),
            3,
            Extractor::Scripted,
            PipelineVersion::V1,
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&tmp.path().display().to_string()),
            "{message}"
        );
        assert!(message.contains("not empty"), "{message}");
        assert!(!tmp.path().join("run-1").exists());
        assert!(!tmp.path().join("memory").exists());
        assert_eq!(fs::read_to_string(tmp.path().join("keep")).unwrap(), "real");

        let err = run_repeat(
            &history(),
            tmp.path(),
            0,
            Extractor::Scripted,
            PipelineVersion::V1,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("at least 1"), "{err}");
        assert!(!tmp.path().join("run-1").exists());
    }

    #[test]
    fn repeat_median_and_range_on_fixed_numbers() {
        let odd = [
            fixed_metric(1.0, 10.0, 100.0, 4.0),
            fixed_metric(3.0, 30.0, 300.0, 8.0),
            fixed_metric(2.0, 20.0, 50.0, 6.0),
        ];
        let (mid, range) = summarize(&odd);
        assert_eq!(mid.found, 2.0);
        assert_eq!(mid.complete, 20.0);
        assert_eq!(mid.calls_per_session, 100.0);
        assert_eq!(mid.ms_per_session, 6.0);
        assert_eq!(range.found.min, 1.0);
        assert_eq!(range.found.max, 3.0);
        assert_eq!(range.calls_per_session.min, 50.0);
        assert_eq!(range.calls_per_session.max, 300.0);

        let even = [
            fixed_metric(0.0, 0.0, 10.0, 5.0),
            fixed_metric(0.0, 0.0, 20.0, 9.0),
        ];
        let (mid, range) = summarize(&even);
        assert_eq!(mid.calls_per_session, 15.0);
        assert_eq!(mid.ms_per_session, 7.0);
        assert_eq!(range.ms_per_session.min, 5.0);
        assert_eq!(range.ms_per_session.max, 9.0);
        assert_eq!(median(&[]), 0.0);
    }

    fn fixed_metric(found: f64, complete: f64, calls: f64, millis: f64) -> MetricSet {
        MetricSet {
            found,
            complete,
            wrong: 0.0,
            missing: 0.0,
            retired: 0.0,
            premature: 0.0,
            noise_hits: 0.0,
            unmatched: 0.0,
            calls_per_session: calls,
            ms_per_session: millis,
        }
    }

    // Pre-redesign baseline for the linkshelf fixture under the scripted
    // extractor. Supersede does not retire the old edge, and reject still
    // stores the fact, so the six superseded facts and F42 stay live.
    // F42 is wrong (rejected_live) and not found. F01 is found but incomplete
    // because its object does not carry the uv detail. Live and superseded
    // core matches are assigned, so shared is empty. F42's row is not
    // assigned and is not shared: a rejected match is left out of shared.
    // Later pipeline work updates these counts on purpose.
    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn linkshelf_scripted_v1_baseline() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/linkshelf/script.toml");
        let script = crate::bench::writes::load_script(&path).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let report = run(
            &script,
            tmp.path(),
            Extractor::Scripted,
            PipelineVersion::V1,
        )
        .await
        .unwrap();

        assert_eq!(report.extractor, ExtractorLabel::Scripted);
        assert_eq!(
            report.steps.len(),
            script
                .sessions
                .iter()
                .map(|session| session.claims.len())
                .sum::<usize>()
        );
        assert_eq!(report.sessions.len(), 14);
        assert_eq!(report.operations.create, counted(39, 1, 0));
        assert_eq!(report.operations.corroborate, counted(0, 0, 0));
        assert_eq!(report.operations.supersede, counted(0, 0, 6));
        assert_eq!(report.operations.contradict, counted(0, 0, 0));
        assert_eq!(report.operations.no_op, counted(0, 0, 1));
        assert_eq!(report.operations.removed, counted(0, 0, 0));
        assert_eq!(report.operations.revived, counted(0, 0, 0));
        assert!(report.junk_edges.is_empty());
        assert_eq!(
            report
                .wrong_facts
                .iter()
                .map(|wrong| (wrong.fact.as_str(), wrong.reason))
                .collect::<Vec<_>>(),
            vec![
                ("F04", grade::WrongReason::StillLive),
                ("F05", grade::WrongReason::StillLive),
                ("F09", grade::WrongReason::StillLive),
                ("F10", grade::WrongReason::StillLive),
                ("F26", grade::WrongReason::StillLive),
                ("F27", grade::WrongReason::StillLive),
                ("F42", grade::WrongReason::StillLive),
            ]
        );
        let patterns = report.patterns.expect("pattern section");
        assert_eq!(patterns.facts, 46);
        assert_eq!(patterns.pairs, 6);
        assert_eq!(patterns.noise, 7);
        assert_eq!(
            patterns.found.count, 45,
            "missing {:?} shared {:?}",
            patterns.missing.ids, patterns.shared.ids
        );
        assert!(patterns.found.ids.iter().any(|id| id == "F01"));
        assert!(patterns.found.ids.iter().all(|id| id != "F42"));
        assert_eq!(patterns.complete.count, 44);
        assert!(!patterns.complete.ids.iter().any(|id| id == "F01"));
        assert!(!patterns.complete.ids.iter().any(|id| id == "F42"));
        assert_eq!(
            patterns.wrong.ids,
            ["F04", "F05", "F09", "F10", "F26", "F27", "F42"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            patterns.wrong.reasons,
            vec![
                grade::PatternWrongReason::StillLive,
                grade::PatternWrongReason::StillLive,
                grade::PatternWrongReason::StillLive,
                grade::PatternWrongReason::StillLive,
                grade::PatternWrongReason::StillLive,
                grade::PatternWrongReason::StillLive,
                grade::PatternWrongReason::RejectedLive,
            ]
        );
        assert_eq!(patterns.missing.count, 0);
        assert_eq!(patterns.rejected.absent.count, 0);
        assert_eq!(patterns.rejected.live.ids, vec!["F42".to_string()]);
        assert_eq!(patterns.shared.count, 0);
        assert_eq!(patterns.retired.count, 0);
        assert_eq!(patterns.premature.count, 0);
        assert_eq!(patterns.noise_hits.count, 0);
        assert_eq!(patterns.unmatched.count, 0);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn prior_alpha_comes_from_first_sight() {
        let script = corroboration();
        let tmp = tempfile::tempdir().unwrap();
        let memory_dir = tmp.path().join("memory");
        let written = write_archives(&memory_dir, &script).unwrap();
        let gm = GraphMemory::open(&memory_dir.join("graph")).await.unwrap();
        gm.embedder().unwrap();
        let (_label, provider) = provider_for(&script, Extractor::Scripted).unwrap();
        let sessions = collect_sessions(&gm, &memory_dir, &written, &provider, PipelineVersion::V1)
            .await
            .unwrap();

        assert_eq!(sessions.len(), 2);
        assert!(sessions[0].before.is_empty());
        let created = setting(&sessions[0].after);
        let later = setting(&sessions[1].after);
        assert_eq!(later.id, created.id);
        assert_eq!(
            created.prior_alpha,
            crate::graph::confidence::PRIOR_CONCENTRATION - created.beta
        );
        assert_eq!(later.prior_alpha, created.prior_alpha);
        assert!(later.alpha > created.alpha);
    }

    #[cfg(feature = "embedded")]
    fn corroboration() -> FactScript {
        FactScript {
            name: "prior".into(),
            sessions: vec![
                ScriptSession {
                    id: "s1".into(),
                    date: "2026-01-01T00:00:00Z".into(),
                    turns: vec![ScriptTurn {
                        role: Speaker::User,
                        text: "The deploy host is pine.".into(),
                        claims: vec![Claim {
                            op: ClaimOp::Create,
                            fact: "host".into(),
                            replacement: None,
                            by: None,
                            quote: None,
                        }],
                        noise: false,
                    }],
                    transcript: None,
                    transcript_path: None,
                    claims: vec![],
                },
                ScriptSession {
                    id: "s2".into(),
                    date: "2026-01-02T00:00:00Z".into(),
                    turns: vec![ScriptTurn {
                        role: Speaker::User,
                        text: "Pine remains the deploy host.".into(),
                        claims: vec![Claim {
                            op: ClaimOp::Corroborate,
                            fact: "host".into(),
                            replacement: None,
                            by: None,
                            quote: None,
                        }],
                        noise: false,
                    }],
                    transcript: None,
                    transcript_path: None,
                    claims: vec![],
                },
            ],
            facts: vec![ScriptFact {
                id: "host".into(),
                subject: "deploy".into(),
                relation: "setting".into(),
                object: "pine".into(),
                subject_type: "project".into(),
                object_type: "concept".into(),
                subject_abstract: None,
                object_abstract: None,
                end_state: Default::default(),
                superseded_in: None,
                pattern: None,
            }],
            expect: vec![],
            questions: vec![],
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        }
    }

    #[cfg(feature = "embedded")]
    fn setting(edges: &[ObservedEdge]) -> &ObservedEdge {
        let found: Vec<_> = edges
            .iter()
            .filter(|edge| {
                edge.subject == "deploy" && edge.relation == "setting" && edge.object == "pine"
            })
            .collect();
        assert_eq!(found.len(), 1, "edges: {edges:?}");
        found[0]
    }

    #[test]
    fn fixture_prefixes_map_onto_v2_shapes() {
        assert!(matches!(
            map_relation("setting:port", "9000"),
            RelationMap::Setting { key } if key == "port"
        ));
        assert!(
            matches!(map_relation("setting", "pine"), RelationMap::Setting { key } if key.is_empty())
        );
        assert!(matches!(
            map_relation("rule:typing", "hints"),
            RelationMap::Rule
        ));
        assert!(matches!(
            map_relation("decision:refresh", "cap at 8"),
            RelationMap::Rule
        ));
        assert!(matches!(
            map_relation("feature:tag_commands", "tag"),
            RelationMap::Rule
        ));
        assert!(matches!(
            map_relation("reason:serve_port", "clash"),
            RelationMap::Rule
        ));
        assert!(matches!(
            map_relation("release:publishing", "pypi"),
            RelationMap::Rule
        ));
        match map_relation("bug:concurrent_add", "the store corrupted") {
            RelationMap::Bug {
                symptom,
                cause,
                fix,
            } => {
                assert_eq!(symptom, "concurrent add");
                assert_eq!(cause, "the store corrupted");
                assert!(fix.is_empty());
            }
            other => panic!("{other:?}"),
        }
        match map_relation("fix:concurrent_add", "move to SQLite") {
            RelationMap::Bug { symptom, fix, .. } => {
                assert_eq!(symptom, "concurrent add");
                assert_eq!(fix, "move to SQLite");
            }
            other => panic!("{other:?}"),
        }
        match map_relation("release:v0.2.0", "sqlite and tags") {
            RelationMap::Release { version, status } => {
                assert_eq!(version, "v0.2.0");
                assert_eq!(status, "released");
            }
            other => panic!("{other:?}"),
        }
        match map_relation("plan:next_release", "v0.3: import bookmarks") {
            RelationMap::Release { version, status } => {
                assert_eq!(version, "0.3");
                assert_eq!(status, "planned");
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn linkshelf_scripted_v2() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/linkshelf/script.toml");
        let script = crate::bench::writes::load_script(&path).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let report = run(
            &script,
            tmp.path(),
            Extractor::Scripted,
            PipelineVersion::V2,
        )
        .await
        .unwrap();

        assert_eq!(report.calls, 14);
        assert_eq!(report.sessions.len(), 14);
        assert!(report.sessions.iter().all(|session| session.calls == 1));
        // The resolver may add one call in a session that has a similar leftover.
        assert!(report.sessions.iter().all(|session| session.calls <= 2));
        assert!(report.sessions.iter().all(|session| session.windows == 1));
        assert!(report
            .sessions
            .iter()
            .all(|session| session.warnings.is_empty()));
        let candidates: u32 = report
            .sessions
            .iter()
            .map(|session| session.candidates)
            .sum();
        let kept: u32 = report.sessions.iter().map(|session| session.kept).sum();
        assert_eq!(candidates, 46);
        // Nine candidates fail outright. A bug cited from a later turn keeps
        // a symptom that appears in that turn or an earlier one, so five bugs
        // that used to drop stay. A component that is the project name is the
        // project fallback, so those affects edges stay and are not drops.
        // This fixture has no partial field drops.
        assert_eq!(kept, 37);
        let mut drops: HashMap<&str, u32> = HashMap::new();
        for session in &report.sessions {
            for (reason, count) in &session.drops {
                *drops.entry(reason.as_str()).or_insert(0) += count;
            }
        }
        assert_eq!(drops.get("value_support").copied(), Some(7));
        assert_eq!(drops.get("key_binding").copied(), Some(2));
        assert!(!drops.contains_key("value_support_partial"));
        assert_eq!(drops.len(), 2, "{drops:?}");
        assert_eq!(kept + drops.values().sum::<u32>(), candidates);
        let patterns = report.patterns.expect("linkshelf carries match blocks");
        assert_eq!(patterns.pairs, 6);
        // A pair retires only when both facts were kept. The replacements
        // whose values or keys failed the gate leave the old fact in place.
        assert_eq!(
            patterns.retired.ids,
            ["F04->F14", "F09->F15", "F10->F22"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            patterns.premature.count, 0,
            "premature {:?}",
            patterns.premature.ids
        );

        let store = v2_store(tmp.path()).await;
        assert_eq!(store.edges, 6, "edges {:?}", store.edge_types);
        assert_eq!(
            store
                .edge_types
                .iter()
                .filter(|rel| rel.as_str() == "affects")
                .count(),
            4,
            "{:?}",
            store.edge_types
        );
        assert_eq!(
            store
                .edge_types
                .iter()
                .filter(|rel| rel.as_str() == "part_of")
                .count(),
            2,
            "{:?}",
            store.edge_types
        );
        assert!(
            store.names.iter().all(|name| !store.literal.is_match(name)),
            "literal names in {:?}",
            store.names
        );
        assert_eq!(
            store.names,
            vec![
                "concurrent add".to_string(),
                "export timestamps".to_string(),
                "fetch encoding".to_string(),
                "linkshelf".to_string(),
                "linkshelf v0.2.0".to_string(),
                "linkshelf v0.3".to_string(),
                "refresh rate limit".to_string(),
            ]
        );
        assert_eq!(
            store
                .names
                .iter()
                .filter(|name| name.as_str() == "linkshelf")
                .count(),
            1
        );
        assert_eq!(store.slots, 51, "slots {:?}", store.slot_predicates);
        let pred = |name: &str| {
            store
                .slot_predicates
                .iter()
                .filter(|item| item.as_str() == name)
                .count()
        };
        // A dot inside a file name does not end the binding sentence, so the
        // migration setting is kept. Rules are unchanged.
        assert_eq!(pred("setting"), 15);
        assert_eq!(pred("rule"), 12);
        // A later citation keeps a symptom said earlier in the session.
        // A kept bug whose component is the project keeps the fallback edge.
        assert_eq!(pred("issue_symptom"), 4);
        assert_eq!(pred("issue_cause"), 4);
        assert_eq!(pred("issue_fix"), 4);
        assert_eq!(pred("issue_status"), 8);
        assert_eq!(pred("release_status"), 2);
        assert_eq!(pred("release_contents"), 2);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn history_scripted_v2() {
        let script = history();
        let tmp = tempfile::tempdir().unwrap();
        let report = run(
            &script,
            tmp.path(),
            Extractor::Scripted,
            PipelineVersion::V2,
        )
        .await
        .unwrap();

        assert_eq!(report.calls, 7);
        assert_eq!(report.sessions.len(), 7);
        assert!(report.sessions.iter().all(|session| session.calls == 1));
        assert!(report.patterns.is_none());
        let store = v2_store(tmp.path()).await;
        assert_eq!(store.edges, 0, "edges {:?}", store.edge_types);
        assert!(
            store.names.iter().all(|name| !store.literal.is_match(name)),
            "{:?}",
            store.names
        );
        assert_eq!(store.names, vec!["deploy".to_string()]);
        assert_eq!(store.slots, 3, "slots {:?}", store.slot_keys);
        assert!(store.slot_keys.iter().all(|key| key == "value"));
        // The host facts have no setting key, so oak is stored and pine stays
        // active. That supersede is a missed retirement. The reject of birch
        // is a no-op: the rejected slot is stored and is not a live record.
        // The contradict claim is still a reinforcement, because the scripted
        // extractor emits the decided value again, so that step stays a
        // contradict miss and the extra reinforcement stays the corroborate
        // false positive. The assistant restatement adds the assistant weight
        // (0.3) and one tally. That is the v2 speaker fit, so the assistant
        // step is a true positive beside the user corroboration.
        assert_eq!(report.operations.create, counted(1, 0, 0));
        assert_eq!(report.operations.corroborate, counted(2, 1, 0));
        assert_eq!(report.operations.supersede, counted(0, 0, 1));
        assert_eq!(report.operations.contradict, counted(0, 0, 1));
        assert_eq!(report.operations.no_op, counted(2, 0, 0));
        assert_eq!(report.operations.removed, counted(0, 0, 0));
        assert_eq!(report.operations.revived, counted(0, 0, 0));
        assert_eq!(
            report.wrong_facts,
            vec![grade::WrongFact {
                fact: "deploy-host".into(),
                reason: grade::WrongReason::StillLive,
            }]
        );
        assert!(report.junk_edges.is_empty(), "{:?}", report.junk_edges);
        assert!(!report.questions[0].graph_hit);
        assert!(report.questions[1].graph_hit);
        assert!(
            report.sessions.iter().all(|session| {
                let Some(timing) = session.timing else {
                    return false;
                };
                timing.total_ms >= timing.pass1_ms
            }),
            "stage timing {:?}",
            report
                .sessions
                .iter()
                .map(|session| session.timing)
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "embedded")]
    struct V2Store {
        names: Vec<String>,
        edges: usize,
        edge_types: Vec<String>,
        slots: usize,
        slot_keys: Vec<String>,
        slot_predicates: Vec<String>,
        literal: regex::Regex,
    }

    #[cfg(feature = "embedded")]
    async fn v2_store(work_dir: &Path) -> V2Store {
        let gm = GraphMemory::open(&work_dir.join("memory/graph"))
            .await
            .unwrap();
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let mut names: Vec<String> = entities.iter().map(|entity| entity.name.clone()).collect();
        names.sort();
        let mut slots = 0;
        let mut slot_keys = Vec::new();
        let mut slot_predicates = Vec::new();
        for entity in &entities {
            let Some(list) = entity
                .attributes
                .as_ref()
                .and_then(|attrs| attrs.get("v2"))
                .and_then(|v2| v2.get("slots"))
                .and_then(|slots| slots.as_array())
            else {
                continue;
            };
            slots += list.len();
            for slot in list {
                if let Some(key) = slot.get("key").and_then(|key| key.as_str()) {
                    slot_keys.push(key.to_string());
                }
                if let Some(predicate) = slot
                    .get("predicate")
                    .and_then(|predicate| predicate.as_str())
                {
                    slot_predicates.push(predicate.to_string());
                }
            }
        }
        let relationships = crate::graph::crud::list_all_relationships(gm.db())
            .await
            .unwrap();
        V2Store {
            names,
            edges: relationships.len(),
            edge_types: relationships
                .iter()
                .map(|edge| edge.rel_type.clone())
                .collect(),
            slots,
            slot_keys,
            slot_predicates,
            literal: regex::Regex::new(r"^[\d.]+[a-z%]*$|[/~=(%]|^v?\d+(\.\d+)+").unwrap(),
        }
    }

    #[cfg(feature = "embedded")]
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
