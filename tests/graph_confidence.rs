// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for Bayesian confidence model and temporal decay.
//!
//! These tests verify boundary conditions and edge cases for the
//! confidence scoring system that drives relationship weighting.

use recall_echo::graph::confidence::*;

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

// ── Evidence accumulation ──────────────────────────────────────────

/// Evidence at the prior for `mean`, after `observations` corroborations
/// (positive count) or contradictions (negative count).
fn accumulate(mean: f64, observations: i32) -> Evidence {
    let mut evidence = Evidence::from_prior(mean);
    for _ in 0..observations.abs() {
        if observations > 0 {
            evidence.corroborate(DEFAULT_EVIDENCE_WEIGHT);
        } else {
            evidence.contradict(DEFAULT_EVIDENCE_WEIGHT);
        }
    }
    evidence
}

#[test]
fn single_corroboration_from_all_priors() {
    // Authoritative (1.0): alpha=10, beta=0 -> corroborate: 11/11=1.0
    let auth = accumulate(1.0, 1).mean();
    assert!(auth > 0.99, "authoritative corroborate: {auth}");

    // Explicit (0.9): alpha=9, beta=1 -> corroborate: 10/11≈0.909
    let expl = accumulate(0.9, 1).mean();
    assert!(approx(expl, 0.909), "explicit corroborate: {expl}");

    // Inferred (0.6): alpha=6, beta=4 -> corroborate: 7/11≈0.636
    let inf = accumulate(0.6, 1).mean();
    assert!(approx(inf, 0.636), "inferred corroborate: {inf}");

    // Speculative (0.3): alpha=3, beta=7 -> corroborate: 4/11≈0.364
    let spec = accumulate(0.3, 1).mean();
    assert!(approx(spec, 0.364), "speculative corroborate: {spec}");
}

#[test]
fn contradiction_reduces_the_mean() {
    let before = 0.6;
    let after = accumulate(before, -1).mean();
    assert!(
        after < before,
        "contradiction should reduce: {before} -> {after}"
    );
}

#[test]
fn repeated_corroboration_converges_upward() {
    // Speculative prior (alpha=3, beta=7). Corroboration only adds to alpha —
    // the 7 units of prior doubt are never discarded, so convergence toward
    // 1.0 is bounded by how much evidence has actually accumulated. (The
    // pre-Phase-1 stateless update re-derived the counts each time and so
    // reached 0.95 in 50 steps; that speed was an artifact of forgetting.)
    let fifty = accumulate(0.3, 50).mean();
    assert!(fifty > 0.85, "50 corroborations: {fifty}");
    assert!(fifty < 1.0, "doubt is never fully erased: {fifty}");

    let two_hundred = accumulate(0.3, 200).mean();
    assert!(
        two_hundred > fifty,
        "more evidence must move further: {two_hundred} vs {fifty}"
    );
    assert!(two_hundred > 0.95, "200 corroborations: {two_hundred}");
}

#[test]
fn repeated_contradiction_converges_downward() {
    let fifty = accumulate(0.9, -50).mean();
    assert!(fifty < 0.2, "50 contradictions: {fifty}");

    let two_hundred = accumulate(0.9, -200).mean();
    assert!(
        two_hundred < fifty,
        "more evidence must move further: {two_hundred} vs {fifty}"
    );
    assert!(two_hundred < 0.05, "200 contradictions: {two_hundred}");
}

#[test]
fn accumulated_evidence_narrows_the_posterior() {
    // AC1: the property the stateless model could not express.
    let one = accumulate(0.6, 1);
    let five = accumulate(0.6, 5);
    let fifty = accumulate(0.6, 50);

    assert!(
        five.variance() < one.variance(),
        "5 obs {} vs 1 obs {}",
        five.variance(),
        one.variance()
    );
    assert!(
        fifty.variance() < five.variance(),
        "50 obs {} vs 5 obs {}",
        fifty.variance(),
        five.variance()
    );
    assert!(fifty.concentration() > five.concentration());
}

// ── Temporal Decay ────────────────────────────────────────────────

#[test]
fn decay_at_exact_half_life() {
    let result = temporal_decay(1.0, 90.0, 90.0);
    assert!(approx(result, 0.5), "one half-life: {result}");
}

#[test]
fn decay_at_two_half_lives() {
    let result = temporal_decay(1.0, 180.0, 90.0);
    assert!(approx(result, 0.25), "two half-lives: {result}");
}

#[test]
fn decay_at_three_half_lives() {
    let result = temporal_decay(1.0, 270.0, 90.0);
    assert!(approx(result, 0.125), "three half-lives: {result}");
}

#[test]
fn decay_floor_prevents_zero() {
    let result = temporal_decay(1.0, 10000.0, 90.0);
    assert_eq!(
        result, DECAY_FLOOR,
        "extreme age should hit floor: {result}"
    );
}

#[test]
fn decay_floor_with_low_initial() {
    let result = temporal_decay(0.1, 900.0, 90.0);
    assert_eq!(
        result, DECAY_FLOOR,
        "low initial + long time = floor: {result}"
    );
}

#[test]
fn decay_zero_days_unchanged() {
    let result = temporal_decay(0.8, 0.0, 90.0);
    assert!(approx(result, 0.8), "zero days: {result}");
}

#[test]
fn decay_negative_days_unchanged() {
    let result = temporal_decay(0.7, -10.0, 90.0);
    assert!(approx(result, 0.7), "negative days: {result}");
}

#[test]
fn decay_custom_half_life() {
    // Half-life of 30 days instead of 90
    let result = temporal_decay(1.0, 30.0, 30.0);
    assert!(approx(result, 0.5), "custom half-life: {result}");
}

// ── Path Confidence ───────────────────────────────────────────────

#[test]
fn path_confidence_single_edge() {
    assert!(approx(path_confidence(&[0.8]), 0.8));
}

#[test]
fn path_confidence_two_edges() {
    assert!(approx(path_confidence(&[0.8, 0.7]), 0.56));
}

#[test]
fn path_confidence_three_edges() {
    assert!(approx(path_confidence(&[0.9, 0.8, 0.7]), 0.504));
}

#[test]
fn path_confidence_degrades_with_hops() {
    let one = path_confidence(&[0.9]);
    let two = path_confidence(&[0.9, 0.9]);
    let three = path_confidence(&[0.9, 0.9, 0.9]);
    assert!(one > two, "two hops should be less than one");
    assert!(two > three, "three hops should be less than two");
}

#[test]
fn path_confidence_empty_is_one() {
    assert_eq!(path_confidence(&[]), 1.0);
}

// ── Extraction Context ───────────────────────────────────────────

#[test]
fn extraction_context_priors_ordered() {
    assert!(ExtractionContext::Authoritative.prior() > ExtractionContext::Explicit.prior());
    assert!(ExtractionContext::Explicit.prior() > ExtractionContext::Inferred.prior());
    assert!(ExtractionContext::Inferred.prior() > ExtractionContext::Speculative.prior());
}

#[test]
fn extraction_context_roundtrip() {
    for ctx in [
        ExtractionContext::Authoritative,
        ExtractionContext::Explicit,
        ExtractionContext::Inferred,
        ExtractionContext::Speculative,
    ] {
        let s = format!("{:?}", ctx).to_lowercase();
        let parsed: ExtractionContext = s.parse().unwrap();
        assert_eq!(parsed, ctx);
    }
}

// ── Effective Confidence ─────────────────────────────────────────

#[test]
fn effective_confidence_uses_last_reinforced_over_valid_from() {
    let now = chrono::Utc::now();
    let recent = (now - chrono::Duration::days(10)).to_rfc3339();
    let old = (now - chrono::Duration::days(300)).to_rfc3339();

    let last_reinforced = serde_json::Value::String(recent);
    let valid_from = serde_json::Value::String(old);

    let result = effective_confidence(0.8, Some(&last_reinforced), &valid_from, &now);
    // Should use 10 days (recent), not 300 days (old)
    assert!(
        result > 0.7,
        "should use last_reinforced (10d), got {result}"
    );
}

#[test]
fn effective_confidence_unparseable_returns_stored() {
    let now = chrono::Utc::now();
    let bad = serde_json::Value::String("not-a-date".into());
    let result = effective_confidence(0.8, None, &bad, &now);
    assert!(
        approx(result, 0.8),
        "unparseable should return stored: {result}"
    );
}
