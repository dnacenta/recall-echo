// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bayesian confidence model for relationship edges.
//!
//! Uses a Beta-Binomial conjugate prior. The pseudo-counts (`alpha`, `beta`)
//! are **persisted on the edge**, so evidence accumulates: the posterior after
//! fifty corroborations is a different distribution from the posterior after
//! five, and its variance is smaller. The stored `confidence` field is the
//! posterior mean — a derived value kept in sync with the counts on every
//! write, so read paths can keep scoring on the mean alone.
//!
//! A new edge (or an edge from a store predating evidence persistence) starts
//! at [`PRIOR_CONCENTRATION`]: the mean is preserved and the concentration is
//! honestly low.

use serde::{Deserialize, Serialize};

/// Total pseudo-count of the Beta prior an edge starts from.
/// ~10 observations to overwhelm the prior.
pub const PRIOR_CONCENTRATION: f64 = 10.0;

/// Evidence weight of a single observation whose provenance is not modelled:
/// one observation, one count.
///
/// Now that observations carry a [`Provenance`], this is the provenance-blind
/// reference behavior — what [`ProvenanceWeights::uniform`] reproduces, and
/// what the differential test measures against.
pub const DEFAULT_EVIDENCE_WEIGHT: f64 = 1.0;

/// Default evidence weight of an observation from an independent source.
pub const DEFAULT_WEIGHT_EXTERNAL: f64 = 1.0;

/// Default evidence weight of an observation authored by the human.
pub const DEFAULT_WEIGHT_USER: f64 = 0.8;

/// Default evidence weight of the agent restating itself.
pub const DEFAULT_WEIGHT_SELF: f64 = 0.05;

/// How a relationship was established — determines initial confidence prior.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionContext {
    Explicit,      // 0.9
    Inferred,      // 0.6
    Speculative,   // 0.3
    Authoritative, // 1.0
}

impl ExtractionContext {
    /// Initial confidence prior for this extraction context.
    #[must_use]
    pub fn prior(self) -> f64 {
        match self {
            Self::Authoritative => 1.0,
            Self::Explicit => 0.9,
            Self::Inferred => 0.6,
            Self::Speculative => 0.3,
        }
    }
}

impl std::str::FromStr for ExtractionContext {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "explicit" => Ok(Self::Explicit),
            "inferred" => Ok(Self::Inferred),
            "speculative" => Ok(Self::Speculative),
            "authoritative" => Ok(Self::Authoritative),
            other => Err(format!("unknown extraction context: {other}")),
        }
    }
}

// ── Provenance ───────────────────────────────────────────────────────
//
// Who authored the text an observation came from. Recorded at write time
// because it cannot be recovered afterwards: nothing in a store of unlabelled
// episodes distinguishes an independent report from the agent restating
// itself. Collapsing three classes into two at scoring time is always
// possible; splitting one class back into three is not.

/// The authorship class of an episode, and of every confidence-moving
/// observation drawn from it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    /// Ingested documents, web content, tool output — sources independent of
    /// the agent.
    External,
    /// Statements authored by the human in conversation.
    User,
    /// The agent's own summaries, reflections and re-assertions — and the
    /// default for anything unlabelled, so unknown evidence never earns full
    /// weight.
    #[default]
    #[serde(rename = "self")]
    SelfGenerated,
}

impl Provenance {
    /// The class of a value read from the store, which may be absent (an
    /// episode written before provenance existed) or unrecognised (written by
    /// a newer build, or by hand).
    ///
    /// Both resolve to [`Provenance::SelfGenerated`]: a legacy store never
    /// gains confidence from backfilled data.
    #[must_use]
    pub fn from_stored(stored: Option<&str>) -> Self {
        stored
            .and_then(|s| s.parse().ok())
            .unwrap_or(Self::SelfGenerated)
    }

    /// The string persisted on an episode and accepted on the CLI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::User => "user",
            Self::SelfGenerated => "self",
        }
    }
}

impl std::str::FromStr for Provenance {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "external" | "document" => Ok(Self::External),
            "user" | "human" => Ok(Self::User),
            "self" | "agent" | "self-generated" => Ok(Self::SelfGenerated),
            other => Err(format!("unknown provenance: {other}")),
        }
    }
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Evidence weight of one observation, by provenance class.
///
/// Corroboration adds the weight to α, contradiction adds it to β. The
/// defaults say an independent source counts fully, the human counts nearly
/// fully, and the agent restating itself counts for almost nothing — which is
/// the point of the whole mechanism: repetition by a single source is
/// coherence, not evidence.
///
/// Maps to the `[graph.provenance]` section of `.recall-echo.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvenanceWeights {
    /// Weight of an [`Provenance::External`] observation. Default `1.0`.
    pub weight_external: f64,
    /// Weight of a [`Provenance::User`] observation. Default `0.8`.
    pub weight_user: f64,
    /// Weight of a [`Provenance::SelfGenerated`] observation. Default `0.05`.
    pub weight_self: f64,
}

impl Default for ProvenanceWeights {
    fn default() -> Self {
        Self {
            weight_external: DEFAULT_WEIGHT_EXTERNAL,
            weight_user: DEFAULT_WEIGHT_USER,
            weight_self: DEFAULT_WEIGHT_SELF,
        }
    }
}

impl ProvenanceWeights {
    /// Weight every class identically — the provenance-blind escape hatch.
    ///
    /// `uniform(DEFAULT_EVIDENCE_WEIGHT)` reproduces pre-provenance behavior
    /// exactly, which is what makes provenance weighting differentially
    /// testable.
    #[must_use]
    pub fn uniform(weight: f64) -> Self {
        Self {
            weight_external: weight,
            weight_user: weight,
            weight_self: weight,
        }
    }

    /// Evidence weight of one observation authored by `provenance`.
    #[must_use]
    pub fn for_provenance(&self, provenance: Provenance) -> f64 {
        match provenance {
            Provenance::External => self.weight_external,
            Provenance::User => self.weight_user,
            Provenance::SelfGenerated => self.weight_self,
        }
    }
}

/// The accumulated evidence for one relationship: the pseudo-counts of its
/// Beta posterior.
///
/// `alpha` counts corroboration, `beta` counts contradiction. Both are
/// weighted sums, not integers — an observation contributes its provenance
/// weight. Counts are non-negative by construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Evidence {
    alpha: f64,
    beta: f64,
}

impl Evidence {
    /// Evidence for an edge that has only a mean: split [`PRIOR_CONCENTRATION`]
    /// between the two counts so the mean is preserved exactly.
    ///
    /// This is the shape a brand-new edge starts in, and the shape the schema
    /// migration backfills legacy edges into.
    #[must_use]
    pub fn from_prior(mean: f64) -> Self {
        let mean = mean.clamp(0.0, 1.0);
        Self {
            alpha: mean * PRIOR_CONCENTRATION,
            beta: (1.0 - mean) * PRIOR_CONCENTRATION,
        }
    }

    /// Evidence from persisted counts. Non-finite or negative counts are
    /// clamped to zero — a corrupt count must not produce a nonsense mean.
    #[must_use]
    pub fn from_counts(alpha: f64, beta: f64) -> Self {
        Self {
            alpha: sanitize_count(alpha),
            beta: sanitize_count(beta),
        }
    }

    /// Evidence for an edge as read from the store.
    ///
    /// Uses the persisted counts when present; falls back to
    /// [`Evidence::from_prior`] over the stored mean when they are absent —
    /// the shape of an edge on a store whose migration has not run yet.
    #[must_use]
    pub fn from_stored(alpha: Option<f64>, beta: Option<f64>, confidence: f64) -> Self {
        match (alpha, beta) {
            (Some(a), Some(b)) => Self::from_counts(a, b),
            _ => Self::from_prior(confidence),
        }
    }

    /// Record supporting evidence of the given weight.
    pub fn corroborate(&mut self, weight: f64) {
        self.alpha += sanitize_weight(weight);
    }

    /// Record contradicting evidence of the given weight.
    pub fn contradict(&mut self, weight: f64) {
        self.beta += sanitize_weight(weight);
    }

    /// Corroboration pseudo-count.
    #[must_use]
    pub fn alpha(self) -> f64 {
        self.alpha
    }

    /// Contradiction pseudo-count.
    #[must_use]
    pub fn beta(self) -> f64 {
        self.beta
    }

    /// Total evidence weight behind this edge (`alpha + beta`).
    ///
    /// This is what never grew in the pre-Phase-1 model: it is the difference
    /// between "believed at 0.9" and "believed at 0.9 for good reason".
    #[must_use]
    pub fn concentration(self) -> f64 {
        self.alpha + self.beta
    }

    /// Posterior mean — the value stored as the edge's `confidence`.
    ///
    /// With no evidence in either direction the mean is 0.5 (maximal ignorance).
    #[must_use]
    pub fn mean(self) -> f64 {
        let total = self.concentration();
        if total <= 0.0 {
            return 0.5;
        }
        self.alpha / total
    }

    /// Posterior variance — `αβ / ((α+β)²(α+β+1))`.
    ///
    /// Strictly decreasing in the amount of evidence at a fixed mean, which is
    /// how "corroborated fifty times" is told apart from "corroborated once".
    #[must_use]
    pub fn variance(self) -> f64 {
        let total = self.concentration();
        if total <= 0.0 {
            return 0.0;
        }
        (self.alpha * self.beta) / (total * total * (total + 1.0))
    }
}

/// Clamp a persisted count into the non-negative reals.
fn sanitize_count(count: f64) -> f64 {
    if count.is_finite() && count > 0.0 {
        count
    } else {
        0.0
    }
}

/// Clamp an observation weight; non-positive or non-finite weights record
/// nothing rather than eroding accumulated evidence.
fn sanitize_weight(weight: f64) -> f64 {
    if weight.is_finite() && weight > 0.0 {
        weight
    } else {
        0.0
    }
}

/// Everything one edge persists about why it is believed: the Beta counts
/// that move confidence, and the coherence counter that must not.
///
/// A self-authored corroboration is weighted into α like any other
/// observation — at the (normally tiny) self weight — *and* tallied in
/// `self_reinforcements`. Keeping the tally separate is what lets "believed
/// because three independent sources said so" stay distinguishable from
/// "believed because the agent has said it thirty times".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeEvidence {
    evidence: Evidence,
    self_reinforcements: i64,
}

impl EdgeEvidence {
    /// Evidence state as read from an edge. A negative stored tally (only
    /// reachable by hand-editing the store) is treated as zero.
    #[must_use]
    pub fn new(evidence: Evidence, self_reinforcements: i64) -> Self {
        Self {
            evidence,
            self_reinforcements: self_reinforcements.max(0),
        }
    }

    /// Record corroboration authored by `provenance`.
    pub fn corroborate(&mut self, provenance: Provenance, weights: &ProvenanceWeights) {
        self.evidence
            .corroborate(weights.for_provenance(provenance));
        if provenance == Provenance::SelfGenerated {
            self.self_reinforcements += 1;
        }
    }

    /// Record contradiction authored by `provenance`.
    ///
    /// Contradicting yourself is not coherence: the tally does not move.
    pub fn contradict(&mut self, provenance: Provenance, weights: &ProvenanceWeights) {
        self.evidence.contradict(weights.for_provenance(provenance));
    }

    /// The Beta pseudo-counts.
    #[must_use]
    pub fn evidence(self) -> Evidence {
        self.evidence
    }

    /// How many corroborations the agent produced itself.
    #[must_use]
    pub fn self_reinforcements(self) -> i64 {
        self.self_reinforcements
    }
}

/// Default half-life for temporal decay (days).
/// At 90 days without reinforcement, effective confidence halves.
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 90.0;

/// Minimum effective confidence floor — decay never goes below this.
pub const DECAY_FLOOR: f64 = 0.05;

/// Compute effective confidence after temporal decay.
///
/// Formula: `effective = stored × 0.5^(days_since_reinforced / half_life)`
///
/// - `stored_confidence`: the Bayesian posterior (stored in DB)
/// - `days_since_reinforced`: days since `last_reinforced` (or `valid_from` if never reinforced)
/// - `half_life_days`: how many days until confidence halves (default: 90)
///
/// Returns at least `DECAY_FLOOR` (0.05) — relationships never fully disappear through decay alone.
#[must_use]
pub fn temporal_decay(
    stored_confidence: f64,
    days_since_reinforced: f64,
    half_life_days: f64,
) -> f64 {
    if days_since_reinforced <= 0.0 {
        return stored_confidence;
    }

    let decay_factor = 0.5_f64.powf(days_since_reinforced / half_life_days);
    let effective = stored_confidence * decay_factor;
    effective.max(DECAY_FLOOR)
}

/// Compute effective confidence for a relationship, using `last_reinforced` or `valid_from` as anchor.
///
/// This is the convenience wrapper that parses datetime values and calls `temporal_decay`.
pub fn effective_confidence(
    stored_confidence: f64,
    last_reinforced: Option<&serde_json::Value>,
    valid_from: &serde_json::Value,
    now: &chrono::DateTime<chrono::Utc>,
) -> f64 {
    let anchor = last_reinforced
        .and_then(parse_datetime_value)
        .or_else(|| parse_datetime_value(valid_from));

    match anchor {
        Some(dt) => {
            let days = (*now - dt).num_hours() as f64 / 24.0;
            temporal_decay(stored_confidence, days, DEFAULT_HALF_LIFE_DAYS)
        }
        None => stored_confidence, // Can't compute decay without a timestamp
    }
}

use super::util::parse_datetime as parse_datetime_value;

/// Compound confidence along a multi-hop path.
///
/// Returns the product of edge confidences. An empty path returns 1.0.
#[must_use]
pub fn path_confidence(edge_confidences: &[f64]) -> f64 {
    edge_confidences.iter().product()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.001
    }

    /// One weighted observation on evidence derived from a bare mean.
    fn one_observation(mean: f64, corroborate: bool) -> Evidence {
        let mut evidence = Evidence::from_prior(mean);
        if corroborate {
            evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
        } else {
            evidence.contradict(DEFAULT_EVIDENCE_WEIGHT);
        }
        evidence
    }

    #[test]
    fn corroborate_from_prior_0_6() {
        let result = one_observation(0.6, true).mean();
        // alpha=6, beta=4 -> (6+1)/(10+1) = 7/11 ≈ 0.636
        assert!(approx_eq(result, 0.636), "got {}", result);
    }

    #[test]
    fn contradict_from_prior_0_6() {
        let result = one_observation(0.6, false).mean();
        // alpha=6, beta=4 -> 6/(10+1) = 6/11 ≈ 0.545
        assert!(approx_eq(result, 0.545), "got {}", result);
    }

    #[test]
    fn corroborate_from_prior_0_9() {
        let result = one_observation(0.9, true).mean();
        // alpha=9, beta=1 -> (9+1)/(10+1) = 10/11 ≈ 0.909
        assert!(approx_eq(result, 0.909), "got {}", result);
    }

    #[test]
    fn contradict_from_prior_0_9() {
        let result = one_observation(0.9, false).mean();
        // alpha=9, beta=1 -> 9/(10+1) = 9/11 ≈ 0.818
        assert!(approx_eq(result, 0.818), "got {}", result);
    }

    #[test]
    fn corroborate_from_prior_0_3() {
        let result = one_observation(0.3, true).mean();
        // alpha=3, beta=7 -> (3+1)/(10+1) = 4/11 ≈ 0.364
        assert!(approx_eq(result, 0.364), "got {}", result);
    }

    #[test]
    fn evidence_accumulates_across_observations() {
        // docs/bayesian-confidence.md, worked example: an Inferred fact (0.6)
        // corroborated three times, then contradicted once.
        let mut evidence = Evidence::from_prior(0.6);
        assert!(approx_eq(evidence.mean(), 0.600));

        evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
        assert!(approx_eq(evidence.mean(), 0.636), "step 1: {evidence:?}");
        evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
        assert!(approx_eq(evidence.mean(), 0.667), "step 2: {evidence:?}");
        evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
        assert!(approx_eq(evidence.mean(), 0.692), "step 3: {evidence:?}");
        evidence.contradict(DEFAULT_EVIDENCE_WEIGHT);
        assert!(approx_eq(evidence.mean(), 0.643), "step 4: {evidence:?}");

        assert!(approx_eq(evidence.alpha(), 9.0));
        assert!(approx_eq(evidence.beta(), 5.0));
        assert!(approx_eq(evidence.concentration(), 14.0));
    }

    #[test]
    fn variance_narrows_with_corroboration() {
        // AC1: more evidence at a comparable mean is a tighter posterior.
        let after = |n: usize| {
            let mut evidence = Evidence::from_prior(0.6);
            for _ in 0..n {
                evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
            }
            evidence.variance()
        };

        assert!(
            after(5) < after(1),
            "5 obs: {} vs 1: {}",
            after(5),
            after(1)
        );
        assert!(
            after(50) < after(5),
            "50 obs: {} vs 5: {}",
            after(50),
            after(5)
        );
    }

    #[test]
    fn concentration_grows_by_observation_weight() {
        let mut evidence = Evidence::from_prior(0.5);
        assert!(approx_eq(evidence.concentration(), PRIOR_CONCENTRATION));

        evidence.corroborate(0.05);
        evidence.contradict(0.8);

        assert!(approx_eq(evidence.alpha(), 5.05), "got {evidence:?}");
        assert!(approx_eq(evidence.beta(), 5.8), "got {evidence:?}");
        assert!(approx_eq(
            evidence.concentration(),
            PRIOR_CONCENTRATION + 0.85
        ));
    }

    #[test]
    fn non_positive_weights_record_nothing() {
        let mut evidence = Evidence::from_prior(0.6);
        evidence.corroborate(-1.0);
        evidence.contradict(f64::NAN);

        assert!(approx_eq(evidence.alpha(), 6.0));
        assert!(approx_eq(evidence.beta(), 4.0));
    }

    #[test]
    fn from_stored_prefers_persisted_counts() {
        let persisted = Evidence::from_stored(Some(56.0), Some(4.0), 0.6);
        assert!(approx_eq(persisted.concentration(), 60.0));
        assert!(approx_eq(persisted.mean(), 56.0 / 60.0));
    }

    #[test]
    fn from_stored_falls_back_to_prior_when_unmigrated() {
        let legacy = Evidence::from_stored(None, None, 0.6);
        assert!(approx_eq(legacy.alpha(), 6.0));
        assert!(approx_eq(legacy.beta(), 4.0));
        assert!(approx_eq(legacy.mean(), 0.6));
    }

    #[test]
    fn empty_evidence_is_maximally_uncertain() {
        let empty = Evidence::from_counts(0.0, 0.0);
        assert!(approx_eq(empty.mean(), 0.5));
        assert_eq!(empty.variance(), 0.0);
    }

    #[test]
    fn corrupt_counts_are_clamped() {
        let corrupt = Evidence::from_counts(-3.0, f64::INFINITY);
        assert_eq!(corrupt.alpha(), 0.0);
        assert_eq!(corrupt.beta(), 0.0);
    }

    #[test]
    fn default_weights_rank_independence_above_repetition() {
        let weights = ProvenanceWeights::default();
        assert!(approx_eq(
            weights.for_provenance(Provenance::External),
            DEFAULT_WEIGHT_EXTERNAL
        ));
        assert!(approx_eq(
            weights.for_provenance(Provenance::User),
            DEFAULT_WEIGHT_USER
        ));
        assert!(approx_eq(
            weights.for_provenance(Provenance::SelfGenerated),
            DEFAULT_WEIGHT_SELF
        ));
        assert!(weights.weight_external > weights.weight_user);
        assert!(weights.weight_user > weights.weight_self);
    }

    #[test]
    fn uniform_weights_are_provenance_blind() {
        let weights = ProvenanceWeights::uniform(DEFAULT_EVIDENCE_WEIGHT);
        for provenance in [
            Provenance::External,
            Provenance::User,
            Provenance::SelfGenerated,
        ] {
            assert_eq!(
                weights.for_provenance(provenance),
                DEFAULT_EVIDENCE_WEIGHT,
                "{provenance} must weigh the same as every other class"
            );
        }
    }

    #[test]
    fn provenance_parses_and_renders() {
        assert_eq!("external".parse::<Provenance>(), Ok(Provenance::External));
        assert_eq!("User".parse::<Provenance>(), Ok(Provenance::User));
        assert_eq!(
            " SELF ".parse::<Provenance>(),
            Ok(Provenance::SelfGenerated)
        );
        assert!("mostly-true".parse::<Provenance>().is_err());

        assert_eq!(Provenance::External.to_string(), "external");
        assert_eq!(Provenance::User.to_string(), "user");
        assert_eq!(Provenance::SelfGenerated.to_string(), "self");
    }

    #[test]
    fn stored_provenance_defaults_to_self() {
        // AC7: absent and unrecognised both land on the conservative class.
        assert_eq!(Provenance::from_stored(None), Provenance::SelfGenerated);
        assert_eq!(
            Provenance::from_stored(Some("nonsense")),
            Provenance::SelfGenerated
        );
        assert_eq!(
            Provenance::from_stored(Some("external")),
            Provenance::External
        );
    }

    #[test]
    fn provenance_serde_uses_wire_names() {
        for (provenance, wire) in [
            (Provenance::External, "\"external\""),
            (Provenance::User, "\"user\""),
            (Provenance::SelfGenerated, "\"self\""),
        ] {
            assert_eq!(serde_json::to_string(&provenance).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<Provenance>(wire).unwrap(),
                provenance
            );
        }
    }

    #[test]
    fn self_corroboration_is_counted_separately_from_confidence() {
        let weights = ProvenanceWeights::default();
        let mut edge = EdgeEvidence::new(Evidence::from_prior(0.6), 0);

        for _ in 0..3 {
            edge.corroborate(Provenance::SelfGenerated, &weights);
        }
        edge.corroborate(Provenance::External, &weights);
        edge.contradict(Provenance::SelfGenerated, &weights);

        assert_eq!(
            edge.self_reinforcements(),
            3,
            "only self-corroboration is coherence"
        );
        assert!(approx_eq(edge.evidence().alpha(), 6.0 + 0.15 + 1.0));
        assert!(approx_eq(edge.evidence().beta(), 4.0 + 0.05));
    }

    #[test]
    fn external_contradiction_outweighs_accumulated_self_corroboration() {
        // AC2: twenty self-corroborations are erased by a single independent
        // contradiction at the default weights.
        let weights = ProvenanceWeights::default();
        let mut edge = EdgeEvidence::new(Evidence::from_prior(0.6), 0);
        let before = edge.evidence().mean();

        for _ in 0..20 {
            edge.corroborate(Provenance::SelfGenerated, &weights);
        }
        let after_coherence = edge.evidence().mean();
        assert!(after_coherence > before);
        assert_eq!(edge.self_reinforcements(), 20);

        edge.contradict(Provenance::External, &weights);
        assert!(
            edge.evidence().mean() < before,
            "one external contradiction must undo the whole coherence run: {} vs {before}",
            edge.evidence().mean()
        );
    }

    #[test]
    fn negative_stored_tally_is_clamped() {
        let edge = EdgeEvidence::new(Evidence::from_prior(0.5), -7);
        assert_eq!(edge.self_reinforcements(), 0);
    }

    #[test]
    fn path_confidence_two_edges() {
        let result = path_confidence(&[0.8, 0.7]);
        assert!(approx_eq(result, 0.56), "got {}", result);
    }

    #[test]
    fn path_confidence_empty() {
        assert_eq!(path_confidence(&[]), 1.0);
    }

    #[test]
    fn extraction_context_priors() {
        assert_eq!(ExtractionContext::Authoritative.prior(), 1.0);
        assert_eq!(ExtractionContext::Explicit.prior(), 0.9);
        assert_eq!(ExtractionContext::Inferred.prior(), 0.6);
        assert_eq!(ExtractionContext::Speculative.prior(), 0.3);
    }

    #[test]
    fn temporal_decay_zero_days() {
        let result = temporal_decay(0.9, 0.0, 90.0);
        assert!(approx_eq(result, 0.9), "got {}", result);
    }

    #[test]
    fn temporal_decay_one_half_life() {
        // After exactly 90 days, confidence should halve
        let result = temporal_decay(0.6, 90.0, 90.0);
        assert!(approx_eq(result, 0.3), "got {}", result);
    }

    #[test]
    fn temporal_decay_two_half_lives() {
        // After 180 days, confidence should quarter
        let result = temporal_decay(0.8, 180.0, 90.0);
        assert!(approx_eq(result, 0.2), "got {}", result);
    }

    #[test]
    fn temporal_decay_floor() {
        // After many half-lives, should hit the floor
        let result = temporal_decay(0.3, 900.0, 90.0);
        assert!(approx_eq(result, DECAY_FLOOR), "got {}", result);
    }

    #[test]
    fn temporal_decay_negative_days() {
        // Negative days (future timestamp) should return stored confidence
        let result = temporal_decay(0.7, -5.0, 90.0);
        assert!(approx_eq(result, 0.7), "got {}", result);
    }

    #[test]
    fn temporal_decay_high_confidence_still_decays() {
        // Even 1.0 confidence decays
        let result = temporal_decay(1.0, 90.0, 90.0);
        assert!(approx_eq(result, 0.5), "got {}", result);
    }

    #[test]
    fn effective_confidence_with_last_reinforced() {
        let now = chrono::Utc::now();
        let ninety_days_ago = (now - chrono::Duration::days(90)).to_rfc3339();
        let valid_from_long_ago = (now - chrono::Duration::days(365)).to_rfc3339();

        let last_reinforced = serde_json::Value::String(ninety_days_ago);
        let valid_from = serde_json::Value::String(valid_from_long_ago);

        // Should use last_reinforced (90 days) not valid_from (365 days)
        let result = effective_confidence(0.6, Some(&last_reinforced), &valid_from, &now);
        assert!(
            approx_eq(result, 0.3),
            "got {} (expected ~0.3, one half-life from last_reinforced)",
            result
        );
    }

    #[test]
    fn effective_confidence_falls_back_to_valid_from() {
        let now = chrono::Utc::now();
        let ninety_days_ago = (now - chrono::Duration::days(90)).to_rfc3339();
        let valid_from = serde_json::Value::String(ninety_days_ago);

        // No last_reinforced — should use valid_from
        let result = effective_confidence(0.6, None, &valid_from, &now);
        assert!(
            approx_eq(result, 0.3),
            "got {} (expected ~0.3, one half-life from valid_from)",
            result
        );
    }

    #[test]
    fn effective_confidence_no_parseable_date() {
        let now = chrono::Utc::now();
        let bad_date = serde_json::Value::String("not-a-date".to_string());

        // Unparseable dates should return stored confidence unchanged
        let result = effective_confidence(0.8, None, &bad_date, &now);
        assert!(approx_eq(result, 0.8), "got {}", result);
    }

    #[test]
    fn extraction_context_from_str() {
        assert_eq!(
            "explicit".parse::<ExtractionContext>().unwrap(),
            ExtractionContext::Explicit
        );
        assert_eq!(
            "inferred".parse::<ExtractionContext>().unwrap(),
            ExtractionContext::Inferred
        );
        assert_eq!(
            "speculative".parse::<ExtractionContext>().unwrap(),
            ExtractionContext::Speculative
        );
        assert_eq!(
            "authoritative".parse::<ExtractionContext>().unwrap(),
            ExtractionContext::Authoritative
        );
        assert!("unknown".parse::<ExtractionContext>().is_err());
    }
}
