// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LoCoMo answer pipeline — hybrid retrieval (graph + archive) → LLM → answer.
//!
//! Wraps [`GraphMemory::query`] and [`search::ranked_search`] in exactly the
//! shape an entity uses at runtime, so benchmark scores reflect production
//! retrieval behavior.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::Provider;
use crate::error::RecallError;
use crate::graph::llm::LlmProvider as GraphLlmProvider;
use crate::graph::types::{MatchSource, QueryOptions, QueryResult};
use crate::graph::GraphMemory;
use crate::search;

// ── Public types ─────────────────────────────────────────────────────────

/// Share of [`AnswerOpts::episode_char_budget`] reserved for archive snippets,
/// as a divisor. Graph episodes are assembled first and would otherwise consume
/// the whole budget, starving a channel that is far cheaper per character.
const ARCHIVE_BUDGET_DIVISOR: usize = 4;

/// Options for [`answer_question`].
///
/// Defaults: graph depth 2, graph result limit 20, episode top-K 20, archive
/// top-K 5, episodes enabled.
#[derive(Debug, Clone)]
pub struct AnswerOpts {
    pub graph_depth: usize,
    pub graph_limit: usize,
    /// How many episodes to pull from graph episode search. Separate from
    /// [`Self::archive_top_k`]: the two channels have different cost per item
    /// and different recall curves, and the episode index measurably loses
    /// results below k=20 (`ef_search` is derived from k).
    pub episode_top_k: usize,
    /// How many archive files the ranked keyword search may contribute.
    pub archive_top_k: usize,
    /// Character ceiling for the assembled `## Recent episodes` section, so
    /// raising `episode_top_k` cannot grow the prompt without bound.
    ///
    /// A backstop, not the operative limit: the default is sized so both
    /// channels can deliver their full top-K at worst-case item size (20
    /// episodes at the 1,000-character ingest cap; 5 archive previews at the
    /// ~1,100 characters measured on the benchmark corpora). It bites only on
    /// pathologically large items, so `episode_top_k` stays meaningful.
    pub episode_char_budget: usize,
    pub include_episodes: bool,
    pub provider_override: Option<Provider>,
    pub model_override: Option<String>,
    /// Hard cap on tokens requested from the LLM. Default 512 — answers are
    /// expected to be one or two sentences.
    pub max_tokens: u32,
}

impl Default for AnswerOpts {
    fn default() -> Self {
        Self {
            graph_depth: 2,
            graph_limit: 20,
            episode_top_k: 20,
            archive_top_k: 5,
            episode_char_budget: 28_000,
            include_episodes: true,
            provider_override: None,
            model_override: None,
            max_tokens: 512,
        }
    }
}

/// A single retrieved fact, derived from a graph entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievedFact {
    pub name: String,
    pub entity_type: String,
    pub abstract_text: String,
    pub overview: String,
    pub score: f64,
    pub source: String,
}

/// A single retrieved episode (graph episode search or archive file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievedEpisode {
    pub source: String,
    pub abstract_text: String,
    pub session_id: Option<String>,
    pub log_number: Option<i64>,
    pub score: f64,
}

/// Full benchmark answer payload, serialised verbatim to JSON for the harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchAnswer {
    pub answer: String,
    pub retrieved_facts: Vec<RetrievedFact>,
    pub retrieved_episodes: Vec<RetrievedEpisode>,
    pub model: String,
    pub provider: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub latency_ms: u64,
}

/// Sentinel returned when retrieval surfaces nothing useful.
pub const NO_INFO_ANSWER: &str = "I don't have enough information to answer.";

// ── Entry point ──────────────────────────────────────────────────────────

/// Answer a question for an entity, with a real LLM provider resolved from the
/// entity's config (plus optional overrides from [`AnswerOpts`]).
pub async fn answer_question(
    entity_root: &Path,
    question: &str,
    opts: AnswerOpts,
) -> Result<BenchAnswer, RecallError> {
    let memory_dir = entity_root.join("memory");
    let (provider, model) = crate::llm_provider::create_provider(
        &memory_dir,
        opts.provider_override
            .as_ref()
            .map(|p| p.to_string())
            .as_deref(),
        opts.model_override.as_deref(),
    )?;

    let provider_label = opts
        .provider_override
        .clone()
        .unwrap_or_else(|| crate::config::load(&memory_dir).llm.provider)
        .to_string();

    answer_with_provider(
        entity_root,
        question,
        &opts,
        provider.as_ref(),
        model,
        provider_label,
    )
    .await
}

/// Answer a question with a caller-supplied LLM provider — used by tests to
/// avoid network calls, and by the harness when it wants to pin a specific
/// provider/model pair outside of recall-echo's config resolution.
pub async fn answer_with_provider(
    entity_root: &Path,
    question: &str,
    opts: &AnswerOpts,
    llm: &dyn GraphLlmProvider,
    model: String,
    provider_label: String,
) -> Result<BenchAnswer, RecallError> {
    let memory_dir = entity_root.join("memory");
    let started = Instant::now();

    let facts = retrieve_facts(&memory_dir, question, opts).await?;
    let episodes = retrieve_episodes(&memory_dir, question, opts).await?;
    let memory_md = read_memory_md(&memory_dir);

    let system_prompt = build_system_prompt();
    let user_message = build_user_message(&memory_md, &facts, &episodes, question);

    let answer_text = llm
        .complete(&system_prompt, &user_message, opts.max_tokens)
        .await?;

    let latency_ms = started.elapsed().as_millis() as u64;

    Ok(BenchAnswer {
        answer: answer_text.trim().to_string(),
        retrieved_facts: facts,
        retrieved_episodes: episodes,
        model,
        provider: provider_label,
        tokens_in: estimate_tokens(&system_prompt) + estimate_tokens(&user_message),
        tokens_out: estimate_tokens(&answer_text),
        latency_ms,
    })
}

// ── Retrieval ────────────────────────────────────────────────────────────

async fn retrieve_facts(
    memory_dir: &Path,
    question: &str,
    opts: &AnswerOpts,
) -> Result<Vec<RetrievedFact>, RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Ok(Vec::new());
    }

    let gm = GraphMemory::open(&graph_dir).await?;
    let query_opts = QueryOptions {
        limit: opts.graph_limit,
        entity_type: None,
        keyword: None,
        graph_depth: opts.graph_depth as u32,
        include_episodes: opts.include_episodes,
    };

    let QueryResult { entities, .. } = gm.query(question, &query_opts).await?;

    let mut facts = Vec::with_capacity(entities.len());
    let mut seen = std::collections::HashSet::new();
    for scored in entities {
        let key = scored.entity.name.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        let source = match &scored.source {
            MatchSource::Semantic => "semantic".to_string(),
            MatchSource::Keyword => "keyword".to_string(),
            MatchSource::Graph { parent, rel_type } => {
                format!("graph:{parent}/{rel_type}")
            }
        };
        facts.push(RetrievedFact {
            name: scored.entity.name,
            entity_type: scored.entity.entity_type.to_string(),
            abstract_text: scored.entity.abstract_text,
            overview: scored.entity.overview,
            score: scored.score,
            source,
        });
    }
    Ok(facts)
}

async fn retrieve_episodes(
    memory_dir: &Path,
    question: &str,
    opts: &AnswerOpts,
) -> Result<Vec<RetrievedEpisode>, RecallError> {
    let archive_budget = opts.episode_char_budget / ARCHIVE_BUDGET_DIVISOR;
    let graph_budget = opts.episode_char_budget - archive_budget;

    let mut episodes = fit_within_budget(
        search_graph_episodes(memory_dir, question, opts).await?,
        graph_budget,
    );
    episodes.extend(fit_within_budget(
        search_archive(memory_dir, question, opts)?,
        archive_budget,
    ));

    Ok(episodes)
}

async fn search_graph_episodes(
    memory_dir: &Path,
    question: &str,
    opts: &AnswerOpts,
) -> Result<Vec<RetrievedEpisode>, RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !opts.include_episodes || !graph_dir.exists() {
        return Ok(Vec::new());
    }

    let gm = GraphMemory::open(&graph_dir).await?;
    let found = gm
        .search_episodes(question, opts.episode_top_k.max(1))
        .await?;

    Ok(found
        .into_iter()
        .map(|ep| RetrievedEpisode {
            source: "graph-episode".to_string(),
            abstract_text: ep.episode.abstract_text,
            session_id: Some(ep.episode.session_id),
            log_number: ep.episode.log_number,
            score: ep.score,
        })
        .collect())
}

fn search_archive(
    memory_dir: &Path,
    question: &str,
    opts: &AnswerOpts,
) -> Result<Vec<RetrievedEpisode>, RecallError> {
    if !memory_dir.join("conversations").exists() {
        return Ok(Vec::new());
    }

    let ranked = match search::ranked_search(question, memory_dir, opts.archive_top_k) {
        Ok(ranked) => ranked,
        Err(RecallError::NotInitialized(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    Ok(ranked
        .into_iter()
        .map(|file| RetrievedEpisode {
            source: format!("archive:{}", file.file),
            abstract_text: file.preview_lines.join(" / "),
            session_id: None,
            log_number: None,
            score: file.score,
        })
        .collect())
}

/// Keep the highest-ranked episodes that fit `budget` characters of abstract
/// text, in order. The first item is always kept — an empty section is worse
/// than one oversized entry, and per-episode size is bounded at ingest.
pub(super) fn fit_within_budget(
    episodes: Vec<RetrievedEpisode>,
    budget: usize,
) -> Vec<RetrievedEpisode> {
    let mut spent = 0usize;
    episodes
        .into_iter()
        .enumerate()
        .take_while(|(index, ep)| {
            spent += ep.abstract_text.len();
            *index == 0 || spent <= budget
        })
        .map(|(_, ep)| ep)
        .collect()
}

fn read_memory_md(memory_dir: &Path) -> String {
    fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap_or_default()
}

// ── Prompt composition ───────────────────────────────────────────────────

fn build_system_prompt() -> String {
    "You are answering a question based on your memory of past conversations. \
     Use only the facts and episodes provided. If the memory does not contain \
     the answer, reply exactly: \"I don't have enough information to answer.\""
        .to_string()
}

fn build_user_message(
    memory_md: &str,
    facts: &[RetrievedFact],
    episodes: &[RetrievedEpisode],
    question: &str,
) -> String {
    let mut buf = String::new();

    if !memory_md.trim().is_empty() {
        buf.push_str("## Curated memory\n\n");
        buf.push_str(memory_md.trim());
        buf.push_str("\n\n");
    }

    buf.push_str("## Memory facts\n\n");
    if facts.is_empty() {
        buf.push_str("(none)\n\n");
    } else {
        for fact in facts {
            buf.push_str(&format!(
                "- **{}** ({}, score {:.2}): {}\n",
                fact.name,
                fact.entity_type,
                fact.score,
                fact.abstract_text.trim()
            ));
            if !fact.overview.trim().is_empty() {
                buf.push_str(&format!("  {}\n", fact.overview.trim()));
            }
        }
        buf.push('\n');
    }

    buf.push_str("## Recent episodes\n\n");
    if episodes.is_empty() {
        buf.push_str("(none)\n\n");
    } else {
        for ep in episodes {
            let session = ep.session_id.as_deref().unwrap_or("-");
            buf.push_str(&format!(
                "- [{}] session={} score={:.2}: {}\n",
                ep.source,
                session,
                ep.score,
                ep.abstract_text.trim()
            ));
        }
        buf.push('\n');
    }

    buf.push_str("## Question\n\n");
    buf.push_str(question);
    buf.push_str("\n\nAnswer concisely. State only the answer; do not narrate your reasoning.\n");
    buf
}

// ── Token estimation ─────────────────────────────────────────────────────

/// Coarse char/4 token estimate. Good enough for benchmark accounting where
/// the harness mostly cares about relative cost between runs, not exact
/// tokenizer-accurate counts.
fn estimate_tokens(s: &str) -> u32 {
    s.len().div_ceil(4) as u32
}
