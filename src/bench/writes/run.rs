// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a fact script into archives, ingest them, and grade the store.
//!
//! Archives go through the same renderer ingestion already reads, so turn
//! headings survive and provenance is classified the way a real session is.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;

use super::grade::{
    self, ExtractorLabel, ObservedEdge, SessionObservation, StatusRow, StatusWindow, WriteReport,
};
use super::script::{Claim, ClaimOp, FactScript, ScriptFact, ScriptSession, Side, Speaker};
use crate::archive::{append_index, highest_conversation_number};
use crate::conversation::{self, conversation_to_markdown, Conversation, ConversationEntry};
use crate::ephemeral::{self, EphemeralEntry};
use crate::error::RecallError;
use crate::frontmatter::Frontmatter;
use crate::graph::confidence::PRIOR_CONCENTRATION;
use crate::graph::llm::{Completion, LlmProvider, Message};
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

/// Whether pass 1 is shown facts already in the store.
///
/// Off does not read the store and leaves the pass-1 request unchanged.
/// On is the experimental known-facts block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownFactsContext {
    /// Do not read or send stored facts.
    Off,
    /// Read live rows and send them on pass 1.
    On,
}

/// Whether kept facts are matched to stored rows after pass 1.
///
/// Off does not read the store for matching and leaves writes unchanged.
/// On is independent of [`KnownFactsContext`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownMatch {
    /// No matcher and no retirement guards.
    Off,
    /// Lookup after pass 1, then the matcher and the guards.
    On,
}

/// Whether the post-write consolidation pass runs.
///
/// Off does not define a cache table, read the store for pairs, or call the
/// provider. On is independent of [`KnownMatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consolidate {
    /// No consolidation pass.
    Off,
    /// Candidate pairs, a pre-filter, and at most one judge call per session.
    On,
    /// Consolidation, and a hold for a conflict the retirement guard would skip.
    Hold,
}

impl Consolidate {
    fn writes_pass(self) -> bool {
        matches!(self, Self::On | Self::Hold)
    }

    fn writes_hold(self) -> bool {
        matches!(self, Self::Hold)
    }
}

/// Whether a bench run records derived-from links.
///
/// On implies consolidation: the scratch config sets both `consolidate` and
/// `consolidate_links`. It stays independent of [`DropRestatements`],
/// [`KnownMatch`], [`KnownFactsContext`], and the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidateLinks {
    /// Leave the flag at its default.
    Off,
    /// Write `consolidate = true` and `consolidate_links = true` for this run.
    On,
}

/// Whether a bench run widens the post-judge cue to sibling and derived-from partners.
///
/// On implies consolidation: the scratch config sets both `consolidate` and
/// `consolidate_components`. It stays independent of [`DropRestatements`],
/// [`ConsolidateLinks`], [`KnownMatch`], [`KnownFactsContext`], and the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidateComponents {
    /// Leave the flag at its default.
    Off,
    /// Write `consolidate = true` and `consolidate_components = true` for this run.
    On,
}

/// Whether a bench run keeps judged renames, "becomes" replacements, and dropped-entity rules.
///
/// On implies consolidation: the scratch config sets both `consolidate` and
/// `consolidate_key_rename`. It stays independent of [`DropRestatements`],
/// [`ConsolidateLinks`], [`ConsolidateComponents`], [`KnownMatch`],
/// [`KnownFactsContext`], and the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidateKeyRename {
    /// Leave the flag at its default.
    Off,
    /// Write `consolidate = true` and `consolidate_key_rename = true` for this run.
    On,
}

/// Whether a bench run opts in to dropping same-session rule restatements.
///
/// On is independent of [`KnownFactsContext`], [`KnownMatch`], and
/// [`Consolidate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropRestatements {
    /// Leave the flag at its default.
    Off,
    /// Write `drop_restatements = true` for this run.
    On,
}

/// Whether a bench run grounds component `part_of` edges on the component.
///
/// On writes `ground_component_edges = true` and implies nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroundComponentEdges {
    /// Leave the flag at its default.
    Off,
    /// Write `ground_component_edges = true` for this run.
    On,
}

#[allow(clippy::too_many_arguments)]
/// Ingest `script` into `work_dir` and grade the resulting graph.
///
/// `work_dir` must be missing or empty. The function creates `memory/` under it.
///
/// Session `millis` is ingest time. Model load is measured first and reported
/// as `setup_millis`, so the first session is not charged for it.
/// `known_facts_context` turns the experimental pass-1 block on or leaves it off.
/// `drop_restatements` writes that opt-in for this run alone.
pub async fn run(
    script: &FactScript,
    work_dir: &Path,
    extractor: Extractor<'_>,
    pipeline: PipelineVersion,
    known_facts_context: KnownFactsContext,
    known_match: KnownMatch,
    consolidate: Consolidate,
    drop_restatements: DropRestatements,
    consolidate_links: ConsolidateLinks,
    ground_component_edges: GroundComponentEdges,
    consolidate_components: ConsolidateComponents,
    consolidate_key_rename: ConsolidateKeyRename,
) -> Result<WriteReport, RecallError> {
    require_scratch(work_dir)?;
    let memory_dir = work_dir.join("memory");
    let written = write_archives(&memory_dir, script)?;
    apply_graph_switches(
        &memory_dir,
        known_facts_context,
        known_match,
        consolidate,
        drop_restatements,
        consolidate_links,
        ground_component_edges,
        consolidate_components,
        consolidate_key_rename,
    )?;
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
    report.consolidate_hold = consolidate.writes_hold();
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
    let mut status_before = snapshot_status(gm).await?;
    let mut sessions = Vec::with_capacity(written.len());
    for archive in written {
        let text = fs::read_to_string(&archive.path)?;
        let calls_before = provider.calls();
        let started = Instant::now();
        let report = ingest_session(gm, memory_dir, archive, &text, provider, pipeline).await?;
        let calls = provider.calls().saturating_sub(calls_before);
        let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let after = snapshot(gm, &mut first_beta).await?;
        let status_after = snapshot_status(gm).await?;
        sessions.push(SessionObservation {
            id: archive.id.clone(),
            before,
            after: after.clone(),
            calls,
            millis,
            input_tokens: report.input_tokens,
            output_tokens: report.output_tokens,
            warnings: report.errors,
            windows: report.windows,
            candidates: report.candidates,
            kept: report.kept,
            drops: report.drops,
            resolve: report.resolve,
            retirements: report.retirements,
            retirements_skipped: report.retirements_skipped,
            timing: report.timing,
            known_match: report.known_match,
            consolidate: report.consolidate,
            hold: report.hold,
            links: report.links,
            component_edges: report.component_edges,
            status_rows: StatusWindow {
                before: status_before,
                after: status_after.clone(),
            },
        });
        before = after;
        status_before = status_after;
    }
    Ok(sessions)
}

async fn ingest_session(
    gm: &GraphMemory,
    memory_dir: &Path,
    archive: &WrittenArchive,
    text: &str,
    provider: &Counting<'_>,
    pipeline: PipelineVersion,
) -> Result<crate::graph::types::IngestionReport, RecallError> {
    let context = IngestContext::new(&archive.id, Some(archive.log_number));
    match pipeline {
        PipelineVersion::V1 => Ok(gm
            .ingest_archive(text, &context, Some(provider.as_provider()))
            .await?),
        PipelineVersion::V2 => ingest_v2(gm, memory_dir, archive, text, provider, &context).await,
    }
}

/// Episodes stay a separate step, with no model call. The pass-1 calls are
/// the only ones the run is billed for.
async fn ingest_v2(
    gm: &GraphMemory,
    memory_dir: &Path,
    archive: &WrittenArchive,
    text: &str,
    provider: &Counting<'_>,
    context: &IngestContext,
) -> Result<crate::graph::types::IngestionReport, RecallError> {
    let mut report = gm.ingest_archive(text, context, None).await?;
    let extracted = crate::graph::v2::extract_log(
        gm,
        memory_dir,
        archive.log_number,
        text,
        context,
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
    report.input_tokens = add_reported(report.input_tokens, extracted.input_tokens);
    report.output_tokens = add_reported(report.output_tokens, extracted.output_tokens);
    report.llm_calls += extracted.llm_calls;
    report.windows = extracted.windows;
    report.candidates = extracted.candidates;
    report.kept = extracted.kept;
    report.drops = extracted.drops;
    report.resolve = extracted.resolve;
    report.retirements = extracted.retirements;
    report.retirements_skipped = extracted.retirements_skipped;
    report.timing = extracted.timing;
    report.known_match = extracted.known_match;
    report.consolidate = extracted.consolidate;
    report.hold = extracted.hold;
    report.links = extracted.links;
    Ok(report)
}

fn add_reported(total: Option<u64>, extra: Option<u64>) -> Option<u64> {
    match (total, extra) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value),
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
    }
}

#[allow(clippy::too_many_arguments)]
/// Grade `script` `repeat` times, each in a fresh `parent/run-<k>` directory.
///
/// `parent` must be missing or empty. Nothing is created when it is not.
/// `k` starts at 1. One run still uses `run-1`; the command prints a single
/// report itself when repeat is 1.
/// `known_facts_context` is passed to every run.
/// `drop_restatements` is passed to every run.
pub async fn run_repeat(
    script: &FactScript,
    parent: &Path,
    repeat: usize,
    extractor: Extractor<'_>,
    pipeline: PipelineVersion,
    known_facts_context: KnownFactsContext,
    known_match: KnownMatch,
    consolidate: Consolidate,
    drop_restatements: DropRestatements,
    consolidate_links: ConsolidateLinks,
    ground_component_edges: GroundComponentEdges,
    consolidate_components: ConsolidateComponents,
    consolidate_key_rename: ConsolidateKeyRename,
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
        runs.push(
            run(
                script,
                &dir,
                extractor,
                pipeline,
                known_facts_context,
                known_match,
                consolidate,
                drop_restatements,
                consolidate_links,
                ground_component_edges,
                consolidate_components,
                consolidate_key_rename,
            )
            .await?,
        );
    }
    Ok(RepeatReport::from_runs(runs))
}

/// Opt in by writing the graph switches into the fresh memory dir.
#[allow(clippy::too_many_arguments)]
fn apply_graph_switches(
    memory_dir: &Path,
    known: KnownFactsContext,
    matched: KnownMatch,
    consolidate: Consolidate,
    drop_restatements: DropRestatements,
    consolidate_links: ConsolidateLinks,
    ground_component_edges: GroundComponentEdges,
    consolidate_components: ConsolidateComponents,
    consolidate_key_rename: ConsolidateKeyRename,
) -> Result<(), RecallError> {
    let lines = switch_lines(
        known,
        matched,
        consolidate,
        drop_restatements,
        consolidate_links,
        ground_component_edges,
        consolidate_components,
        consolidate_key_rename,
    );
    if lines.is_empty() {
        return Ok(());
    }
    let mut body = String::from("[graph]\n");
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    fs::write(crate::config::config_path(memory_dir), body)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn switch_lines(
    known: KnownFactsContext,
    matched: KnownMatch,
    consolidate: Consolidate,
    drop_restatements: DropRestatements,
    consolidate_links: ConsolidateLinks,
    ground_component_edges: GroundComponentEdges,
    consolidate_components: ConsolidateComponents,
    consolidate_key_rename: ConsolidateKeyRename,
) -> Vec<&'static str> {
    let mut lines = Vec::new();
    push_line(
        &mut lines,
        known == KnownFactsContext::On,
        "known_facts_context = true",
    );
    push_line(&mut lines, matched == KnownMatch::On, "known_match = true");
    push_line(
        &mut lines,
        writes_consolidate(
            consolidate,
            consolidate_links,
            consolidate_components,
            consolidate_key_rename,
        ),
        "consolidate = true",
    );
    push_line(
        &mut lines,
        consolidate.writes_hold(),
        "consolidate_hold = true",
    );
    push_line(
        &mut lines,
        consolidate_links == ConsolidateLinks::On,
        "consolidate_links = true",
    );
    push_line(
        &mut lines,
        consolidate_components == ConsolidateComponents::On,
        "consolidate_components = true",
    );
    push_line(
        &mut lines,
        consolidate_key_rename == ConsolidateKeyRename::On,
        "consolidate_key_rename = true",
    );
    push_line(
        &mut lines,
        drop_restatements == DropRestatements::On,
        "drop_restatements = true",
    );
    push_line(
        &mut lines,
        ground_component_edges == GroundComponentEdges::On,
        "ground_component_edges = true",
    );
    lines
}

fn writes_consolidate(
    consolidate: Consolidate,
    consolidate_links: ConsolidateLinks,
    consolidate_components: ConsolidateComponents,
    consolidate_key_rename: ConsolidateKeyRename,
) -> bool {
    consolidate.writes_pass()
        || consolidate_links == ConsolidateLinks::On
        || consolidate_components == ConsolidateComponents::On
        || consolidate_key_rename == ConsolidateKeyRename::On
}

fn push_line(lines: &mut Vec<&'static str>, on: bool, line: &'static str) {
    if on {
        lines.push(line);
    }
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

async fn pending_edge_ids(gm: &GraphMemory) -> Result<HashSet<String>, RecallError> {
    let mut response = gm
        .db()
        .query("SELECT id FROM relates_to WHERE pending != NONE")
        .await
        .map_err(crate::graph::error::GraphError::from)?;
    let rows: Vec<serde_json::Value> = crate::graph::deserialize_take(&mut response, 0)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            crate::graph::edge_view::record_id(row.get("id").unwrap_or(&serde_json::Value::Null))
        })
        .filter(|id| !id.is_empty())
        .collect())
}

async fn snapshot(
    gm: &GraphMemory,
    first_beta: &mut HashMap<String, f64>,
) -> Result<Vec<ObservedEdge>, RecallError> {
    // Names live on the entity records, not on the edge. The same lookup the
    // inspect path uses keeps a dangling id visible instead of dropping it.
    let relationships = crate::graph::crud::list_all_relationships(gm.db()).await?;
    let pending_edge_ids = pending_edge_ids(gm).await?;
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
        let live = relationship.valid_until.is_none() && !pending_edge_ids.contains(&id);
        edges.push(ObservedEdge {
            id,
            subject: names.name_of(gm.db(), &from_id).await?,
            relation: relationship.rel_type.clone(),
            object: names.name_of(gm.db(), &to_id).await?,
            live,
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
            let pending = slot
                .get("pending")
                .is_some_and(serde_json::Value::is_object);
            let live = !pending
                && until.is_none_or(|value| {
                    value.is_null() || value.as_str().is_some_and(str::is_empty)
                });
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

/// Every stored row, including rejected slots, with status and pending.
///
/// `snapshot` keeps its grading filter. This sibling supplies the rows and
/// the stored status that filter drops.
async fn snapshot_status(gm: &GraphMemory) -> Result<Vec<StatusRow>, RecallError> {
    let mut rows = relationship_status_rows(gm).await?;
    rows.extend(slot_status_rows(gm).await?);
    Ok(rows)
}

async fn relationship_status_rows(gm: &GraphMemory) -> Result<Vec<StatusRow>, RecallError> {
    let relationships = crate::graph::crud::list_all_relationships(gm.db()).await?;
    let pending = pending_edge_ids(gm).await?;
    let stored = stored_relationship_status(gm).await?;
    let mut names = crate::graph::edge_view::NameCache::new();
    let mut rows = Vec::with_capacity(relationships.len());
    for relationship in &relationships {
        rows.push(relationship_status_row(gm, relationship, &pending, &stored, &mut names).await?);
    }
    Ok(rows)
}

async fn relationship_status_row(
    gm: &GraphMemory,
    relationship: &crate::graph::types::Relationship,
    pending: &HashSet<String>,
    stored: &HashMap<String, String>,
    names: &mut crate::graph::edge_view::NameCache,
) -> Result<StatusRow, RecallError> {
    let id = relationship.id_string();
    let from_id = crate::graph::edge_view::record_id(&relationship.from_id);
    let to_id = crate::graph::edge_view::record_id(&relationship.to_id);
    let pending_row = pending.contains(&id);
    let status = stored.get(&id).map(String::as_str);
    Ok(StatusRow {
        id: id.clone(),
        subject: names.name_of(gm.db(), &from_id).await?,
        relation: relationship.rel_type.clone(),
        object: names.name_of(gm.db(), &to_id).await?,
        live: relation_live(relationship.valid_until.is_none(), pending_row, status),
        status: status.map(str::to_string),
        pending: pending_row,
    })
}

fn relation_live(open: bool, pending: bool, status: Option<&str>) -> bool {
    open && !pending && status != Some("rejected")
}

async fn stored_relationship_status(
    gm: &GraphMemory,
) -> Result<HashMap<String, String>, RecallError> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: serde_json::Value,
        #[serde(default)]
        status: Option<String>,
    }
    let mut response = gm
        .db()
        .query("SELECT id, status FROM relates_to")
        .await
        .map_err(crate::graph::error::GraphError::from)?;
    let rows: Vec<Row> = crate::graph::deserialize_take(&mut response, 0)?;
    let mut by_id = HashMap::new();
    for row in rows {
        let Some(status) = present_status(row.status) else {
            continue;
        };
        by_id.insert(crate::graph::edge_view::record_id(&row.id), status);
    }
    Ok(by_id)
}

fn present_status(status: Option<String>) -> Option<String> {
    status.filter(|value| !value.is_empty())
}

async fn slot_status_rows(gm: &GraphMemory) -> Result<Vec<StatusRow>, RecallError> {
    let entities = crate::graph::crud::list_entities(gm.db(), None).await?;
    let mut rows = Vec::new();
    for entity in &entities {
        let Some(slots) = entity_slots(entity) else {
            continue;
        };
        rows.extend(status_rows_for_slots(
            &entity.id_string(),
            &entity.name,
            slots,
        ));
    }
    Ok(rows)
}

fn entity_slots(entity: &crate::graph::types::Entity) -> Option<&Vec<serde_json::Value>> {
    entity
        .attributes
        .as_ref()
        .and_then(|attrs| attrs.get("v2"))
        .and_then(|v2| v2.get("slots"))
        .and_then(|slots| slots.as_array())
}

fn status_rows_for_slots(
    entity_id: &str,
    name: &str,
    slots: &[serde_json::Value],
) -> Vec<StatusRow> {
    let mut grading_seen = HashMap::new();
    let mut side_seen = HashMap::new();
    slots
        .iter()
        .map(|slot| status_row_for_slot(entity_id, name, slot, &mut grading_seen, &mut side_seen))
        .collect()
}

fn status_row_for_slot(
    entity_id: &str,
    name: &str,
    slot: &serde_json::Value,
    grading_seen: &mut HashMap<String, u32>,
    side_seen: &mut HashMap<String, u32>,
) -> StatusRow {
    let predicate = slot_text(slot, "predicate");
    let key = slot_text(slot, "key");
    let value = slot_text(slot, "value");
    let valid_from = slot_text(slot, "valid_from");
    let status = slot_status_text(slot);
    let base = format!("{entity_id}|{predicate}|{key}|{value}|{valid_from}");
    let id = if grading_slot(status) {
        grading_slot_id(&base, grading_seen)
    } else {
        side_slot_id(&base, side_suffix(status), side_seen)
    };
    StatusRow {
        id,
        subject: name.to_string(),
        relation: slot_relation(predicate, key),
        object: value.to_string(),
        live: slot_live(slot),
        status: present_status(Some(status.to_string())),
        pending: slot_pending(slot),
    }
}

fn slot_text<'a>(slot: &'a serde_json::Value, field: &str) -> &'a str {
    slot.get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn slot_status_text(slot: &serde_json::Value) -> &str {
    slot_text(slot, "status")
}

fn grading_slot(status: &str) -> bool {
    status == "decided" || status == "considered"
}

fn side_suffix(status: &str) -> &str {
    if status.is_empty() {
        "nostatus"
    } else {
        status
    }
}

fn grading_slot_id(base: &str, seen: &mut HashMap<String, u32>) -> String {
    let count = next_count(seen, base);
    if count > 1 {
        format!("{base}|{count}")
    } else {
        base.to_string()
    }
}

fn side_slot_id(base: &str, suffix: &str, seen: &mut HashMap<String, u32>) -> String {
    let key = format!("{base}|{suffix}");
    let count = next_count(seen, &key);
    if count > 1 {
        format!("{base}|{count}|{suffix}")
    } else {
        key
    }
}

fn next_count(seen: &mut HashMap<String, u32>, key: &str) -> u32 {
    let count = seen.entry(key.to_string()).or_insert(0);
    *count += 1;
    *count
}

fn slot_pending(slot: &serde_json::Value) -> bool {
    slot.get("pending")
        .is_some_and(serde_json::Value::is_object)
}

fn slot_live(slot: &serde_json::Value) -> bool {
    !slot_pending(slot)
        && open_until(slot.get("valid_until"))
        && slot_status_text(slot) != "rejected"
}

fn open_until(until: Option<&serde_json::Value>) -> bool {
    let Some(value) = until else {
        return true;
    };
    value.is_null() || empty_text(value)
}

fn empty_text(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(str::is_empty)
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

    /// The default would drop every turn but the last before the inner provider.
    async fn complete_structured_chat(
        &self,
        system_prompt: &str,
        messages: &[Message],
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, crate::graph::error::GraphError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner
            .complete_structured_chat(system_prompt, messages, schema, max_tokens)
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

    async fn complete_structured_chat(
        &self,
        system_prompt: &str,
        messages: &[Message],
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<Completion, crate::graph::error::GraphError> {
        self.0
            .complete_structured_chat(system_prompt, messages, schema, max_tokens)
            .await
    }
}

/// Replays the script's claims.
///
/// The provider trait only receives prompt text, so a dedup prompt is told
/// apart by the sentence `graph::dedup` opens with. Anything else is treated
/// as extraction and answered from the turn whose text appears in the prompt.
/// A pass-1 chat call folds the worked example into the system prompt and
/// keeps the last user message. Claims are matched on that message, so the
/// example is not scanned.
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
        if system_prompt.starts_with(crate::graph::v2::pass1::PASS1_SYSTEM_BARE) {
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
    use super::super::script::{ScriptFact, ScriptTurn, Speaker};
    use super::*;
    use super::{map_relation, RelationMap};
    use crate::graph::extract::chunk_conversation;
    #[cfg(feature = "embedded")]
    use crate::graph::llm::TokenUsage;
    use crate::graph::{Provenance, ProvenancePolicy};

    fn history() -> FactScript {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/writes/history.toml");
        crate::bench::writes::load_script(&path).unwrap()
    }

    #[tokio::test]
    async fn counting_forwards_the_chat_call() {
        struct Inner(std::sync::Arc<std::sync::Mutex<Vec<Message>>>);

        #[async_trait]
        impl LlmProvider for Inner {
            async fn complete(
                &self,
                _: &str,
                _: &str,
                _: u32,
            ) -> Result<String, crate::graph::error::GraphError> {
                Err(crate::graph::error::GraphError::Llm(
                    "plain complete was called".into(),
                ))
            }

            async fn complete_structured_chat(
                &self,
                _: &str,
                messages: &[Message],
                _: &serde_json::Value,
                _: u32,
            ) -> Result<Completion, crate::graph::error::GraphError> {
                *self.0.lock().expect("lock") = messages.to_vec();
                Ok(Completion::unmeasured("{}"))
            }
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let counting = Counting::new(Box::new(Inner(std::sync::Arc::clone(&seen))));
        let messages = [
            Message::user("example-user"),
            Message::assistant("example-reply"),
            Message::user("real-user"),
        ];
        counting
            .complete_structured_chat("sys", &messages, &serde_json::json!({}), 8)
            .await
            .unwrap();
        assert_eq!(counting.calls(), 1);
        assert_eq!(seen.lock().expect("lock").as_slice(), &messages);
    }

    #[test]
    fn consolidate_switch_is_written_only_when_on() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(&memory).unwrap();
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        assert!(!crate::config::config_path(&memory).exists());
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::On,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(!text.contains("known_match"), "{text}");
        assert!(!text.contains("consolidate_hold"), "{text}");
        assert!(!text.contains("consolidate_links"), "{text}");
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Hold,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(text.contains("consolidate_hold = true\n"), "{text}");
        assert!(!text.contains("consolidate_links"), "{text}");
        assert!(!text.contains("drop_restatements"), "{text}");
    }

    #[test]
    fn drop_restatements_switch_is_written_only_when_on() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(&memory).unwrap();
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        assert!(!crate::config::config_path(&memory).exists());

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::On,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert_eq!(text, "[graph]\ndrop_restatements = true\n");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.drop_restatements);
        assert!(!graph.known_facts_context);
        assert!(!graph.known_match);
        assert!(!graph.consolidate);
        assert!(!graph.consolidate_hold);
        assert!(!graph.consolidate_links);
        assert!(!text.contains("consolidate_links"));
    }

    #[test]
    fn consolidate_links_implies_consolidate_and_stays_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(&memory).unwrap();
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::On,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(text.contains("consolidate_links = true\n"), "{text}");
        assert!(!text.contains("consolidate_hold"), "{text}");
        assert!(!text.contains("drop_restatements"), "{text}");
        assert!(!text.contains("known_match"), "{text}");
        assert!(!text.contains("known_facts_context"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.links_apply());
        assert!(!graph.hold_applies());
        assert!(!graph.drop_restatements);
        assert!(!graph.known_match);
        assert!(!graph.known_facts_context);

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Hold,
            DropRestatements::Off,
            ConsolidateLinks::On,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(text.contains("consolidate_hold = true\n"), "{text}");
        assert!(text.contains("consolidate_links = true\n"), "{text}");
        assert!(!text.contains("drop_restatements"), "{text}");
        assert!(!text.contains("known_match"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.links_apply());
        assert!(graph.hold_applies());
        assert!(!graph.drop_restatements);

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::On,
            ConsolidateLinks::On,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("drop_restatements = true\n"), "{text}");
        assert!(text.contains("consolidate_links = true\n"), "{text}");
        assert!(!text.contains("consolidate_hold"), "{text}");
        assert!(!text.contains("known_facts_context"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.drop_restatements);
        assert!(graph.links_apply());
        assert!(!graph.hold_applies());
        assert!(!graph.known_match);
        assert!(!graph.known_facts_context);
        assert!(!graph.ground_component_edges);
        assert!(!text.contains("ground_component_edges"), "{text}");
    }

    #[test]
    fn ground_component_edges_is_written_only_when_passed() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(&memory).unwrap();
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        assert!(!crate::config::config_path(&memory).exists());

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::On,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert_eq!(text, "[graph]\nground_component_edges = true\n");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.ground_component_edges);
        assert!(!graph.consolidate);
        assert!(!graph.consolidate_links);
        assert!(!graph.consolidate_hold);
        assert!(!graph.drop_restatements);
        assert!(!graph.known_facts_context);
        assert!(!graph.known_match);
        assert!(!text.contains("consolidate"));
    }

    #[test]
    fn consolidate_components_implies_consolidate_and_stays_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(&memory).unwrap();
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        assert!(!crate::config::config_path(&memory).exists());

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::On,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(text.contains("consolidate_components = true\n"), "{text}");
        assert!(!text.contains("consolidate_hold"), "{text}");
        assert!(!text.contains("consolidate_links"), "{text}");
        assert!(!text.contains("drop_restatements"), "{text}");
        assert!(!text.contains("known_match"), "{text}");
        assert!(!text.contains("known_facts_context"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.components_apply());
        assert!(!graph.links_apply());
        assert!(!graph.hold_applies());
        assert!(!graph.drop_restatements);
        assert!(!graph.known_match);
        assert!(!graph.known_facts_context);

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Hold,
            DropRestatements::Off,
            ConsolidateLinks::On,
            GroundComponentEdges::Off,
            ConsolidateComponents::On,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(text.contains("consolidate_hold = true\n"), "{text}");
        assert!(text.contains("consolidate_links = true\n"), "{text}");
        assert!(text.contains("consolidate_components = true\n"), "{text}");
        assert!(!text.contains("drop_restatements"), "{text}");
        assert!(!text.contains("known_match"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.components_apply());
        assert!(graph.links_apply());
        assert!(graph.hold_applies());
        assert!(!graph.drop_restatements);

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::On,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::On,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("drop_restatements = true\n"), "{text}");
        assert!(text.contains("consolidate_components = true\n"), "{text}");
        assert!(!text.contains("consolidate_hold"), "{text}");
        assert!(!text.contains("consolidate_links"), "{text}");
        assert!(!text.contains("known_facts_context"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.drop_restatements);
        assert!(graph.components_apply());
        assert!(!graph.links_apply());
        assert!(!graph.hold_applies());
        assert!(!graph.known_match);
        assert!(!graph.known_facts_context);
    }

    #[test]
    fn consolidate_key_rename_implies_consolidate_and_stays_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().join("memory");
        fs::create_dir_all(&memory).unwrap();
        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        assert!(!crate::config::config_path(&memory).exists());

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::On,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate = true\n"), "{text}");
        assert!(text.contains("consolidate_key_rename = true\n"), "{text}");
        assert!(!text.contains("consolidate_hold"), "{text}");
        assert!(!text.contains("consolidate_links"), "{text}");
        assert!(!text.contains("consolidate_components"), "{text}");
        assert!(!text.contains("drop_restatements"), "{text}");
        assert!(!text.contains("known_match"), "{text}");
        assert!(!text.contains("known_facts_context"), "{text}");
        assert!(!text.contains("ground_component_edges"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(graph.rename_apply());
        assert!(!graph.components_apply());
        assert!(!graph.links_apply());
        assert!(!graph.hold_applies());
        assert!(!graph.drop_restatements);

        apply_graph_switches(
            &memory,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Hold,
            DropRestatements::On,
            ConsolidateLinks::On,
            GroundComponentEdges::On,
            ConsolidateComponents::On,
            ConsolidateKeyRename::Off,
        )
        .unwrap();
        let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
        assert!(text.contains("consolidate_components = true\n"), "{text}");
        assert!(text.contains("consolidate_links = true\n"), "{text}");
        assert!(text.contains("consolidate_hold = true\n"), "{text}");
        assert!(text.contains("drop_restatements = true\n"), "{text}");
        assert!(text.contains("ground_component_edges = true\n"), "{text}");
        assert!(!text.contains("consolidate_key_rename"), "{text}");
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        let graph = loaded.graph.expect("graph");
        assert!(!graph.consolidate_key_rename);
        assert!(!graph.rename_apply());
        assert!(graph.components_apply());
        assert!(graph.links_apply());
        assert!(graph.hold_applies());
    }

    #[test]
    fn drop_restatements_switch_stays_independent() {
        let switches = [
            (
                KnownFactsContext::On,
                KnownMatch::Off,
                Consolidate::Off,
                DropRestatements::Off,
            ),
            (
                KnownFactsContext::Off,
                KnownMatch::On,
                Consolidate::Off,
                DropRestatements::Off,
            ),
            (
                KnownFactsContext::Off,
                KnownMatch::Off,
                Consolidate::On,
                DropRestatements::Off,
            ),
            (
                KnownFactsContext::Off,
                KnownMatch::Off,
                Consolidate::Hold,
                DropRestatements::Off,
            ),
            (
                KnownFactsContext::On,
                KnownMatch::On,
                Consolidate::Hold,
                DropRestatements::On,
            ),
        ];
        for (known, matched, consolidate, drop_restatements) in switches {
            let tmp = tempfile::tempdir().unwrap();
            let memory = tmp.path().join("memory");
            fs::create_dir_all(&memory).unwrap();
            apply_graph_switches(
                &memory,
                known,
                matched,
                consolidate,
                drop_restatements,
                ConsolidateLinks::Off,
                GroundComponentEdges::Off,
                ConsolidateComponents::Off,
                ConsolidateKeyRename::Off,
            )
            .unwrap();
            let text = fs::read_to_string(crate::config::config_path(&memory)).unwrap();
            let loaded: crate::config::Config = toml::from_str(&text).unwrap();
            let graph = loaded.graph.expect("graph");
            assert_eq!(
                graph.known_facts_context,
                known == KnownFactsContext::On,
                "{text}"
            );
            assert_eq!(graph.known_match, matched == KnownMatch::On, "{text}");
            assert_eq!(graph.consolidate, consolidate.writes_pass(), "{text}");
            assert_eq!(graph.consolidate_hold, consolidate.writes_hold(), "{text}");
            assert!(!graph.consolidate_links, "{text}");
            assert!(!text.contains("consolidate_links"), "{text}");
            assert!(!graph.ground_component_edges, "{text}");
            assert!(!text.contains("ground_component_edges"), "{text}");
            assert_eq!(
                graph.drop_restatements,
                drop_restatements == DropRestatements::On,
                "{text}"
            );
            if drop_restatements == DropRestatements::On {
                assert!(text.contains("drop_restatements = true\n"), "{text}");
            } else {
                assert!(!text.contains("drop_restatements"), "{text}");
            }
        }
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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
        assert_eq!(parsed.relationships[0].rel_type, "setting:host");

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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
        let session = serde_json::to_value(&report.sessions[0]).unwrap();
        assert!(session["input_tokens"].is_null() && session["output_tokens"].is_null());
        assert!(report.input_tokens.is_none() && report.output_tokens.is_none());
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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
        assert!(report.sessions.iter().all(|session| {
            session.consolidate.is_blank()
                && serde_json::to_value(session)
                    .unwrap()
                    .get("consolidate")
                    .is_none()
        }));
        let gm = GraphMemory::open(&tmp.path().join("memory/graph"))
            .await
            .unwrap();
        let mut info = gm.db().query("INFO FOR DB").await.unwrap();
        let rows: Vec<serde_json::Value> = crate::graph::deserialize_take(&mut info, 0).unwrap();
        let text = serde_json::to_string(&rows).unwrap_or_default();
        assert!(
            !text.contains("consolidation_cache"),
            "off run defined the cache table"
        );
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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
        assert!(!drops.contains_key("restatement"));
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
        // A later `fixed` closes the synthetic `open` from an earlier turn.
        // The closed slot stays stored, so the count is still two statuses
        // per kept bug. One pair cites both quotes in a single assistant
        // turn, so both of those statuses stay live.
        assert_eq!(pred("issue_status"), 8);
        assert_eq!(store.open_and_fixed, vec!["fetch encoding".to_string()]);
        assert_eq!(pred("release_status"), 2);
        assert_eq!(pred("release_contents"), 2);
    }

    #[cfg(feature = "embedded")]
    fn uses_fact(object: &str, quote: &str) -> serde_json::Value {
        serde_json::json!({
            "subject": "app",
            "subject_type": "project",
            "predicate": "uses",
            "role": "database",
            "key": "",
            "object": object,
            "object_type": "technology",
            "status": "decided",
            "replaces": "",
            "turn": 0,
            "quote": quote,
            "confidence": 0.9
        })
    }

    #[cfg(feature = "embedded")]
    fn pass1(facts: Vec<serde_json::Value>) -> String {
        serde_json::json!({"facts": facts, "bugs": [], "releases": []}).to_string()
    }

    #[cfg(feature = "embedded")]
    fn counted_usage(input: u64, output: u64) -> Option<TokenUsage> {
        TokenUsage::from_counts(Some(input), Some(output))
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn known_facts_context_flag_changes_the_pass1_prompt() {
        use std::sync::Mutex;

        const SCRIPT: &str = "name = \"cedar\"\nfacts = []\n\
             [[sessions]]\nid = \"s1\"\ndate = \"2026-03-01T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\ntext = \"Keep the shelf label as cedar.\"\n";

        struct Seen(Mutex<Vec<(String, String)>>);

        #[async_trait]
        impl LlmProvider for Seen {
            async fn complete(
                &self,
                _: &str,
                _: &str,
                _: u32,
            ) -> Result<String, crate::graph::error::GraphError> {
                Err(crate::graph::error::GraphError::Llm(
                    "plain complete was called".into(),
                ))
            }

            async fn complete_structured_chat(
                &self,
                system: &str,
                messages: &[Message],
                _: &serde_json::Value,
                _: u32,
            ) -> Result<Completion, crate::graph::error::GraphError> {
                let user = messages.last().map(|message| message.content.clone());
                self.0
                    .lock()
                    .expect("lock")
                    .push((system.to_string(), user.unwrap_or_default()));
                Ok(Completion::unmeasured(
                    r#"{"facts":[],"bugs":[],"releases":[]}"#,
                ))
            }
        }

        async fn once(known: KnownFactsContext) -> (String, String, bool) {
            let script = crate::bench::writes::parse_script(SCRIPT).unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let seen = Seen(Mutex::new(Vec::new()));
            run(
                &script,
                tmp.path(),
                Extractor::Live(&seen),
                PipelineVersion::V2,
                known,
                KnownMatch::Off,
                Consolidate::Off,
                DropRestatements::Off,
                ConsolidateLinks::Off,
                GroundComponentEdges::Off,
                ConsolidateComponents::Off,
                ConsolidateKeyRename::Off,
            )
            .await
            .unwrap();
            let rows = seen.0.lock().expect("lock").clone();
            let configured = std::fs::read_to_string(tmp.path().join("memory/.recall-echo.toml"))
                .unwrap_or_default()
                .contains("known_facts_context = true");
            (rows[0].0.clone(), rows[0].1.clone(), configured)
        }

        let (off_system, off_user, off_configured) = once(KnownFactsContext::Off).await;
        assert_eq!(off_system, crate::graph::v2::pass1::PASS1_SYSTEM_BARE);
        assert!(!off_system.contains("Known facts lists what memory already holds."));
        assert!(!off_user.contains("Known facts"));
        assert!(!off_configured);

        let (on_system, on_user, on_configured) = once(KnownFactsContext::On).await;
        assert_eq!(on_system, crate::graph::v2::pass1::PASS1_SYSTEM);
        assert!(on_system.contains("reuse its key for the same setting."));
        assert!(!on_user.contains("Known facts"));
        assert!(on_configured);
    }

    #[cfg(feature = "embedded")]
    async fn live_v2(script: &str, provider: &dyn LlmProvider) -> WriteReport {
        let script = crate::bench::writes::parse_script(script).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        run(
            &script,
            tmp.path(),
            Extractor::Live(provider),
            PipelineVersion::V2,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .await
        .unwrap()
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn reported_usage_sums_across_sessions() {
        const INPUT: u64 = 17;
        const OUTPUT: u64 = 4;
        // First pass-1 reply for the Postgres window is not the root object.
        struct Reporting(AtomicU64);

        #[async_trait]
        impl LlmProvider for Reporting {
            async fn complete(
                &self,
                _: &str,
                _: &str,
                _: u32,
            ) -> Result<String, crate::graph::error::GraphError> {
                Err(crate::graph::error::GraphError::Llm(
                    "plain complete was called".into(),
                ))
            }

            async fn complete_structured(
                &self,
                system_prompt: &str,
                user_message: &str,
                _: &serde_json::Value,
                _: u32,
            ) -> Result<Completion, crate::graph::error::GraphError> {
                if system_prompt == crate::graph::v2::resolve::PASS2_SYSTEM {
                    let reply = r#"{"entities":[{"mention":"PostgreSQL","match_id":""}]}"#;
                    return Ok(Completion::measured(reply, counted_usage(INPUT, OUTPUT)));
                }
                let text = if user_message.contains("PostgreSQL") {
                    pass1(vec![uses_fact(
                        "PostgreSQL",
                        "The app uses PostgreSQL for storage.",
                    )])
                } else if user_message.contains("Postgres for storage") {
                    let attempt = self.0.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        "nope".to_string()
                    } else {
                        pass1(vec![uses_fact(
                            "Postgres",
                            "The app uses Postgres for storage.",
                        )])
                    }
                } else {
                    pass1(vec![])
                };
                let usage = if user_message.contains("Gamma note") {
                    None
                } else {
                    counted_usage(INPUT, OUTPUT)
                };
                Ok(Completion::measured(text, usage))
            }
        }

        let report = live_v2(
            "name = \"tokens\"\nfacts = []\n\
             [[sessions]]\nid = \"s1\"\ndate = \"2026-01-01T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\ntext = \"The app uses Postgres for storage.\"\n\
             [[sessions]]\nid = \"s2\"\ndate = \"2026-01-02T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\ntext = \"The app uses PostgreSQL for storage.\"\n\
             [[sessions]]\nid = \"s3\"\ndate = \"2026-01-03T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\ntext = \"Gamma note.\"\n",
            &Reporting(AtomicU64::new(0)),
        )
        .await;
        assert!(
            report
                .sessions
                .iter()
                .all(|session| session.warnings.is_empty()),
            "{:?}",
            report.sessions
        );
        // s1 retries a bad reply, then stores Postgres. s2 asks the resolver
        // about PostgreSQL. s3 reports nothing, so it stays out of the mean.
        assert_eq!(report.sessions[0].calls, 2);
        assert_eq!(report.sessions[0].resolve.resolver_called, 0);
        assert_eq!(report.sessions[1].calls, 2);
        assert_eq!(report.sessions[1].resolve.resolver_called, 1);
        assert_eq!(
            (
                report.sessions[0].input_tokens,
                report.sessions[0].output_tokens
            ),
            (Some(INPUT * 2), Some(OUTPUT * 2))
        );
        assert_eq!(
            (
                report.sessions[1].input_tokens,
                report.sessions[1].output_tokens
            ),
            (Some(INPUT * 2), Some(OUTPUT * 2))
        );
        assert_eq!(
            (
                report.sessions[2].input_tokens,
                report.sessions[2].output_tokens
            ),
            (None, None)
        );
        assert_eq!(
            (
                report.input_tokens,
                report.output_tokens,
                report.input_tokens_per_session,
                report.output_tokens_per_session,
            ),
            (
                Some(INPUT * 4),
                Some(OUTPUT * 4),
                Some((INPUT * 2) as f64),
                Some((OUTPUT * 2) as f64)
            )
        );
        let json = serde_json::to_string(&report.sessions[0]).unwrap();
        let at = |key: &str| json.find(&format!("\"{key}\"")).unwrap();
        assert!(at("millis") < at("input_tokens") && at("input_tokens") < at("output_tokens"));
    }

    /// Two pass-1 calls, each reporting (3, 1). The retry is billed.
    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn parse_retry_sums_both_calls_on_the_session_report() {
        struct Retry(AtomicU64);

        #[async_trait]
        impl LlmProvider for Retry {
            async fn complete(
                &self,
                _: &str,
                _: &str,
                _: u32,
            ) -> Result<String, crate::graph::error::GraphError> {
                Err(crate::graph::error::GraphError::Llm(
                    "plain complete was called".into(),
                ))
            }

            async fn complete_structured(
                &self,
                _: &str,
                _: &str,
                _: &serde_json::Value,
                _: u32,
            ) -> Result<Completion, crate::graph::error::GraphError> {
                let attempt = self.0.fetch_add(1, Ordering::Relaxed);
                let text = if attempt == 0 {
                    "nope"
                } else {
                    r#"{"facts":[],"bugs":[],"releases":[]}"#
                };
                Ok(Completion::measured(text, counted_usage(3, 1)))
            }
        }

        let report = live_v2(
            "name = \"retry\"\nfacts = []\n\
             [[sessions]]\nid = \"s1\"\ndate = \"2026-01-01T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\ntext = \"Alpha note.\"\n",
            &Retry(AtomicU64::new(0)),
        )
        .await;
        assert_eq!(report.sessions.len(), 1);
        let session = &report.sessions[0];
        assert!(session.warnings.is_empty(), "{:?}", session.warnings);
        assert_eq!(session.calls, 2);
        assert_eq!(
            (session.input_tokens, session.output_tokens),
            (Some(6), Some(2))
        );
    }

    /// Resolver usage is on the session for a parsed reply and for "nope".
    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn resolver_usage_reaches_the_session_report() {
        struct Resolver;

        #[async_trait]
        impl LlmProvider for Resolver {
            async fn complete(
                &self,
                _: &str,
                _: &str,
                _: u32,
            ) -> Result<String, crate::graph::error::GraphError> {
                Err(crate::graph::error::GraphError::Llm(
                    "plain complete was called".into(),
                ))
            }

            async fn complete_structured(
                &self,
                system_prompt: &str,
                user_message: &str,
                _: &serde_json::Value,
                _: u32,
            ) -> Result<Completion, crate::graph::error::GraphError> {
                if system_prompt == crate::graph::v2::resolve::PASS2_SYSTEM {
                    let mentions = user_message
                        .split_once("NEW MENTIONS:")
                        .map(|(_, rest)| rest)
                        .unwrap_or("");
                    let text = if mentions.contains("PostgreSQL") {
                        r#"{"entities":[{"mention":"PostgreSQL","match_id":""}]}"#
                    } else {
                        "nope"
                    };
                    return Ok(Completion::measured(text, counted_usage(7, 2)));
                }
                let text = if user_message.contains("uvicorn worker") {
                    pass1(vec![
                        uses_fact("Postgres", "The app uses Postgres for storage."),
                        uses_fact("uvicorn worker", "The app uses uvicorn worker for jobs."),
                    ])
                } else if user_message.contains("PostgreSQL") {
                    pass1(vec![uses_fact(
                        "PostgreSQL",
                        "The app uses PostgreSQL for storage.",
                    )])
                } else if user_message.contains("uvicorn for storage") {
                    pass1(vec![uses_fact(
                        "uvicorn",
                        "The app uses uvicorn for storage.",
                    )])
                } else {
                    pass1(vec![])
                };
                Ok(Completion::unmeasured(text))
            }
        }

        let report = live_v2(
            "name = \"resolver\"\nfacts = []\n\
             [[sessions]]\nid = \"seed\"\ndate = \"2026-01-01T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\n\
             text = \"The app uses Postgres for storage. The app uses uvicorn worker for jobs.\"\n\
             [[sessions]]\nid = \"parse\"\ndate = \"2026-01-02T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\n\
             text = \"The app uses PostgreSQL for storage.\"\n\
             [[sessions]]\nid = \"nope\"\ndate = \"2026-01-03T00:00:00Z\"\n\
             [[sessions.turns]]\nrole = \"user\"\n\
             text = \"The app uses uvicorn for storage.\"\n",
            &Resolver,
        )
        .await;
        let session = |id: &str| {
            report
                .sessions
                .iter()
                .find(|session| session.id == id)
                .unwrap()
        };
        let parse = session("parse");
        let nope = session("nope");
        assert_eq!(parse.resolve.resolver_called, 1, "{:?}", parse.warnings);
        assert_eq!(nope.resolve.resolver_called, 1, "{:?}", nope.warnings);
        assert_eq!(
            (parse.input_tokens, parse.output_tokens),
            (Some(7), Some(2))
        );
        assert_eq!((nope.input_tokens, nope.output_tokens), (Some(7), Some(2)));
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
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
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
        assert!(store.slot_keys.iter().all(|key| key == "host"));
        // The three host facts share subject abstract "release destination
        // hostname": one setting, keyed host. Supersede closes pine when oak
        // is written. The reject of birch
        // is a no-op: the rejected slot is stored and is not a live record.
        // The contradict claim is still a reinforcement, because the scripted
        // extractor emits the decided value again, so that step stays a
        // contradict miss and the extra reinforcement stays the corroborate
        // false positive. The assistant restatement adds the assistant weight
        // (0.3) and one tally. That is the v2 speaker fit, so the assistant
        // step is a true positive beside the user corroboration.
        assert_eq!(report.operations.create, counted(1, 0, 0));
        assert_eq!(report.operations.corroborate, counted(2, 1, 0));
        assert_eq!(report.operations.supersede, counted(1, 0, 0));
        assert_eq!(report.operations.contradict, counted(0, 0, 1));
        assert_eq!(report.operations.no_op, counted(2, 0, 0));
        assert_eq!(report.operations.removed, counted(0, 0, 0));
        assert_eq!(report.operations.revived, counted(0, 0, 0));
        assert!(report.wrong_facts.is_empty(), "{:?}", report.wrong_facts);
        assert!(report.junk_edges.is_empty(), "{:?}", report.junk_edges);
        assert!(report.questions[0].graph_hit);
        assert!(!report.questions[1].graph_hit);
        assert!(report
            .sessions
            .iter()
            .all(|session| { session.input_tokens.is_none() && session.output_tokens.is_none() }));
        assert_eq!(
            (
                report.input_tokens,
                report.output_tokens,
                report.input_tokens_per_session,
                report.output_tokens_per_session,
            ),
            (None, None, None, None)
        );
        assert!(
            report.sessions.iter().all(|session| {
                let Some(timing) = session.timing else {
                    return false;
                };
                timing.match_ms.is_none()
                    && timing.consolidate_ms.is_none()
                    && timing.total_ms >= timing.pass1_ms
            }),
            "stage timing {:?}",
            report
                .sessions
                .iter()
                .map(|session| session.timing)
                .collect::<Vec<_>>()
        );
        assert!(report.sessions.iter().all(|session| {
            session.known_match.is_blank()
                && session.consolidate.is_blank()
                && session.hold.is_blank()
                && serde_json::to_value(session)
                    .unwrap()
                    .get("known_match")
                    .is_none()
                && serde_json::to_value(session).unwrap().get("hold").is_none()
                && serde_json::to_value(session)
                    .unwrap()
                    .get("links")
                    .is_none()
                && session.links.is_blank()
                && serde_json::to_value(session)
                    .unwrap()
                    .get("consolidate")
                    .is_none()
                && serde_json::to_value(session).unwrap()["timing"]
                    .get("match_ms")
                    .is_none()
                && serde_json::to_value(session).unwrap()["timing"]
                    .get("consolidate_ms")
                    .is_none()
        }));
        let encoded = serde_json::to_value(&report).unwrap();
        assert!(encoded.get("consolidate_drops").is_none());
        assert!(encoded.get("consolidate_hold").is_none());
        let gm = GraphMemory::open(&tmp.path().join("memory/graph"))
            .await
            .unwrap();
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        assert!(
            !serde_json::to_string(&entities)
                .unwrap_or_default()
                .contains("\"pending\""),
            "hold off wrote a pending field"
        );
        let mut info = gm.db().query("INFO FOR DB").await.unwrap();
        let rows: Vec<serde_json::Value> = crate::graph::deserialize_take(&mut info, 0).unwrap();
        assert!(!serde_json::to_string(&rows)
            .unwrap_or_default()
            .contains("consolidation_cache"));
    }

    /// The switch adds match timing. Writes on this script stay the off-path
    /// counts: each decided setting already names the stored key.
    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn history_scripted_v2_known_match_reports_match_time() {
        let script = history();
        let tmp = tempfile::tempdir().unwrap();
        let report = run(
            &script,
            tmp.path(),
            Extractor::Scripted,
            PipelineVersion::V2,
            KnownFactsContext::Off,
            KnownMatch::On,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            ConsolidateKeyRename::Off,
        )
        .await
        .unwrap();
        assert_eq!(report.operations.supersede, counted(1, 0, 0));
        assert_eq!(report.operations.create, counted(1, 0, 0));
        assert!(report.wrong_facts.is_empty(), "{:?}", report.wrong_facts);
        assert!(report.sessions.iter().all(|session| {
            session
                .timing
                .is_some_and(|timing| timing.match_ms.is_some())
        }));
        let encoded = serde_json::to_value(&report.sessions[0]).unwrap();
        assert!(encoded["timing"].get("match_ms").is_some());
    }

    #[cfg(feature = "embedded")]
    struct V2Store {
        names: Vec<String>,
        edges: usize,
        edge_types: Vec<String>,
        slots: usize,
        slot_keys: Vec<String>,
        slot_predicates: Vec<String>,
        open_and_fixed: Vec<String>,
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
        let mut open_and_fixed = Vec::new();
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
            let live_status = |value: &str| {
                list.iter().any(|slot| {
                    slot.get("predicate").and_then(|item| item.as_str()) == Some("issue_status")
                        && slot.get("value").and_then(|item| item.as_str()) == Some(value)
                        && slot.get("valid_until").is_none_or(|until| {
                            until.is_null() || until.as_str().is_some_and(str::is_empty)
                        })
                })
            };
            if live_status("open") && live_status("fixed") {
                open_and_fixed.push(entity.name.clone());
            }
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
        open_and_fixed.sort();
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
            open_and_fixed,
            literal: regex::Regex::new(r"^[\d.]+[a-z%]*$|[/~=(%]|^v?\d+(\.\d+)+").unwrap(),
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_pending_edge_is_not_live_in_the_bench_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let gm = crate::graph::GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        for (id, name) in [("one", "acme"), ("two", "linen"), ("three", "rust")] {
            gm.db()
                .query(format!(
                    "CREATE entity:{id} SET name = '{name}', entity_type = 'project', abstract = '', overview = ''"
                ))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        gm.db()
            .query(
                "RELATE entity:one->relates_to->entity:two SET rel_type = 'uses', confidence = 0.4, valid_from = time::now(), pending = { since: 's2', conflicts: 'relates_to:old' }",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        gm.db()
            .query(
                "RELATE entity:one->relates_to->entity:three SET rel_type = 'uses', confidence = 0.4, valid_from = time::now()",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        let mut first_beta = std::collections::HashMap::new();
        let edges = snapshot(&gm, &mut first_beta).await.unwrap();
        let pending = edges
            .iter()
            .find(|edge| edge.object == "linen")
            .expect("pending edge");
        let live = edges
            .iter()
            .find(|edge| edge.object == "rust")
            .expect("live edge");
        let mut response = gm
            .db()
            .query("SELECT id FROM relates_to WHERE pending != NONE")
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> =
            crate::graph::deserialize_take(&mut response, 0).unwrap();
        let stored = crate::graph::edge_view::record_id(rows[0].get("id").unwrap());
        assert_eq!(pending.id, stored);
        assert!(!pending.live);
        assert!(live.live);
    }

    #[test]
    fn grading_slot_ids_follow_the_snapshot_counter() {
        let mut seen = std::collections::HashMap::new();
        assert_eq!(grading_slot_id("base", &mut seen), "base");
        assert_eq!(grading_slot_id("base", &mut seen), "base|2");
        assert_eq!(grading_slot_id("other", &mut seen), "other");
    }

    #[test]
    fn rejected_slots_do_not_consume_the_grading_counter() {
        let rejected = serde_json::json!({
            "predicate": "setting",
            "key": "host",
            "value": "pine",
            "status": "rejected",
            "valid_from": "t0"
        });
        let decided = serde_json::json!({
            "predicate": "setting",
            "key": "host",
            "value": "pine",
            "status": "decided",
            "valid_from": "t0"
        });
        let mut grading = std::collections::HashMap::new();
        let mut side = std::collections::HashMap::new();
        let first =
            status_row_for_slot("entity:host", "deploy", &rejected, &mut grading, &mut side);
        let second =
            status_row_for_slot("entity:host", "deploy", &decided, &mut grading, &mut side);
        let third =
            status_row_for_slot("entity:host", "deploy", &rejected, &mut grading, &mut side);
        assert_eq!(first.id, "entity:host|setting|host|pine|t0|rejected");
        assert!(!first.live);
        assert_eq!(first.status.as_deref(), Some("rejected"));
        assert_eq!(second.id, "entity:host|setting|host|pine|t0");
        assert_eq!(second.status.as_deref(), Some("decided"));
        assert_eq!(third.id, "entity:host|setting|host|pine|t0|2|rejected");
    }

    #[test]
    fn a_blank_status_string_is_omitted() {
        assert_eq!(present_status(None), None);
        assert_eq!(present_status(Some(String::new())), None);
        assert_eq!(
            present_status(Some("decided".into())),
            Some("decided".into())
        );
        let slot = serde_json::json!({
            "predicate": "setting",
            "key": "host",
            "value": "pine",
            "valid_from": "t0"
        });
        let row = status_row_for_slot(
            "entity:host",
            "deploy",
            &slot,
            &mut std::collections::HashMap::new(),
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(row.id, "entity:host|setting|host|pine|t0|nostatus");
        assert!(row.status.is_none());
    }

    #[test]
    fn slot_liveness_matches_the_grading_rule() {
        let open = serde_json::json!({"status": "considered"});
        assert!(slot_live(&open));
        assert!(grading_slot("considered"));
        let pending = serde_json::json!({"pending": {"since": "s2"}, "status": "decided"});
        assert!(slot_pending(&pending));
        assert!(!slot_live(&pending));
        let closed = serde_json::json!({"valid_until": "2026-02-01"});
        assert!(!slot_live(&closed));
        let blank_until = serde_json::json!({"valid_until": ""});
        assert!(slot_live(&blank_until));
        let null_until = serde_json::json!({"valid_until": null});
        assert!(slot_live(&null_until));
        let rejected = serde_json::json!({"status": "rejected"});
        assert!(!slot_live(&rejected));
        assert!(!grading_slot("rejected"));
        assert!(!grading_slot(""));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn stored_status_includes_rejected_slots_and_pending_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let gm = crate::graph::GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        seed_status_graph(&gm).await;
        let mut first_beta = std::collections::HashMap::new();
        let edges = snapshot(&gm, &mut first_beta).await.unwrap();
        let status = snapshot_status(&gm).await.unwrap();
        assert_side_channel_matches_grading(&edges, &status);
        assert_stored_slot_status(&status);
        assert_stored_relationship_status(&status);
        assert_grading_unchanged_by_rejected_slots(&edges, &status);
    }

    #[cfg(feature = "embedded")]
    async fn seed_status_graph(gm: &GraphMemory) {
        for (id, name) in [
            ("host", "deploy"),
            ("linen", "linen"),
            ("rust", "rust"),
            ("clay", "clay"),
            ("wool", "wool"),
        ] {
            gm.db()
                .query(format!(
                    "CREATE entity:{id} SET name = '{name}', entity_type = 'project', abstract = '', overview = ''"
                ))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let slots = serde_json::json!([
            {"predicate": "setting", "key": "host", "value": "birch", "status": "rejected", "valid_from": "t0", "valid_until": "2026-02-01"},
            {"predicate": "setting", "key": "host", "value": "pine", "status": "decided", "valid_from": "t0"},
            {"predicate": "setting", "key": "host", "value": "oak", "status": "considered", "valid_from": "t0"},
            {"predicate": "setting", "key": "dup", "value": "same", "status": "decided", "valid_from": "t0"},
            {"predicate": "setting", "key": "dup", "value": "same", "status": "decided", "valid_from": "t0"},
            {"predicate": "setting", "key": "mode", "value": "fast", "status": "decided", "valid_from": "t0", "pending": {"since": "s2"}}
        ]);
        gm.db()
            .query("UPDATE type::record($id) SET attributes = $attributes")
            .bind(("id", "entity:host".to_string()))
            .bind((
                "attributes",
                serde_json::json!({"v2": {"kind": "", "slots": slots}}),
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
        relate(gm, "host", "linen", ", status = 'decided'").await;
        relate(gm, "host", "rust", "").await;
        relate(gm, "host", "clay", ", status = 'rejected'").await;
        relate(
            gm,
            "host",
            "wool",
            ", status = 'considered', pending = { since: 's2' }",
        )
        .await;
    }

    #[cfg(feature = "embedded")]
    async fn relate(gm: &GraphMemory, from: &str, to: &str, extra: &str) {
        gm.db()
            .query(format!(
                "RELATE entity:{from}->relates_to->entity:{to} SET rel_type = 'uses', confidence = 0.4, valid_from = time::now(){extra}"
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    #[cfg(feature = "embedded")]
    fn assert_side_channel_matches_grading(edges: &[ObservedEdge], status: &[StatusRow]) {
        assert!(edges.iter().all(|edge| edge.object != "birch"));
        assert!(edges.iter().all(|edge| !edge.id.ends_with("|rejected")));
        assert!(status.iter().any(|row| row.object == "birch"));
        for edge in edges {
            let row = status.iter().find(|row| row.id == edge.id).expect(&edge.id);
            assert_eq!(row.subject, edge.subject);
            assert_eq!(row.relation, edge.relation);
            assert_eq!(row.object, edge.object);
            assert_side_live(edge, row);
        }
        let clay = edges
            .iter()
            .find(|edge| edge.object == "clay")
            .expect("clay");
        assert!(clay.live);
    }

    #[cfg(feature = "embedded")]
    fn assert_side_live(edge: &ObservedEdge, row: &StatusRow) {
        if row.status.as_deref() == Some("rejected") {
            assert!(!row.live);
            return;
        }
        assert_eq!(row.live, edge.live);
    }

    #[cfg(feature = "embedded")]
    fn assert_stored_slot_status(status: &[StatusRow]) {
        let pine = status_object(status, "pine");
        let oak = status_object(status, "oak");
        let birch = status_object(status, "birch");
        let fast = status_object(status, "fast");
        assert_eq!(pine.status.as_deref(), Some("decided"));
        assert!(pine.live);
        assert_eq!(pine.relation, "setting:host");
        assert_eq!(oak.status.as_deref(), Some("considered"));
        assert!(oak.live);
        assert_eq!(birch.status.as_deref(), Some("rejected"));
        assert!(!birch.live);
        assert!(birch.id.ends_with("|rejected"));
        assert!(fast.pending);
        assert!(!fast.live);
        assert_eq!(fast.status.as_deref(), Some("decided"));
        let mut dups: Vec<&str> = status
            .iter()
            .filter(|row| row.object == "same")
            .map(|row| row.id.as_str())
            .collect();
        dups.sort();
        assert_eq!(dups.len(), 2);
        assert!(dups[0].ends_with("|setting|dup|same|t0"));
        assert!(dups[1].ends_with("|setting|dup|same|t0|2"));
    }

    #[cfg(feature = "embedded")]
    fn assert_stored_relationship_status(status: &[StatusRow]) {
        let linen = status_object(status, "linen");
        let rust = status_object(status, "rust");
        let clay = status_object(status, "clay");
        let wool = status_object(status, "wool");
        assert_eq!(linen.status.as_deref(), Some("decided"));
        assert!(linen.live);
        assert!(!linen.pending);
        assert!(rust.status.is_none());
        assert!(rust.live);
        assert_eq!(clay.status.as_deref(), Some("rejected"));
        assert!(!clay.live);
        assert_eq!(wool.status.as_deref(), Some("considered"));
        assert!(wool.pending);
        assert!(!wool.live);
    }

    #[cfg(feature = "embedded")]
    fn status_object<'a>(status: &'a [StatusRow], object: &str) -> &'a StatusRow {
        status
            .iter()
            .find(|row| row.object == object)
            .unwrap_or_else(|| panic!("missing {object}"))
    }

    #[cfg(feature = "embedded")]
    fn assert_grading_unchanged_by_rejected_slots(edges: &[ObservedEdge], status: &[StatusRow]) {
        let script = status_script();
        let weights = crate::graph::ProvenanceWeights::default();
        let plain = grade::grade(
            &script,
            &[observed_session("s1", edges.to_vec(), Vec::new())],
            &weights,
            &[],
            grade::ExtractorLabel::Scripted,
        )
        .unwrap();
        let with_status = grade::grade(
            &script,
            &[observed_session("s1", edges.to_vec(), status.to_vec())],
            &weights,
            &[],
            grade::ExtractorLabel::Scripted,
        )
        .unwrap();
        assert_eq!(plain.operations, with_status.operations);
        assert_eq!(plain.junk_edges, with_status.junk_edges);
        assert_eq!(plain.patterns, with_status.patterns);
        assert!(plain.patterns.is_some());
        assert!(plain.junk_edges.iter().all(|edge| edge.object != "birch"));
        assert_final_edge_json(&with_status);
    }

    #[cfg(feature = "embedded")]
    fn assert_final_edge_json(report: &grade::WriteReport) {
        let json = serde_json::to_value(report).unwrap();
        expect_final(&json, "pine", Some("decided"), false, true);
        expect_final(&json, "oak", Some("considered"), false, true);
        expect_final(&json, "birch", Some("rejected"), false, false);
        expect_final(&json, "fast", Some("decided"), true, false);
        expect_final(&json, "linen", Some("decided"), false, true);
        expect_final(&json, "rust", None, false, true);
        expect_final(&json, "clay", Some("rejected"), false, false);
        expect_final(&json, "wool", Some("considered"), true, false);
        let stripped = {
            let mut value = json.clone();
            value.as_object_mut().unwrap().remove("final_edges");
            value
        };
        let mut disabled = report.clone();
        disabled.final_edges.clear();
        assert_eq!(stripped, serde_json::to_value(&disabled).unwrap());
    }

    #[cfg(feature = "embedded")]
    fn expect_final(
        json: &serde_json::Value,
        object: &str,
        status: Option<&str>,
        pending: bool,
        live: bool,
    ) {
        let row = json["final_edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["object"] == object)
            .unwrap_or_else(|| panic!("missing {object}"));
        assert_eq!(row["live"], live);
        match status {
            Some(value) => assert_eq!(row["status"], value),
            None => assert!(row.get("status").is_none()),
        }
        if pending {
            assert_eq!(row["pending"], true);
        } else {
            assert!(row.get("pending").is_none());
        }
    }

    #[cfg(feature = "embedded")]
    fn observed_session(
        id: &str,
        edges: Vec<ObservedEdge>,
        status: Vec<StatusRow>,
    ) -> SessionObservation {
        SessionObservation {
            id: id.to_string(),
            before: Vec::new(),
            after: edges,
            calls: 0,
            millis: 0,
            input_tokens: None,
            output_tokens: None,
            warnings: Vec::new(),
            windows: 0,
            candidates: 0,
            kept: 0,
            drops: std::collections::HashMap::new(),
            resolve: crate::graph::types::ResolveStats::default(),
            retirements: Vec::new(),
            retirements_skipped: 0,
            timing: None,
            known_match: crate::graph::types::MatchReport::default(),
            consolidate: crate::graph::types::ConsolidateReport::default(),
            hold: crate::graph::types::HoldReport::default(),
            links: crate::graph::types::LinksReport::default(),
            component_edges: crate::graph::types::ComponentEdgesReport::default(),
            status_rows: StatusWindow {
                before: Vec::new(),
                after: status,
            },
        }
    }

    #[cfg(feature = "embedded")]
    fn status_script() -> FactScript {
        FactScript {
            name: "status".into(),
            project: None,
            cwd: None,
            sessions: vec![super::super::script::ScriptSession {
                id: "s1".into(),
                date: "2026-01-01T00:00:00Z".into(),
                turns: vec![ScriptTurn {
                    role: Speaker::User,
                    text: "status".into(),
                    claims: vec![],
                    noise: false,
                }],
                transcript: None,
                transcript_path: None,
                claims: vec![],
            }],
            facts: vec![ScriptFact {
                id: "host".into(),
                subject: "deploy".into(),
                relation: "setting:host".into(),
                object: "pine".into(),
                subject_type: "project".into(),
                object_type: "literal".into(),
                subject_abstract: None,
                object_abstract: None,
                end_state: super::super::script::EndState::Live,
                superseded_in: None,
                pattern: Some(super::super::script::FactPattern {
                    core_all: vec!["pine".into()],
                    detail_all: vec![],
                    wrong_any: vec![],
                }),
            }],
            expect: vec![],
            questions: vec![],
            superseded_pairs: vec![],
            noise: vec![],
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
