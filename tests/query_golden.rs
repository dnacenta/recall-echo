//! Golden-set integration tests for the hybrid recall path (`src/graph/query.rs`).
//!
//! The hybrid path is: semantic KNN (`limit * 2` candidates) → 1-hop expansion
//! from the top 3 candidates scored `parent_score * effective_confidence` →
//! merge/dedup → sort → truncate. This file pins the behaviors that Phase 2
//! retrieval work must not silently change: recall@k membership, confidence
//! weighting (including read-time decay and the `< 0.1` edge drop), scoring
//! weight sensitivity, and expansion depth.
//!
//! These tests drive the graph layer one level below `GraphMemory` (raw
//! `Surreal<Db>` + `query::query`) for two reasons the higher-level API cannot
//! serve: scoring weights must be varied per call (`GraphMemory` binds them at
//! open time from `.recall-echo.toml`), and edges must be backdated to exercise
//! temporal decay.
//!
//! Semantic contrasts in the fixture are deliberately coarse (systems
//! programming vs bread baking vs offshore sailing) so assertions survive
//! embedding-model churn.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use recall_echo::config::GraphScoringConfig;
use recall_echo::graph::embed::FastEmbedder;
use recall_echo::graph::store::{self, Db};
use recall_echo::graph::types::{
    EntityType, MatchSource, NewEntity, NewEpisode, NewRelationship, QueryOptions, QueryResult,
    ScoredEntity,
};
use recall_echo::graph::{crud, query};
use surrealdb::Surreal;
use tempfile::TempDir;

// ── Entity names under assertion ─────────────────────────────────────

const BORROW_CHECKER: &str = "Borrow Checker";
const OWNERSHIP_MODEL: &str = "Ownership Model";
const CARGO: &str = "Cargo";

const SOURDOUGH_STARTER: &str = "Sourdough Starter";
const BULK_FERMENTATION: &str = "Bulk Fermentation";
const DUTCH_OVEN: &str = "Dutch Oven";

const CELESTIAL_NAVIGATION: &str = "Celestial Navigation";
const SPINNAKER: &str = "Spinnaker";

/// Neighbor of [`BORROW_CHECKER`] via a fresh, high-confidence edge.
const INES: &str = "Ines Marchetti";
/// Neighbor of [`BORROW_CHECKER`] via a two-year-old, low-confidence edge.
const OTTO: &str = "Otto Brenner";
/// Neighbor of [`BORROW_CHECKER`] via a high-confidence but half-year-old edge.
const LUCIA: &str = "Lucia Fontana";
/// Two hops from [`BORROW_CHECKER`] (via [`INES`]).
const ESTUDIO: &str = "Estudio Marchetti";
/// Unconnected person — never reachable by expansion.
const PRIYA: &str = "Priya Raghavan";
const MIREIA: &str = "Mireia Costa";

/// High similarity to [`SENSOR_QUERY`], utility pinned to 0.0.
const KALMAN_FILTER: &str = "Kalman Filter";
/// Low similarity to every fixture query, utility pinned to 1.0.
const MUNICIPAL_BONDS: &str = "Municipal Bond Yields";

const RUST_QUERY: &str = "Rust borrow checker ownership and lifetimes";
const BAKING_QUERY: &str = "sourdough starter and bulk fermentation for artisan bread";
const SENSOR_QUERY: &str = "Kalman filter sensor fusion state estimation";

// ── Fixture ──────────────────────────────────────────────────────────

/// One process-wide embedder. The ONNX model is downloaded and loaded once per
/// test-binary run; `CARGO_TARGET_TMPDIR` keeps the cache out of the per-test
/// temp dirs so re-runs are offline.
fn embedder() -> &'static FastEmbedder {
    static EMBEDDER: OnceLock<FastEmbedder> = OnceLock::new();
    EMBEDDER.get_or_init(|| {
        let cache_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fastembed-cache");
        std::fs::create_dir_all(&cache_dir).expect("failed to create embedder cache dir");
        FastEmbedder::new(&cache_dir).expect("failed to initialize embedder")
    })
}

/// A freshly seeded golden graph in its own temp dir.
///
/// Each test gets its own store: semantic search mutates `access_count`, which
/// feeds the hotness term, so a shared store would leak scoring state between
/// tests.
struct GoldenGraph {
    db: Surreal<Db>,
    _dir: TempDir,
}

impl GoldenGraph {
    async fn seed() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let graph_path = dir.path().join("graph");
        std::fs::create_dir_all(&graph_path).expect("failed to create graph dir");

        let db = store::open(&graph_path)
            .await
            .expect("failed to open store");
        store::init_schema(&db)
            .await
            .expect("failed to init schema");

        let graph = Self { db, _dir: dir };
        graph.seed_entities().await;
        graph.seed_relationships().await;
        graph.seed_episodes().await;
        graph
    }

    async fn seed_entities(&self) {
        let entities = [
            (
                BORROW_CHECKER,
                EntityType::Concept,
                "The Rust borrow checker enforces ownership and lifetime rules at compile time, \
                 rejecting programs that alias or free memory unsafely.",
            ),
            (
                OWNERSHIP_MODEL,
                EntityType::Concept,
                "Rust ownership and move semantics decide when a value is dropped and its heap \
                 memory is released.",
            ),
            (
                CARGO,
                EntityType::Tool,
                "Cargo is the Rust build system and package manager that resolves crate \
                 dependencies and runs the compiler.",
            ),
            (
                SOURDOUGH_STARTER,
                EntityType::Concept,
                "A sourdough starter is a living culture of wild yeast and lactobacilli kept \
                 alive by regular flour and water refreshments to leaven bread.",
            ),
            (
                BULK_FERMENTATION,
                EntityType::Concept,
                "Bulk fermentation is the long first rise of bread dough after the levain is \
                 mixed in, before the loaves are shaped.",
            ),
            (
                DUTCH_OVEN,
                EntityType::Tool,
                "A cast iron dutch oven traps steam so an artisan loaf bakes with an open crumb \
                 and a blistered crust.",
            ),
            (
                CELESTIAL_NAVIGATION,
                EntityType::Concept,
                "Sailors fix their position offshore with a sextant, a nautical almanac and star \
                 sights taken at dawn.",
            ),
            (
                SPINNAKER,
                EntityType::Tool,
                "A spinnaker is the large downwind sail flown from the bow of a racing yacht.",
            ),
            (
                INES,
                EntityType::Person,
                "Trail runner and amateur luthier who restores nineteenth century parlour \
                 guitars in Lisbon.",
            ),
            (
                OTTO,
                EntityType::Person,
                "Retired ferry captain who collects nautical charts of the Baltic coast.",
            ),
            (
                LUCIA,
                EntityType::Person,
                "Watercolour painter who teaches botanical illustration classes in Bologna.",
            ),
            (
                ESTUDIO,
                EntityType::Project,
                "A workshop in Alfama that rebuilds soundboards and necks for parlour guitars.",
            ),
            (
                PRIYA,
                EntityType::Person,
                "Marine biologist studying coral bleaching on Red Sea reefs.",
            ),
            (
                MIREIA,
                EntityType::Person,
                "Baker who runs a wood fired bakery in Girona and teaches weekend bread courses.",
            ),
            (
                KALMAN_FILTER,
                EntityType::Concept,
                "The Kalman filter fuses noisy sensor measurements into a smoothed state estimate \
                 for a moving vehicle.",
            ),
            (
                MUNICIPAL_BONDS,
                EntityType::Concept,
                "Municipal bond yields slipped after the treasury auction repriced ten year notes.",
            ),
        ];

        for (name, entity_type, abstract_text) in entities {
            crud::add_entity(
                &self.db,
                embedder(),
                NewEntity {
                    name: name.to_string(),
                    entity_type,
                    abstract_text: abstract_text.to_string(),
                    overview: None,
                    content: None,
                    attributes: None,
                    source: Some("golden".to_string()),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("failed to seed entity {name}: {e}"));
        }
    }

    async fn seed_relationships(&self) {
        // Fresh, high-confidence edge — effective confidence stays ~0.95.
        self.relate(BORROW_CHECKER, INES, "MAINTAINED_BY", 0.95)
            .await;

        // Two years old and weak: 0.30 × 0.5^(730/90) hits the decay floor
        // (0.05) and must be dropped by the `< 0.1` filter.
        let stale = self
            .relate(BORROW_CHECKER, OTTO, "DOCUMENTED_BY", 0.30)
            .await;
        self.age_edge(&stale, 730).await;

        // Half a year old, never reinforced: 0.90 × 0.5^(180/90) ≈ 0.225 —
        // survives the filter but is heavily discounted.
        let aging = self
            .relate(BORROW_CHECKER, LUCIA, "REVIEWED_BY", 0.90)
            .await;
        self.age_unreinforced_edge(&aging, 180).await;

        // Second hop out of INES — must never surface from a query anchored on
        // BORROW_CHECKER, because expansion is one hop.
        self.relate(INES, ESTUDIO, "FOUNDED", 1.0).await;

        self.relate(OWNERSHIP_MODEL, BORROW_CHECKER, "RELATED_TO", 1.0)
            .await;
        self.relate(SOURDOUGH_STARTER, MIREIA, "MAINTAINED_BY", 0.95)
            .await;
        self.relate(BULK_FERMENTATION, SOURDOUGH_STARTER, "REQUIRES", 1.0)
            .await;
        self.relate(DUTCH_OVEN, MIREIA, "USED_BY", 0.80).await;
        self.relate(CELESTIAL_NAVIGATION, OTTO, "PRACTICED_BY", 0.90)
            .await;
    }

    async fn seed_episodes(&self) {
        let episodes = [
            (
                "s-rust",
                "Debugged a lifetime error where a mutable borrow outlived the struct it pointed \
                 at.",
            ),
            (
                "s-bake",
                "Refreshed the sourdough starter twice and shortened bulk fermentation because \
                 the kitchen was warm.",
            ),
            (
                "s-sail",
                "Practised sextant sights at dawn and compared the fix against the chart plotter.",
            ),
        ];

        for (session_id, abstract_text) in episodes {
            crud::add_episode(
                &self.db,
                embedder(),
                NewEpisode {
                    session_id: session_id.to_string(),
                    abstract_text: abstract_text.to_string(),
                    overview: None,
                    content: None,
                    log_number: None,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("failed to seed episode {session_id}: {e}"));
        }
    }

    /// Create an edge and return its record id.
    async fn relate(&self, from: &str, to: &str, rel_type: &str, confidence: f32) -> String {
        crud::add_relationship(
            &self.db,
            NewRelationship {
                from_entity: from.to_string(),
                to_entity: to.to_string(),
                rel_type: rel_type.to_string(),
                description: None,
                confidence: Some(confidence),
                source: Some("golden".to_string()),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("failed to relate {from} -> {to}: {e}"))
        .id_string()
    }

    /// Backdate an edge that was reinforced when it was created.
    async fn age_edge(&self, rel_id: &str, days: u32) {
        self.db
            .query(
                "UPDATE type::record($id) SET
                     valid_from = time::now() - type::duration($age),
                     last_reinforced = time::now() - type::duration($age)",
            )
            .bind(("id", rel_id.to_string()))
            .bind(("age", format!("{days}d")))
            .await
            .and_then(surrealdb::IndexedResults::check)
            .unwrap_or_else(|e| panic!("failed to age edge {rel_id}: {e}"));
    }

    /// Backdate an edge that was never reinforced — decay anchors on `valid_from`.
    async fn age_unreinforced_edge(&self, rel_id: &str, days: u32) {
        self.db
            .query(
                "UPDATE type::record($id) SET
                     valid_from = time::now() - type::duration($age),
                     last_reinforced = NONE",
            )
            .bind(("id", rel_id.to_string()))
            .bind(("age", format!("{days}d")))
            .await
            .and_then(surrealdb::IndexedResults::check)
            .unwrap_or_else(|e| panic!("failed to age edge {rel_id}: {e}"));
    }

    async fn set_utility(&self, name: &str, utility: f64) {
        self.db
            .query("UPDATE entity SET utility_score = $utility WHERE name = $name")
            .bind(("name", name.to_string()))
            .bind(("utility", utility))
            .await
            .and_then(surrealdb::IndexedResults::check)
            .unwrap_or_else(|e| panic!("failed to set utility for {name}: {e}"));
    }

    /// Run the hybrid query with default scoring weights.
    async fn query(&self, text: &str, options: &QueryOptions) -> QueryResult {
        self.query_scored(text, options, &GraphScoringConfig::default())
            .await
    }

    async fn query_scored(
        &self,
        text: &str,
        options: &QueryOptions,
        scoring: &GraphScoringConfig,
    ) -> QueryResult {
        query::query(&self.db, embedder(), scoring, text, options)
            .await
            .unwrap_or_else(|e| panic!("query {text:?} failed: {e}"))
    }
}

// ── Assertion helpers ────────────────────────────────────────────────

fn names(results: &[ScoredEntity]) -> Vec<&str> {
    results.iter().map(|r| r.entity.name.as_str()).collect()
}

fn name_set(results: &[ScoredEntity]) -> BTreeSet<&str> {
    results.iter().map(|r| r.entity.name.as_str()).collect()
}

fn find<'a>(results: &'a [ScoredEntity], name: &str) -> Option<&'a ScoredEntity> {
    results.iter().find(|r| r.entity.name == name)
}

/// Rank of `name`, or [`usize::MAX`] when it did not survive truncation.
/// Lets rank comparisons stay meaningful when one side is ranked off the list.
fn rank(results: &[ScoredEntity], name: &str) -> usize {
    results
        .iter()
        .position(|r| r.entity.name == name)
        .unwrap_or(usize::MAX)
}

fn graph_sourced(results: &[ScoredEntity]) -> BTreeSet<&str> {
    results
        .iter()
        .filter(|r| matches!(r.source, MatchSource::Graph { .. }))
        .map(|r| r.entity.name.as_str())
        .collect()
}

fn assert_close(actual: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (actual - expected).abs() < tolerance,
        "{what}: expected {expected} ± {tolerance}, got {actual}"
    );
}

fn semantic_only() -> GraphScoringConfig {
    GraphScoringConfig {
        weight_semantic: 1.0,
        weight_hotness: 0.0,
        weight_utility: 0.0,
    }
}

fn utility_only() -> GraphScoringConfig {
    GraphScoringConfig {
        weight_semantic: 0.0,
        weight_hotness: 0.0,
        weight_utility: 1.0,
    }
}

/// A query anchored on exactly one semantic hit.
///
/// The keyword `"borrow checker"` matches a single entity's name/abstract, so
/// the semantic phase yields one candidate and **every other result must have
/// arrived through 1-hop graph expansion** — the keyword filter is applied in
/// the semantic phase only, never to expanded neighbors. With no truncation
/// pressure, the expansion ordering is visible in full.
fn borrow_checker_anchor() -> QueryOptions {
    QueryOptions {
        limit: 5,
        graph_depth: 1,
        keyword: Some("borrow checker".to_string()),
        ..Default::default()
    }
}

// ── Golden set ───────────────────────────────────────────────────────

/// recall@k: a query whose relevant set is unambiguous by construction (the
/// three baking entities) must return exactly that set as its top 3, ahead of
/// every systems-programming, sailing and finance entity in the graph.
#[tokio::test]
async fn golden_query_recall_at_k_ordering() {
    let graph = GoldenGraph::seed().await;
    let options = QueryOptions {
        limit: 5,
        graph_depth: 1,
        ..Default::default()
    };

    let result = graph.query(BAKING_QUERY, &options).await;
    let ranked = names(&result.entities);

    assert_eq!(ranked.len(), 5, "limit honored: {ranked:?}");
    let top3: BTreeSet<&str> = ranked[..3].iter().copied().collect();
    let expected: BTreeSet<&str> = [SOURDOUGH_STARTER, BULK_FERMENTATION, DUTCH_OVEN]
        .into_iter()
        .collect();
    assert_eq!(top3, expected, "recall@3 for {BAKING_QUERY:?}: {ranked:?}");
}

/// Confidence weighting: neighbors reached over a fresh high-confidence edge
/// outrank neighbors reached over an old one, and the graph score is exactly
/// `parent_score * effective_confidence` — decay included.
#[tokio::test]
async fn golden_query_confidence_weighting_orders_neighbors() {
    let graph = GoldenGraph::seed().await;
    let result = graph.query(RUST_QUERY, &borrow_checker_anchor()).await;
    let ranked = names(&result.entities);

    let parent = find(&result.entities, BORROW_CHECKER)
        .unwrap_or_else(|| panic!("anchor missing: {ranked:?}"))
        .score;
    let fresh = find(&result.entities, INES)
        .unwrap_or_else(|| panic!("fresh-edge neighbor missing: {ranked:?}"))
        .score;
    let decayed = find(&result.entities, LUCIA)
        .unwrap_or_else(|| panic!("decayed-edge neighbor missing: {ranked:?}"))
        .score;

    // 0.95, reinforced at creation — no decay yet.
    assert_close(fresh / parent, 0.95, 1e-6, "fresh edge multiplier");
    // 0.90 stored, never reinforced, 180 days old: 0.90 x 0.5^(180/90).
    assert_close(decayed / parent, 0.225, 1e-3, "decayed edge multiplier");

    assert!(
        rank(&result.entities, INES) < rank(&result.entities, LUCIA),
        "fresh high-confidence neighbor must outrank the decayed one: {ranked:?}"
    );
}

/// Confidence weighting: an edge whose effective confidence has decayed below
/// `0.1` contributes nothing — its target is not reachable by expansion at all.
#[tokio::test]
async fn golden_query_confidence_weighting_drops_dead_edges() {
    let graph = GoldenGraph::seed().await;
    let result = graph.query(RUST_QUERY, &borrow_checker_anchor()).await;
    let ranked = names(&result.entities);

    assert!(
        find(&result.entities, INES).is_some(),
        "expansion did not run — the rest of this test would pass vacuously: {ranked:?}"
    );
    // 0.30 stored, 730 days old: decays to the 0.05 floor, below the 0.1 cutoff.
    assert!(
        find(&result.entities, OTTO).is_none(),
        "target of a sub-0.1 effective-confidence edge must be dropped: {ranked:?}"
    );
}

/// Scoring weights are live: the same graph and the same query produce a rank
/// flip when the weight vector moves from similarity-only to utility-only.
#[tokio::test]
async fn golden_query_scoring_weights_flip_rank() {
    let graph = GoldenGraph::seed().await;
    graph.set_utility(KALMAN_FILTER, 0.0).await;
    graph.set_utility(MUNICIPAL_BONDS, 1.0).await;

    let options = QueryOptions {
        limit: 10,
        graph_depth: 0,
        ..Default::default()
    };

    let by_similarity = graph
        .query_scored(SENSOR_QUERY, &options, &semantic_only())
        .await
        .entities;
    assert_eq!(
        names(&by_similarity).first().copied(),
        Some(KALMAN_FILTER),
        "similarity-only must rank the on-topic entity first: {:?}",
        names(&by_similarity)
    );
    assert!(
        rank(&by_similarity, KALMAN_FILTER) < rank(&by_similarity, MUNICIPAL_BONDS),
        "similarity-only ordering: {:?}",
        names(&by_similarity)
    );

    let by_utility = graph
        .query_scored(SENSOR_QUERY, &options, &utility_only())
        .await
        .entities;
    assert_eq!(
        names(&by_utility).first().copied(),
        Some(MUNICIPAL_BONDS),
        "utility-only must rank the high-utility entity first: {:?}",
        names(&by_utility)
    );
    assert!(
        rank(&by_utility, MUNICIPAL_BONDS) < rank(&by_utility, KALMAN_FILTER),
        "utility-only ordering: {:?}",
        names(&by_utility)
    );
}

/// Expansion reaches every 1-hop neighbor of a semantic hit — in both edge
/// directions — and stops there: no 2-hop entity and no unconnected entity is
/// pulled in.
#[tokio::test]
async fn golden_query_expansion_pulls_one_hop_neighbors() {
    let graph = GoldenGraph::seed().await;
    let result = graph.query(RUST_QUERY, &borrow_checker_anchor()).await;
    let ranked = names(&result.entities);

    let expected: BTreeSet<&str> = [OWNERSHIP_MODEL, INES, LUCIA].into_iter().collect();
    assert_eq!(
        graph_sourced(&result.entities),
        expected,
        "1-hop neighborhood of {BORROW_CHECKER}: {ranked:?}"
    );

    // OWNERSHIP_MODEL is reached over an incoming edge — traversal is undirected.
    let incoming = find(&result.entities, OWNERSHIP_MODEL).expect("incoming neighbor missing");
    assert!(
        matches!(&incoming.source, MatchSource::Graph { parent, rel_type }
            if parent == BORROW_CHECKER && rel_type == "RELATED_TO"),
        "incoming edge provenance: {:?}",
        incoming.source
    );

    assert!(
        find(&result.entities, ESTUDIO).is_none(),
        "2-hop entity must not be expanded into: {ranked:?}"
    );
    assert!(
        find(&result.entities, PRIYA).is_none(),
        "unconnected entity must not appear: {ranked:?}"
    );
}

/// `graph_depth = 0` turns the hybrid query into pure semantic search.
#[tokio::test]
async fn golden_query_graph_depth_zero_disables_expansion() {
    let graph = GoldenGraph::seed().await;
    let options = QueryOptions {
        graph_depth: 0,
        ..borrow_checker_anchor()
    };

    let result = graph.query(RUST_QUERY, &options).await;

    assert_eq!(names(&result.entities), vec![BORROW_CHECKER]);
    assert!(
        graph_sourced(&result.entities).is_empty(),
        "no expansion at depth 0"
    );
}

/// `graph_depth` is currently a switch, not a depth: any value above zero
/// expands exactly one hop. Pinned so a real multi-hop implementation has to
/// change this test deliberately.
#[tokio::test]
async fn golden_query_graph_depth_above_one_is_still_one_hop() {
    let graph = GoldenGraph::seed().await;
    let one_hop = graph.query(RUST_QUERY, &borrow_checker_anchor()).await;
    let deep = graph
        .query(
            RUST_QUERY,
            &QueryOptions {
                graph_depth: 5,
                ..borrow_checker_anchor()
            },
        )
        .await;

    assert_eq!(
        name_set(&deep.entities),
        name_set(&one_hop.entities),
        "depth 5 returned a different set than depth 1"
    );
    assert!(
        find(&deep.entities, ESTUDIO).is_none(),
        "depth 5 must still not reach the 2-hop entity: {:?}",
        names(&deep.entities)
    );
}

/// The entity-type filter is applied to graph-expanded neighbors too, not only
/// to semantic hits.
#[tokio::test]
async fn golden_query_entity_type_filter_applies_to_expanded_neighbors() {
    let graph = GoldenGraph::seed().await;
    let options = QueryOptions {
        entity_type: Some("concept".to_string()),
        ..borrow_checker_anchor()
    };

    let result = graph.query(RUST_QUERY, &options).await;
    let ranked = names(&result.entities);

    assert!(
        result
            .entities
            .iter()
            .all(|r| r.entity.entity_type == EntityType::Concept),
        "type filter leaked: {ranked:?}"
    );
    assert_eq!(
        name_set(&result.entities),
        [BORROW_CHECKER, OWNERSHIP_MODEL].into_iter().collect(),
        "concept-typed 1-hop neighborhood: {ranked:?}"
    );
}

/// Episode search is opt-in and ranks by its own semantic similarity.
#[tokio::test]
async fn golden_query_episodes_are_opt_in() {
    let graph = GoldenGraph::seed().await;
    let options = QueryOptions {
        limit: 5,
        ..Default::default()
    };

    let without = graph.query(BAKING_QUERY, &options).await;
    assert!(
        without.episodes.is_empty(),
        "episodes must not be searched unless requested"
    );

    let with = graph
        .query(
            BAKING_QUERY,
            &QueryOptions {
                include_episodes: true,
                ..options
            },
        )
        .await;
    let sessions: Vec<&str> = with
        .episodes
        .iter()
        .map(|e| e.episode.session_id.as_str())
        .collect();
    assert_eq!(sessions.first().copied(), Some("s-bake"), "{sessions:?}");
}

/// `limit = 0` is not "no results" — it means the documented default of 10.
#[tokio::test]
async fn golden_query_zero_limit_defaults_to_ten() {
    let graph = GoldenGraph::seed().await;
    let options = QueryOptions {
        limit: 0,
        ..Default::default()
    };

    let result = graph.query(RUST_QUERY, &options).await;

    assert_eq!(result.entities.len(), 10, "{:?}", names(&result.entities));
}
