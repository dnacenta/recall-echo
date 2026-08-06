// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Human correction — telling memory that something it learned is wrong.
//!
//! # Why this is evidence, not a delete
//!
//! Everything else in the graph moves confidence by *observation*: an edge is
//! corroborated or contradicted, the Beta counts move, the posterior mean
//! follows. A human saying "that's wrong" is the highest-authority observation
//! the system can receive, so it enters the same way — as contradicting
//! evidence at [`Provenance::User`] weight — rather than as a silent write of
//! a lower number or a quiet delete.
//!
//! That buys three things a delete cannot. The correction accumulates (saying
//! it twice is twice the evidence). It stays visible (`graph decay-report`
//! shows the counts that moved). And it degrades gracefully: an edge that was
//! believed for good reason survives one contradiction with reduced
//! confidence, which is the correct posture when a human and thirty
//! observations disagree.
//!
//! # What `--wrong` on an *entity* means
//!
//! An entity has no truth value. "Rust" is not true or false; what can be
//! wrong is a claim *about* Rust, and claims live on edges. So contradicting
//! an entity means contradicting the claims it takes part in.
//!
//! Which claims is exactly the ambiguity, so this module refuses to guess: one
//! live edge is unambiguous and is contradicted, several are reported back for
//! the human to choose between, and contradicting all of them is available but
//! must be asked for. Nothing is damaged on a guess.
//!
//! # Removal
//!
//! [`Correction::Forget`] is the escape hatch for memory that should not exist
//! at all rather than be believed less. It is destructive, so the plan and the
//! act are two separate requests: an unconfirmed forget only reports what would
//! go, and nothing is removed until a caller sends `confirmed`.

use serde::{Deserialize, Serialize};
use surrealdb::Surreal;

use super::confidence::{Provenance, ProvenanceWeights};
use super::edge_view::{self, EdgeView, NameCache};
use super::error::GraphError;
use super::store::Db;
use super::types::Relationship;

/// Near-misses shown when a name does not resolve.
const MAX_CANDIDATES: usize = 5;

/// Below this similarity a name is not a near-miss, it is a different name.
const CANDIDATE_FLOOR: f64 = 0.34;

/// What a correction is aimed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CorrectTarget {
    /// Everything the graph claims about one entity.
    Entity { name: String },
    /// One specific claim.
    Edge {
        from: String,
        rel_type: String,
        to: String,
    },
}

/// What to do to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Correction {
    /// Record contradicting evidence at user authority.
    Wrong {
        /// Contradict every live edge of an entity instead of refusing to
        /// choose between them.
        #[serde(default)]
        all_edges: bool,
    },
    /// Remove outright. Nothing is deleted unless `confirmed`.
    Forget {
        #[serde(default)]
        confirmed: bool,
    },
}

/// An entity, named well enough for a person to recognise it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityMatch {
    pub id: String,
    pub name: String,
    pub entity_type: String,
}

/// One edge after a contradiction, next to where it stood before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeCorrection {
    /// The edge as it now stands.
    pub edge: EdgeView,
    /// Posterior mean before the contradiction was recorded.
    pub confidence_before: f64,
    /// Evidence weight before the contradiction was recorded.
    pub evidence_before: f64,
}

/// What a forget would take, or did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Removal {
    /// The entity going away. Absent when only an edge was targeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<EntityMatch>,
    /// Every relationship going away with it, superseded ones included.
    pub edges: Vec<EdgeView>,
}

/// The result of one correction request.
///
/// Every outcome that changed nothing says why, and says it with enough detail
/// for the caller to make a better request — because the alternative to
/// refusing is damaging the wrong memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CorrectionReport {
    /// The name matched no entity. `candidates` are the closest stored names.
    UnknownEntity {
        query: String,
        candidates: Vec<EntityMatch>,
    },
    /// Both entities exist but no live edge of that type joins them.
    /// `existing` is what does join them.
    NoSuchEdge {
        from: String,
        to: String,
        rel_type: String,
        existing: Vec<EdgeView>,
    },
    /// The entity takes part in several claims; which one is wrong is the
    /// caller's to say.
    Ambiguous {
        entity: String,
        edges: Vec<EdgeView>,
    },
    /// The entity exists and is not connected to anything, so there is no
    /// claim to contradict.
    NothingToCorrect { entity: String },
    /// Contradicting evidence was recorded.
    Contradicted { edges: Vec<EdgeCorrection> },
    /// What an unconfirmed forget would remove. Nothing was removed.
    Planned { removal: Removal },
    /// What a confirmed forget removed.
    Removed { removal: Removal },
}

impl CorrectionReport {
    /// Whether the store was changed.
    #[must_use]
    pub fn applied(&self) -> bool {
        matches!(
            self,
            Self::Contradicted { .. } | Self::Removed { .. } | Self::Planned { .. }
        )
    }
}

/// Apply a correction, or report why it was refused.
pub async fn correct(
    db: &Surreal<Db>,
    weights: &ProvenanceWeights,
    target: &CorrectTarget,
    correction: Correction,
) -> Result<CorrectionReport, GraphError> {
    match target {
        CorrectTarget::Entity { name } => correct_entity(db, weights, name, correction).await,
        CorrectTarget::Edge { from, rel_type, to } => {
            correct_edge(db, weights, from, rel_type, to, correction).await
        }
    }
}

// ── Entity target ────────────────────────────────────────────────────────

async fn correct_entity(
    db: &Surreal<Db>,
    weights: &ProvenanceWeights,
    name: &str,
    correction: Correction,
) -> Result<CorrectionReport, GraphError> {
    let entity = match resolve(db, name).await? {
        Resolution::Found(entity) => entity,
        Resolution::Unresolved(report) => return Ok(report),
    };

    match correction {
        Correction::Wrong { all_edges } => contradict_entity(db, weights, &entity, all_edges).await,
        Correction::Forget { confirmed } => forget_entity(db, &entity, confirmed).await,
    }
}

/// Contradict what the graph claims about one entity.
///
/// One live edge is what the human meant. Several are not, and guessing which
/// would damage a memory the human never mentioned — so the choice goes back
/// to them, unless they asked for all of it.
async fn contradict_entity(
    db: &Surreal<Db>,
    weights: &ProvenanceWeights,
    entity: &EntityMatch,
    all_edges: bool,
) -> Result<CorrectionReport, GraphError> {
    let edges = edge_view::live_edges_of(db, &entity.id).await?;

    if edges.is_empty() {
        return Ok(CorrectionReport::NothingToCorrect {
            entity: entity.name.clone(),
        });
    }
    if edges.len() > 1 && !all_edges {
        let mut cache = NameCache::new();
        return Ok(CorrectionReport::Ambiguous {
            entity: entity.name.clone(),
            edges: edge_view::views(db, &mut cache, &edges).await?,
        });
    }

    contradict(db, weights, &edges).await
}

async fn forget_entity(
    db: &Surreal<Db>,
    entity: &EntityMatch,
    confirmed: bool,
) -> Result<CorrectionReport, GraphError> {
    let edges = edge_view::all_edges_of(db, &entity.id).await?;
    let mut cache = NameCache::new();
    let removal = Removal {
        entity: Some(entity.clone()),
        edges: edge_view::views(db, &mut cache, &edges).await?,
    };

    if !confirmed {
        return Ok(CorrectionReport::Planned { removal });
    }
    super::crud::delete_entity(db, &entity.id).await?;
    Ok(CorrectionReport::Removed { removal })
}

// ── Edge target ──────────────────────────────────────────────────────────

async fn correct_edge(
    db: &Surreal<Db>,
    weights: &ProvenanceWeights,
    from: &str,
    rel_type: &str,
    to: &str,
    correction: Correction,
) -> Result<CorrectionReport, GraphError> {
    let source = match resolve(db, from).await? {
        Resolution::Found(entity) => entity,
        Resolution::Unresolved(report) => return Ok(report),
    };
    let target = match resolve(db, to).await? {
        Resolution::Found(entity) => entity,
        Resolution::Unresolved(report) => return Ok(report),
    };

    let between = edge_view::live_edges_between(db, &source.id, &target.id).await?;
    let matched: Vec<Relationship> = between
        .iter()
        .filter(|edge| edge.rel_type.eq_ignore_ascii_case(rel_type))
        .cloned()
        .collect();

    if matched.is_empty() {
        let mut cache = NameCache::new();
        return Ok(CorrectionReport::NoSuchEdge {
            from: source.name,
            to: target.name,
            rel_type: rel_type.to_string(),
            existing: edge_view::views(db, &mut cache, &between).await?,
        });
    }

    match correction {
        Correction::Wrong { .. } => contradict(db, weights, &matched).await,
        Correction::Forget { confirmed } => forget_edges(db, &matched, confirmed).await,
    }
}

async fn forget_edges(
    db: &Surreal<Db>,
    edges: &[Relationship],
    confirmed: bool,
) -> Result<CorrectionReport, GraphError> {
    let mut cache = NameCache::new();
    let removal = Removal {
        entity: None,
        edges: edge_view::views(db, &mut cache, edges).await?,
    };

    if !confirmed {
        return Ok(CorrectionReport::Planned { removal });
    }
    for edge in edges {
        super::crud::delete_relationship(db, &edge.id_string()).await?;
    }
    Ok(CorrectionReport::Removed { removal })
}

// ── Contradiction ────────────────────────────────────────────────────────

/// Record one contradicting observation, at user authority, on every edge.
async fn contradict(
    db: &Surreal<Db>,
    weights: &ProvenanceWeights,
    edges: &[Relationship],
) -> Result<CorrectionReport, GraphError> {
    let mut cache = NameCache::new();
    let mut corrections = Vec::with_capacity(edges.len());

    for edge in edges {
        let mut evidence = edge.edge_evidence();
        evidence.contradict(Provenance::User, weights);
        super::crud::contradict_relationship(db, &edge.id_string(), evidence).await?;

        let counts = evidence.evidence();
        let from = cache
            .name_of(db, &edge_view::record_id(&edge.from_id))
            .await?;
        let to = cache
            .name_of(db, &edge_view::record_id(&edge.to_id))
            .await?;
        let mut view = EdgeView::of(edge, from, to);
        let confidence_before = view.confidence;
        let evidence_before = view.evidence;
        view.confidence = counts.mean();
        view.evidence = counts.concentration();

        corrections.push(EdgeCorrection {
            edge: view,
            confidence_before,
            evidence_before,
        });
    }

    Ok(CorrectionReport::Contradicted { edges: corrections })
}

// ── Name resolution ──────────────────────────────────────────────────────

/// Either the one entity a name means, or the report saying why it means no
/// single entity.
enum Resolution {
    Found(EntityMatch),
    Unresolved(CorrectionReport),
}

/// Resolve a typed name to exactly one stored entity.
///
/// Only an exact name — or a unique case-insensitive one — resolves. Anything
/// else comes back as candidates for the human to choose from: a fuzzy match
/// picked by the machine is a correction applied to a memory nobody named.
async fn resolve(db: &Surreal<Db>, name: &str) -> Result<Resolution, GraphError> {
    let index = entity_index(db).await?;
    let query = name.trim();

    if let Some(entity) = index.iter().find(|entity| entity.name == query) {
        return Ok(Resolution::Found(entity.clone()));
    }

    let folded: Vec<&EntityMatch> = index
        .iter()
        .filter(|entity| entity.name.eq_ignore_ascii_case(query))
        .collect();
    if folded.len() == 1 {
        return Ok(Resolution::Found(folded[0].clone()));
    }
    if folded.len() > 1 {
        return Ok(Resolution::Unresolved(CorrectionReport::UnknownEntity {
            query: query.to_string(),
            candidates: folded.into_iter().cloned().collect(),
        }));
    }

    Ok(Resolution::Unresolved(CorrectionReport::UnknownEntity {
        query: query.to_string(),
        candidates: nearest(query, &index),
    }))
}

/// Every entity's identity, without the 384 floats attached to it.
async fn entity_index(db: &Surreal<Db>) -> Result<Vec<EntityMatch>, GraphError> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: serde_json::Value,
        name: String,
        entity_type: String,
    }

    let mut response = db
        .query("SELECT id, name, entity_type FROM entity ORDER BY name")
        .await?;
    let rows: Vec<Row> = super::deserialize_take(&mut response, 0)?;
    Ok(rows
        .into_iter()
        .map(|row| EntityMatch {
            id: edge_view::record_id(&row.id),
            name: row.name,
            entity_type: row.entity_type,
        })
        .collect())
}

/// The stored names closest to what was typed, best first.
fn nearest(query: &str, index: &[EntityMatch]) -> Vec<EntityMatch> {
    let query = query.to_lowercase();
    let mut scored: Vec<(f64, &EntityMatch)> = index
        .iter()
        .map(|entity| (similarity(&query, &entity.name.to_lowercase()), entity))
        .filter(|(score, _)| *score >= CANDIDATE_FLOOR)
        .collect();

    scored.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.name.cmp(&right.1.name))
    });
    scored
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, entity)| entity.clone())
        .collect()
}

/// How alike two lowercased names are, in `[0, 1]`.
///
/// Sørensen–Dice over character bigrams, which catches typos and word-order
/// changes without a dependency. Containment scores high on its own: "recall"
/// should surface "recall-echo" however few bigrams they share.
fn similarity(query: &str, name: &str) -> f64 {
    if query.is_empty() || name.is_empty() {
        return 0.0;
    }
    if query == name {
        return 1.0;
    }

    let dice = dice_coefficient(query, name);
    if name.contains(query) || query.contains(name) {
        return dice.max(0.9);
    }
    dice
}

fn dice_coefficient(left: &str, right: &str) -> f64 {
    let left: Vec<[char; 2]> = bigrams(left);
    let right: Vec<[char; 2]> = bigrams(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    let mut remaining = right.clone();
    let mut shared = 0usize;
    for bigram in &left {
        if let Some(position) = remaining.iter().position(|other| other == bigram) {
            remaining.swap_remove(position);
            shared += 1;
        }
    }
    (2.0 * shared as f64) / (left.len() + right.len()) as f64
}

fn bigrams(text: &str) -> Vec<[char; 2]> {
    let chars: Vec<char> = text.chars().collect();
    chars.windows(2).map(|pair| [pair[0], pair[1]]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Vec<EntityMatch> {
        ["recall-echo", "pulse-null", "Rust", "rust", "Cargo"]
            .into_iter()
            .map(|name| EntityMatch {
                id: format!("entity:{}", name.to_lowercase()),
                name: name.to_string(),
                entity_type: "tool".to_string(),
            })
            .collect()
    }

    #[test]
    fn a_name_is_most_like_itself() {
        assert_eq!(similarity("rust", "rust"), 1.0);
        assert!(similarity("rust", "cargo") < CANDIDATE_FLOOR);
    }

    #[test]
    fn a_typo_stays_a_near_miss() {
        assert!(
            similarity("recall-eco", "recall-echo") > CANDIDATE_FLOOR,
            "{}",
            similarity("recall-eco", "recall-echo")
        );
    }

    #[test]
    fn a_prefix_surfaces_the_whole_name() {
        assert!(similarity("recall", "recall-echo") >= 0.9);
    }

    #[test]
    fn candidates_are_the_closest_names_and_no_others() {
        let candidates = nearest("recall", &index());
        assert_eq!(
            candidates.first().map(|entity| entity.name.as_str()),
            Some("recall-echo")
        );
        assert!(
            candidates.len() <= MAX_CANDIDATES,
            "a wall of names is not a choice"
        );
        assert!(
            !candidates.iter().any(|entity| entity.name == "Cargo"),
            "unrelated names are not near-misses: {candidates:?}"
        );
    }

    #[test]
    fn nothing_is_close_to_nonsense() {
        assert!(nearest("zzzzqqqq", &index()).is_empty());
    }

    #[test]
    fn a_report_that_changed_nothing_says_so() {
        assert!(!CorrectionReport::NothingToCorrect {
            entity: "Rust".into()
        }
        .applied());
        assert!(!CorrectionReport::UnknownEntity {
            query: "Rust".into(),
            candidates: Vec::new(),
        }
        .applied());
        assert!(CorrectionReport::Contradicted { edges: Vec::new() }.applied());
    }

    #[test]
    fn corrections_survive_the_daemon_wire_format() {
        let target = CorrectTarget::Edge {
            from: "D".into(),
            rel_type: "USES".into(),
            to: "Vim".into(),
        };
        let line = serde_json::to_string(&target).unwrap();
        assert_eq!(
            serde_json::from_str::<CorrectTarget>(&line).unwrap(),
            target
        );

        // Optional modifiers may be omitted by an older client.
        assert_eq!(
            serde_json::from_str::<Correction>(r#"{"kind":"wrong"}"#).unwrap(),
            Correction::Wrong { all_edges: false }
        );
        assert_eq!(
            serde_json::from_str::<Correction>(r#"{"kind":"forget"}"#).unwrap(),
            Correction::Forget { confirmed: false }
        );
    }
}
