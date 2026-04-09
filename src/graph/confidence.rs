//! Bayesian confidence model for relationship edges.
//!
//! Uses Beta-Binomial conjugate prior with pseudocount 10.
//! Confidence moves slowly per observation but accumulates with repeated evidence.

use serde::{Deserialize, Serialize};

/// Pseudocount total for the Beta-Binomial prior.
/// ~10 observations to overwhelm the prior.
const PSEUDOCOUNT: f64 = 10.0;

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

/// Bayesian update using Beta-Binomial conjugate prior.
///
/// Given a current confidence (interpreted as alpha / (alpha + beta) with
/// total pseudocount), updates the posterior by adding one observation.
///
/// - `corroborate = true`: alpha += 1 (evidence supports the relationship)
/// - `corroborate = false`: beta += 1 (evidence contradicts the relationship)
#[must_use]
pub fn bayesian_update(current_confidence: f64, corroborate: bool) -> f64 {
    let alpha = current_confidence * PSEUDOCOUNT;
    let beta = PSEUDOCOUNT - alpha;

    if corroborate {
        (alpha + 1.0) / (alpha + beta + 1.0)
    } else {
        alpha / (alpha + beta + 1.0)
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

    #[test]
    fn bayesian_update_corroborate_0_6() {
        let result = bayesian_update(0.6, true);
        // alpha=6, beta=4 -> (6+1)/(10+1) = 7/11 ≈ 0.636
        assert!(approx_eq(result, 0.636), "got {}", result);
    }

    #[test]
    fn bayesian_update_contradict_0_6() {
        let result = bayesian_update(0.6, false);
        // alpha=6, beta=4 -> 6/(10+1) = 6/11 ≈ 0.545
        assert!(approx_eq(result, 0.545), "got {}", result);
    }

    #[test]
    fn bayesian_update_corroborate_0_9() {
        let result = bayesian_update(0.9, true);
        // alpha=9, beta=1 -> (9+1)/(10+1) = 10/11 ≈ 0.909
        assert!(approx_eq(result, 0.909), "got {}", result);
    }

    #[test]
    fn bayesian_update_contradict_0_9() {
        let result = bayesian_update(0.9, false);
        // alpha=9, beta=1 -> 9/(10+1) = 9/11 ≈ 0.818
        assert!(approx_eq(result, 0.818), "got {}", result);
    }

    #[test]
    fn bayesian_update_corroborate_0_3() {
        let result = bayesian_update(0.3, true);
        // alpha=3, beta=7 -> (3+1)/(10+1) = 4/11 ≈ 0.364
        assert!(approx_eq(result, 0.364), "got {}", result);
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
