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
            other => Err(format!("unknown extraction context: {}", other)),
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
pub fn bayesian_update(current_confidence: f64, corroborate: bool) -> f64 {
    let alpha = current_confidence * PSEUDOCOUNT;
    let beta = PSEUDOCOUNT - alpha;

    if corroborate {
        (alpha + 1.0) / (alpha + beta + 1.0)
    } else {
        alpha / (alpha + beta + 1.0)
    }
}

/// Configuration for temporal decay of relationship confidence.
#[derive(Debug, Clone)]
pub struct DecayConfig {
    /// Half-life in days. After this many days unreinforced, confidence halves.
    pub half_life_days: f64,
    /// Minimum effective confidence. Relationships never decay below this.
    pub floor: f64,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            half_life_days: 90.0,
            floor: 0.05,
        }
    }
}

/// Compute the effective confidence after temporal decay.
///
/// Uses half-life exponential decay:
///   effective = stored × 0.5^(days_since_reinforced / half_life)
///
/// Clamped to `floor` so relationships never fully vanish — they become GC candidates.
/// If `days_since_reinforced` is zero or negative, returns stored confidence unchanged.
pub fn temporal_decay(stored_confidence: f64, days_since_reinforced: f64, config: &DecayConfig) -> f64 {
    if days_since_reinforced <= 0.0 || config.half_life_days <= 0.0 {
        return stored_confidence;
    }

    let decay_factor = 0.5_f64.powf(days_since_reinforced / config.half_life_days);
    let effective = stored_confidence * decay_factor;

    effective.max(config.floor)
}

/// Parse a SurrealDB datetime JSON value and compute days elapsed since then.
/// Returns None if the value can't be parsed or is in the future.
pub fn days_since(timestamp: &serde_json::Value) -> Option<f64> {
    let ts_str = match timestamp {
        serde_json::Value::String(s) => s.as_str(),
        _ => return None,
    };

    // SurrealDB returns ISO 8601 datetimes
    let parsed = chrono::DateTime::parse_from_rfc3339(ts_str)
        .or_else(|_| {
            // Try without timezone (SurrealDB sometimes omits it)
            chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|naive| naive.and_utc().fixed_offset())
        })
        .ok()?;

    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(parsed);
    let days = duration.num_seconds() as f64 / 86400.0;

    if days < 0.0 { None } else { Some(days) }
}

/// Compute effective confidence for an edge, applying temporal decay.
///
/// Uses `last_reinforced` if available, falls back to `valid_from`.
/// If neither can be parsed, returns stored confidence unchanged.
pub fn effective_confidence(
    stored_confidence: f64,
    last_reinforced: Option<&serde_json::Value>,
    valid_from: &serde_json::Value,
    config: &DecayConfig,
) -> f64 {
    let reference_time = last_reinforced.unwrap_or(valid_from);
    match days_since(reference_time) {
        Some(days) => temporal_decay(stored_confidence, days, config),
        None => stored_confidence,
    }
}

/// Compound confidence along a multi-hop path.
///
/// Returns the product of edge confidences. An empty path returns 1.0.
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

    // ── Temporal decay tests ─────────────────────────────────────────

    #[test]
    fn decay_zero_days_unchanged() {
        let config = DecayConfig::default();
        let result = temporal_decay(0.6, 0.0, &config);
        assert_eq!(result, 0.6);
    }

    #[test]
    fn decay_half_life_halves() {
        let config = DecayConfig::default(); // 90 days
        let result = temporal_decay(0.6, 90.0, &config);
        assert!(approx_eq(result, 0.3), "got {}", result);
    }

    #[test]
    fn decay_two_half_lives_quarters() {
        let config = DecayConfig::default(); // 90 days
        let result = temporal_decay(0.8, 180.0, &config);
        assert!(approx_eq(result, 0.2), "got {}", result);
    }

    #[test]
    fn decay_respects_floor() {
        let config = DecayConfig {
            half_life_days: 90.0,
            floor: 0.05,
        };
        // After many half-lives, should clamp to floor
        let result = temporal_decay(0.6, 900.0, &config);
        assert!(approx_eq(result, 0.05), "got {}", result);
    }

    #[test]
    fn decay_negative_days_unchanged() {
        let config = DecayConfig::default();
        let result = temporal_decay(0.6, -5.0, &config);
        assert_eq!(result, 0.6);
    }

    #[test]
    fn decay_custom_half_life() {
        let config = DecayConfig {
            half_life_days: 30.0,
            floor: 0.01,
        };
        let result = temporal_decay(1.0, 30.0, &config);
        assert!(approx_eq(result, 0.5), "got {}", result);
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
