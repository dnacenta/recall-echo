// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Entity dedup: which candidates are worth a model call.
//!
//! The property under test is cost. Dedup used to escalate on the *blended*
//! retrieval score (`0.45·similarity + 0.30·hotness + 0.25·utility`), so a
//! popular entity bought a model call for any candidate that drifted near it —
//! and every entity gets more popular as a graph grows, which is why ingest
//! cost climbed 9× across the LongMemEval baseline. Escalation now happens on
//! raw cosine similarity, in a band; everything outside the band is decided
//! locally. Each test below asserts a fast path by making the model provider
//! fatal: if dedup calls it, the test panics.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use recall_echo::graph::dedup::{self, ResolutionPath, ResolvedEntity};
use recall_echo::graph::error::GraphError;
use recall_echo::graph::llm::LlmProvider;
use recall_echo::graph::types::{EntityType, ExtractedEntity, NewEntity};
use recall_echo::graph::{GraphMemory, IngestContext};
use tempfile::TempDir;

const SESSION: &str = "conversation-001";

// ── Fixture ──────────────────────────────────────────────────────────────

/// A graph store whose `[graph.dedup]` section the test controls.
struct Fixture {
    _dir: TempDir,
    graph: GraphMemory,
}

impl Fixture {
    /// A store on the shipped defaults.
    async fn new() -> Self {
        Self::with_dedup_config("").await
    }

    /// A store whose dedup thresholds are pinned by the test, so a band can be
    /// exercised without seeding a graph large enough to produce one by luck.
    async fn with_dedup_config(dedup_toml: &str) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let graph_dir = dir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).expect("graph dir");

        if !dedup_toml.is_empty() {
            std::fs::write(
                dir.path().join(".recall-echo.toml"),
                format!("[graph]\n\n[graph.dedup]\n{dedup_toml}"),
            )
            .expect("write config");
        }

        let graph = GraphMemory::open(&graph_dir).await.expect("open graph");
        Self { _dir: dir, graph }
    }

    async fn seed(&self, name: &str, entity_type: EntityType, abstract_text: &str) {
        self.graph
            .add_entity(NewEntity {
                name: name.to_string(),
                entity_type,
                abstract_text: abstract_text.to_string(),
                overview: None,
                content: None,
                attributes: None,
                source: Some(SESSION.to_string()),
            })
            .await
            .unwrap_or_else(|e| panic!("seed {name}: {e}"));
    }

    /// Read an entity's stored abstract back.
    async fn abstract_of(&self, name: &str) -> String {
        self.graph
            .get_entity(name)
            .await
            .expect("lookup")
            .unwrap_or_else(|| panic!("{name} is not in the store"))
            .abstract_text
    }

    /// Raise an entity's access count the only way the runtime does: by
    /// retrieving it. Hotness is what used to drag unrelated candidates over
    /// the old gate.
    async fn warm(&self, query: &str, times: usize) {
        for _ in 0..times {
            self.graph.search(query, 5).await.expect("search");
        }
    }
}

fn candidate(name: &str, entity_type: EntityType, abstract_text: &str) -> ExtractedEntity {
    ExtractedEntity {
        name: name.to_string(),
        entity_type,
        abstract_text: abstract_text.to_string(),
        overview: None,
        content: None,
        attributes: None,
    }
}

// ── Providers ────────────────────────────────────────────────────────────

/// Fatal on use: the assertion that a fast path is a fast path.
struct NoModel;

#[async_trait]
impl LlmProvider for NoModel {
    async fn complete(&self, _system: &str, user: &str, _max: u32) -> Result<String, GraphError> {
        panic!("dedup paid for a model call it should have decided locally:\n{user}");
    }
}

/// Counts dedup calls and answers with one canned decision.
struct CountingModel {
    calls: AtomicUsize,
    prompts: Mutex<Vec<String>>,
    decision: String,
}

impl CountingModel {
    fn new(decision: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
            decision: decision.to_string(),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_prompt(&self) -> String {
        self.prompts
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl LlmProvider for CountingModel {
    async fn complete(&self, _system: &str, user: &str, _max: u32) -> Result<String, GraphError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(user.to_string());
        Ok(self.decision.clone())
    }
}

/// Extraction and dedup from one provider, told apart by the system prompt,
/// counting the dedup half. What an ingest run's model bill looks like.
struct ScriptedModel {
    extraction: String,
    dedup_decision: String,
    dedup_calls: AtomicUsize,
}

impl ScriptedModel {
    fn new(extraction: &str, dedup_decision: &str) -> Self {
        Self {
            extraction: extraction.to_string(),
            dedup_decision: dedup_decision.to_string(),
            dedup_calls: AtomicUsize::new(0),
        }
    }

    fn dedup_calls(&self) -> usize {
        self.dedup_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for ScriptedModel {
    async fn complete(&self, system: &str, _user: &str, _max: u32) -> Result<String, GraphError> {
        if system.contains("deduplication system") {
            self.dedup_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(self.dedup_decision.clone());
        }
        Ok(self.extraction.clone())
    }
}

// ── The three bands ──────────────────────────────────────────────────────

#[tokio::test]
async fn an_empty_graph_creates_without_a_model_call() {
    let fixture = Fixture::new().await;

    let resolution = dedup::resolve_entity(
        &fixture.graph,
        &NoModel,
        &candidate("Biscuit", EntityType::Person, "A golden retriever"),
        SESSION,
    )
    .await
    .expect("resolve");

    assert_eq!(resolution.path, ResolutionPath::NewEntityBand);
    assert!(matches!(resolution.entity, ResolvedEntity::Created(_)));
}

/// The cheapest duplicate there is: the same string. It never reaches the
/// embedding search, let alone the model.
#[tokio::test]
async fn the_same_name_and_type_merges_without_a_model_call() {
    let fixture = Fixture::new().await;
    fixture
        .seed("Biscuit", EntityType::Person, "A dog Caroline adopted")
        .await;

    let resolution = dedup::resolve_entity(
        &fixture.graph,
        &NoModel,
        &candidate(
            "Biscuit",
            EntityType::Person,
            "A golden retriever Caroline adopted in May 2023",
        ),
        SESSION,
    )
    .await
    .expect("resolve");

    assert_eq!(resolution.path, ResolutionPath::NameMatch);
    assert!(matches!(resolution.entity, ResolvedEntity::Merged(_)));
    assert_eq!(
        fixture.abstract_of("Biscuit").await,
        "A golden retriever Caroline adopted in May 2023",
        "the fuller abstract wins the merge"
    );
}

/// Re-reading an archive must not grow the entity it describes — and must not
/// pay a model to say so.
#[tokio::test]
async fn a_repeat_of_what_is_stored_is_skipped_without_a_model_call() {
    let fixture = Fixture::new().await;
    fixture
        .seed("Biscuit", EntityType::Person, "A golden retriever")
        .await;

    let resolution = dedup::resolve_entity(
        &fixture.graph,
        &NoModel,
        &candidate("Biscuit", EntityType::Person, "A golden retriever"),
        SESSION,
    )
    .await
    .expect("resolve");

    assert_eq!(resolution.path, ResolutionPath::NameMatch);
    assert!(matches!(resolution.entity, ResolvedEntity::Skipped));
}

/// A name shared across kinds is not a shared identity: the event is not the
/// project, so the bands, not the name, decide.
#[tokio::test]
async fn the_same_name_under_a_different_type_is_not_a_name_match() {
    let fixture = Fixture::new().await;
    fixture
        .seed(
            "Launch",
            EntityType::Event,
            "The team shipped v1 on 3 June 2023",
        )
        .await;

    let resolution = dedup::resolve_entity(
        &fixture.graph,
        &NoModel,
        &candidate(
            "Launch",
            EntityType::Project,
            "A woodworking project: building a canoe over the summer",
        ),
        SESSION,
    )
    .await
    .expect("resolve");

    assert_eq!(resolution.path, ResolutionPath::NewEntityBand);
    assert!(matches!(resolution.entity, ResolvedEntity::Created(_)));
}

/// The regression this work exists for: a hot entity whose *blended* score
/// clears the old 0.7 gate, and a candidate that is nothing like it. The old
/// gate escalated; the similarity band does not.
#[tokio::test]
async fn a_hot_but_unrelated_entity_no_longer_buys_a_model_call() {
    let fixture = Fixture::new().await;
    fixture
        .seed(
            "Espresso machine",
            EntityType::Tool,
            "Melanie bought an espresso machine for her kitchen last spring",
        )
        .await;
    fixture.warm("espresso machine kitchen", 60).await;

    let candidate = candidate(
        "Road bicycle",
        EntityType::Tool,
        "Melanie bought a road bicycle for her commute last autumn",
    );

    let hit = fixture
        .graph
        .search(&candidate.abstract_text, 1)
        .await
        .expect("search")
        .remove(0);
    let review = fixture.graph.dedup_config().review_similarity;
    assert!(
        hit.score > 0.7,
        "fixture must reproduce the old gate: blended score {:.3} on {}",
        hit.score,
        hit.entity.name
    );
    assert!(
        hit.similarity() < review,
        "fixture must be genuinely unalike: similarity {:.3} >= {review}",
        hit.similarity()
    );

    let resolution = dedup::resolve_entity(&fixture.graph, &NoModel, &candidate, SESSION)
        .await
        .expect("resolve");

    assert_eq!(resolution.path, ResolutionPath::NewEntityBand);
}

/// The middle band is the one thing worth paying for, and it costs exactly one
/// call. Thresholds are pinned so every neighbour lands in it.
#[tokio::test]
async fn the_ambiguous_band_costs_exactly_one_model_call() {
    let fixture =
        Fixture::with_dedup_config("certain_similarity = 1.0\nreview_similarity = 0.0\n").await;
    fixture
        .seed("Biscuit", EntityType::Person, "A dog Caroline adopted")
        .await;

    let model = CountingModel::new(r#"{"decision": "skip", "reason": "duplicate"}"#);
    let resolution = dedup::resolve_entity(
        &fixture.graph,
        &model,
        &candidate("Biscuit the dog", EntityType::Person, "Caroline's dog"),
        SESSION,
    )
    .await
    .expect("resolve");

    assert_eq!(resolution.path, ResolutionPath::LlmDecision);
    assert!(matches!(resolution.entity, ResolvedEntity::Skipped));
    assert_eq!(model.calls(), 1);
    assert!(
        model.last_prompt().contains("similarity:"),
        "the model weighs meaning, not popularity: {}",
        model.last_prompt()
    );
}

/// However large the store, the prompt stays the same size.
#[tokio::test]
async fn the_candidate_set_handed_to_the_model_is_capped() {
    let fixture = Fixture::with_dedup_config(
        "certain_similarity = 1.0\nreview_similarity = 0.0\nmax_candidates = 2\n",
    )
    .await;
    for i in 1..=6 {
        fixture
            .seed(
                &format!("Dog {i}"),
                EntityType::Person,
                &format!("A dog Caroline adopted, number {i}"),
            )
            .await;
    }

    let model = CountingModel::new(r#"{"decision": "create", "reason": "new"}"#);
    dedup::resolve_entity(
        &fixture.graph,
        &model,
        &candidate("Biscuit", EntityType::Person, "A dog Caroline adopted"),
        SESSION,
    )
    .await
    .expect("resolve");

    let prompt = model.last_prompt();
    assert_eq!(
        prompt.matches("similarity:").count(),
        2,
        "max_candidates = 2 must bound the comparison set:\n{prompt}"
    );
}

// ── The counters an ingest run reports ───────────────────────────────────

const ARCHIVE: &str = "### User\n\nI adopted a golden retriever named Biscuit last May.\n";

const EXTRACTION: &str = r#"{"entities": [{"name": "Biscuit", "type": "person",
    "abstract": "A golden retriever Caroline adopted in May"}], "relationships": []}"#;

/// Ingest reports what dedup cost. Re-ingesting the same archive must cost
/// nothing at all: every candidate is already in the store under its own name.
#[tokio::test]
async fn ingest_reports_dedup_resolved_without_a_model() {
    let fixture = Fixture::new().await;
    let model = ScriptedModel::new(EXTRACTION, r#"{"decision": "skip", "reason": "dup"}"#);
    let context = IngestContext::new(SESSION, Some(1));

    let first = fixture
        .graph
        .ingest_archive(ARCHIVE, &context, Some(&model))
        .await
        .expect("first ingest");
    assert_eq!(first.entities_created, 1);
    assert_eq!(first.dedup_llm_calls, 0);
    assert_eq!(first.dedup_fast_path, 1);
    assert_eq!(
        first.estimated_tokens, 2_500,
        "the estimate bills extraction only when dedup costs nothing"
    );

    let second = fixture
        .graph
        .ingest_archive(ARCHIVE, &context, Some(&model))
        .await
        .expect("second ingest");
    assert_eq!(second.entities_skipped, 1);
    assert_eq!(second.dedup_llm_calls, 0);
    assert_eq!(second.dedup_fast_path, 1);
    assert_eq!(model.dedup_calls(), 0, "no dedup decision needed a model");
}

/// And when a candidate does land in the ambiguous band, the counter says so.
#[tokio::test]
async fn ingest_counts_the_candidates_that_needed_a_model() {
    let fixture =
        Fixture::with_dedup_config("certain_similarity = 1.0\nreview_similarity = 0.0\n").await;
    fixture
        .seed("Rusty", EntityType::Person, "A dog Caroline adopted")
        .await;

    let model = ScriptedModel::new(EXTRACTION, r#"{"decision": "create", "reason": "new dog"}"#);
    let context = IngestContext::new(SESSION, Some(1));

    let report = fixture
        .graph
        .ingest_archive(ARCHIVE, &context, Some(&model))
        .await
        .expect("ingest");

    assert_eq!(report.dedup_llm_calls, 1);
    assert_eq!(report.dedup_fast_path, 0);
    assert_eq!(model.dedup_calls(), 1);
    assert_eq!(report.entities_created, 1);
}
