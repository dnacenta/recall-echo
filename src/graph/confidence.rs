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

/// Evidence weight of a single observation whose provenance is not (yet)
/// modelled. Provenance-weighted observations arrive in Phase 1 increment 2;
/// until then every observation counts once.
pub const DEFAULT_EVIDENCE_WEIGHT: f64 = 1.0;

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
