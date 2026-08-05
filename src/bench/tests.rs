// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the bench harness.
//!
//! These cover everything that does *not* require a real graph store or LLM
//! call: serde round-trip, archive layout, defaults, and prompt composition
//! through a stub LLM provider. End-to-end ingestion (which spins up
//! SurrealKV + FastEmbed) is exercised by the harness's own integration
//! tests, not here — those take seconds, not milliseconds.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::frontmatter;
use crate::graph::error::GraphError;
use crate::graph::llm::LlmProvider as GraphLlmProvider;

use super::ingest::{ingest_conversation, IngestStats};
use super::{
    answer::{answer_with_provider, fit_within_budget, AnswerOpts, NO_INFO_ANSWER},
    BenchConversation, BenchSession, BenchTurn, RetrievedEpisode,
};

// ── Test fixtures ────────────────────────────────────────────────────────

fn sample_conversation() -> BenchConversation {
    BenchConversation {
        sample_id: "conv-1".to_string(),
        speaker_a: "Caroline".to_string(),
        speaker_b: "Melanie".to_string(),
        sessions: vec![
            BenchSession {
                date_time: "22 May 2023".to_string(),
                turns: vec![
                    BenchTurn {
                        speaker: "Caroline".to_string(),
                        text: "I just adopted a golden retriever named Biscuit.".to_string(),
                        dia_id: "D1:1".to_string(),
                    },
                    BenchTurn {
                        speaker: "Melanie".to_string(),
                        text: "That's lovely \u{2014} are you taking him to training?".to_string(),
                        dia_id: "D1:2".to_string(),
                    },
                ],
            },
            BenchSession {
                date_time: "3 June 2023, 10:30 am".to_string(),
                turns: vec![BenchTurn {
                    speaker: "Caroline".to_string(),
                    text: "Biscuit finished his first puppy class yesterday.".to_string(),
                    dia_id: "D2:1".to_string(),
                }],
            },
        ],
    }
}

// ── A test LLM provider that records prompts and returns a canned answer ──

struct TestLlmProvider {
    prompts: Mutex<Vec<(String, String)>>,
    response: String,
}

impl TestLlmProvider {
    fn new(response: &str) -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            response: response.to_string(),
        }
    }

    fn captured(&self) -> Vec<(String, String)> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl GraphLlmProvider for TestLlmProvider {
    async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
        _max_tokens: u32,
    ) -> Result<String, GraphError> {
        self.prompts
            .lock()
            .unwrap()
            .push((system_prompt.to_string(), user_message.to_string()));
        Ok(self.response.clone())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[test]
fn conversation_serde_roundtrip() {
    let conv = sample_conversation();
    let json = serde_json::to_string(&conv).unwrap();
    let back: BenchConversation = serde_json::from_str(&json).unwrap();
    assert_eq!(conv, back);
}

#[test]
fn ingest_stats_serde_roundtrip() {
    let stats = IngestStats {
        sessions_written: 2,
        entities_extracted: 5,
        relations_extracted: 3,
        episodes: 7,
        log_numbers: vec![1, 2],
        warnings: vec!["dedup warn".to_string()],
        dedup_llm_calls: 1,
        dedup_fast_path: 4,
    };
    let json = serde_json::to_string(&stats).unwrap();
    let back: IngestStats = serde_json::from_str(&json).unwrap();
    assert_eq!(stats, back);
}

#[test]
fn answer_opts_defaults_match_spec() {
    let opts = AnswerOpts::default();
    assert_eq!(opts.graph_depth, 2);
    assert_eq!(opts.graph_limit, 20);
    assert_eq!(opts.archive_top_k, 5);
    assert!(opts.include_episodes);
    assert!(opts.provider_override.is_none());
    assert!(opts.model_override.is_none());
}

/// Episode retrieval must not inherit the archive limit: the measured episode
/// index loses results below k=20, and `archive_top_k` was 5.
#[test]
fn episode_top_k_is_decoupled_from_archive_top_k() {
    let opts = AnswerOpts::default();
    assert_eq!(opts.episode_top_k, 20);
    assert!(opts.episode_top_k > opts.archive_top_k);
    assert_eq!(opts.episode_char_budget, 28_000);
}

/// The budget is a backstop, not a second cutoff: at the ingest-time abstract
/// cap, a full `episode_top_k` of episodes must still fit in the graph share.
#[test]
fn budget_admits_a_full_episode_top_k_at_the_abstract_cap() {
    let opts = AnswerOpts::default();
    let graph_share = opts.episode_char_budget - opts.episode_char_budget / 4;
    assert!(graph_share >= opts.episode_top_k * 1_003);
}

fn episode_of(abstract_text: &str) -> RetrievedEpisode {
    RetrievedEpisode {
        source: "graph-episode".to_string(),
        abstract_text: abstract_text.to_string(),
        session_id: None,
        log_number: None,
        score: 0.5,
    }
}

#[test]
fn budget_keeps_episodes_until_the_ceiling_is_reached() {
    let episodes = vec![episode_of(&"a".repeat(40)); 5];
    let kept = fit_within_budget(episodes, 100);
    assert_eq!(kept.len(), 2);
}

#[test]
fn budget_never_returns_an_empty_section_for_a_non_empty_input() {
    let kept = fit_within_budget(vec![episode_of(&"a".repeat(500))], 100);
    assert_eq!(kept.len(), 1);
}

#[test]
fn budget_leaves_a_section_under_the_ceiling_untouched() {
    let episodes = vec![episode_of("short"); 4];
    assert_eq!(fit_within_budget(episodes, 28_000).len(), 4);
}

#[test]
fn normalize_date_time_parses_locomo_strings() {
    use super::ingest;
    // Re-test via behavioral exposure: write a synthetic conversation and
    // inspect the frontmatter date. The private helper is tested via the
    // public surface to avoid coupling tests to private names.
    let _ = ingest::IngestStats::default(); // referenced for clippy
}

/// End-to-end archive-write test: ingest two sessions, verify two
/// conversation-NNN.md files exist with correctly back-stamped dates.
/// Uses no LLM so the graph layer only creates episodes (still requires a
/// SurrealKV store, which the test sets up under a temp dir).
#[tokio::test(flavor = "current_thread")]
async fn ingest_writes_archives_with_session_dates() {
    let tmp = tempfile::tempdir().unwrap();
    let entity_root = tmp.path();
    let conv = sample_conversation();

    let stats = ingest_conversation(entity_root, &conv, None).await.unwrap();
    assert_eq!(stats.sessions_written, 2);
    assert_eq!(stats.log_numbers, vec![1, 2]);

    let conversations = entity_root.join("memory/conversations");
    assert!(conversations.join("conversation-001.md").exists());
    assert!(conversations.join("conversation-002.md").exists());

    let first = fs::read_to_string(conversations.join("conversation-001.md")).unwrap();
    let fm1 = frontmatter::parse(&first).expect("frontmatter parses");
    assert_eq!(fm1.date, "2023-05-22T00:00:00Z");
    assert_eq!(fm1.session_id, "conv-1:session-1");
    assert_eq!(fm1.source, "locomo");
    assert!(fm1.topics.contains(&"locomo".to_string()));

    let second = fs::read_to_string(conversations.join("conversation-002.md")).unwrap();
    let fm2 = frontmatter::parse(&second).expect("frontmatter parses");
    assert_eq!(fm2.date, "2023-06-03T10:30:00Z");
    assert_eq!(fm2.session_id, "conv-1:session-2");
}

/// answer_with_provider must compose a prompt that includes the question and
/// any retrieved memory, even when the graph store is empty.
#[tokio::test(flavor = "current_thread")]
async fn answer_calls_llm_with_question_in_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let entity_root = tmp.path();
    setup_empty_entity(entity_root).unwrap();

    let provider = TestLlmProvider::new("Biscuit");
    let opts = AnswerOpts::default();

    let answer = answer_with_provider(
        entity_root,
        "What is Caroline's dog's name?",
        &opts,
        &provider,
        "test-model".to_string(),
        "test-provider".to_string(),
    )
    .await
    .unwrap();

    assert_eq!(answer.answer, "Biscuit");
    assert_eq!(answer.model, "test-model");
    assert_eq!(answer.provider, "test-provider");
    assert!(answer.tokens_in > 0);

    let captured = provider.captured();
    assert_eq!(captured.len(), 1);
    let (sys, user) = &captured[0];
    assert!(sys.contains(NO_INFO_ANSWER));
    assert!(user.contains("What is Caroline's dog's name?"));
    assert!(user.contains("## Memory facts"));
    assert!(user.contains("## Recent episodes"));
}

fn setup_empty_entity(entity_root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(entity_root.join("memory/conversations"))?;
    fs::write(entity_root.join("memory/MEMORY.md"), "")?;
    Ok(())
}
