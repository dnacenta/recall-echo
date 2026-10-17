// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! E9 — provenance-weighting ablation, arithmetic variant.
//!
//! Pre-registration: <https://github.com/dnacenta/recall-echo/issues/79>.
//!
//! An agent keeps restating a false claim (`recall-echo -uses-> PostgreSQL`)
//! while an independent source contradicts it. Each restatement is a
//! corroboration at [`Provenance::SelfGenerated`]; each contradiction comes
//! from [`Provenance::External`] (the model's arithmetic) or
//! [`Provenance::User`] (the only contradiction path the shipped code has,
//! `graph correct --wrong`). The same sequence is replayed with
//! `weight_self` at the shipped 0.05, a middle 0.25, and 1.0 — self counting
//! like an independent source.
//!
//! Everything goes through `recall_echo::graph::confidence`; no model calls,
//! no store, deterministic. Each subcommand prints one CSV to stdout:
//!
//! ```text
//! cargo run --example e9_provenance_ablation -- grid        # 810 rows
//! cargo run --example e9_provenance_ablation -- recovery    # 108 rows
//! cargo run --example e9_provenance_ablation -- trajectory  # T1 and T2
//! cargo run --example e9_provenance_ablation -- check       # H1, H2, H3, H4
//! ```

use std::process::ExitCode;

use recall_echo::graph::confidence::{
    EdgeEvidence, Evidence, Provenance, ProvenanceWeights, PRIOR_CONCENTRATION,
};

/// `Speculative`, `Inferred` (the pipeline default), `Explicit`.
const PRIORS: [f64; 3] = [0.3, 0.6, 0.9];
const PRIMARY_PRIOR: f64 = 0.6;
/// Shipped default, a middle value, and the ablation.
const SELF_WEIGHTS: [f64; 3] = [0.05, 0.25, 1.0];
const SELF_ASSERTION_COUNTS: [usize; 5] = [0, 1, 3, 10, 30];
const CONTRADICTION_COUNTS: [usize; 3] = [0, 1, 3];
const RECOVERY_CONTRADICTION_COUNTS: [usize; 2] = [1, 3];
const CONTRADICTION_SOURCES: [Provenance; 2] = [Provenance::External, Provenance::User];
/// Incremental sums of 0.05 must not move an exact boundary by one step.
const TOLERANCE: f64 = 1e-9;
const RECOVERY_CAP: u64 = 1_000_000;

// ── Scenario ─────────────────────────────────────────────────────────

/// One observation about the false claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The agent restates the claim.
    SelfAssertion,
    /// The contradiction source denies it.
    Contradiction,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::SelfAssertion => "self_assertion",
            Self::Contradiction => "contradiction",
        }
    }
}

/// Where the contradictions sit relative to the self-assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ordering {
    ContradictionsFirst,
    ContradictionsLast,
    /// Contradiction `k` (0-based) follows ⌊(k+1)·N/(M+1)⌋ self-assertions.
    Interleaved,
}

impl Ordering {
    const ALL: [Self; 3] = [
        Self::ContradictionsFirst,
        Self::ContradictionsLast,
        Self::Interleaved,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::ContradictionsFirst => "contradictions_first",
            Self::ContradictionsLast => "contradictions_last",
            Self::Interleaved => "interleaved",
        }
    }

    fn sequence(self, self_assertions: usize, contradictions: usize) -> Vec<Step> {
        let repeat = |step, count| std::iter::repeat_n(step, count);
        match self {
            Self::ContradictionsFirst => repeat(Step::Contradiction, contradictions)
                .chain(repeat(Step::SelfAssertion, self_assertions))
                .collect(),
            Self::ContradictionsLast => repeat(Step::SelfAssertion, self_assertions)
                .chain(repeat(Step::Contradiction, contradictions))
                .collect(),
            Self::Interleaved => interleave(self_assertions, contradictions),
        }
    }
}

fn interleave(self_assertions: usize, contradictions: usize) -> Vec<Step> {
    let mut steps = Vec::with_capacity(self_assertions + contradictions);
    let mut placed = 0;
    for k in 0..contradictions {
        let insert_after = (k + 1) * self_assertions / (contradictions + 1);
        steps.extend(std::iter::repeat_n(
            Step::SelfAssertion,
            insert_after - placed,
        ));
        placed = insert_after;
        steps.push(Step::Contradiction);
    }
    steps.extend(std::iter::repeat_n(
        Step::SelfAssertion,
        self_assertions - placed,
    ));
    steps
}

/// The weights and contradiction source one replay runs under.
#[derive(Debug, Clone, Copy)]
struct Condition {
    weights: ProvenanceWeights,
    contradiction_source: Provenance,
}

impl Condition {
    /// Shipped defaults for every class except the one being ablated.
    fn new(weight_self: f64, contradiction_source: Provenance) -> Self {
        Self {
            weights: ProvenanceWeights {
                weight_self,
                ..ProvenanceWeights::default()
            },
            contradiction_source,
        }
    }

    fn weight_self(&self) -> f64 {
        self.weights.weight_self
    }

    fn weight_contradiction(&self) -> f64 {
        self.weights.for_provenance(self.contradiction_source)
    }

    fn apply(&self, edge: &mut EdgeEvidence, step: Step) {
        match step {
            Step::SelfAssertion => edge.corroborate(Provenance::SelfGenerated, &self.weights),
            Step::Contradiction => edge.contradict(self.contradiction_source, &self.weights),
        }
    }
}

/// A fresh edge for the claim, exactly as `add_relationship` starts one.
fn new_edge(prior: f64) -> EdgeEvidence {
    EdgeEvidence::new(Evidence::from_prior(prior), 0)
}

fn mean(edge: EdgeEvidence) -> f64 {
    edge.evidence().mean()
}

// ── Closed form (the model the library is checked against) ───────────

/// `(c·p + w_s·N) / (c + w_s·N + w_c·M)` with `c` = prior concentration.
fn closed_form_mean(
    prior: f64,
    condition: &Condition,
    self_assertions: usize,
    contradictions: usize,
) -> f64 {
    let support = PRIOR_CONCENTRATION * prior + condition.weight_self() * self_assertions as f64;
    let against = PRIOR_CONCENTRATION * (1.0 - prior)
        + condition.weight_contradiction() * contradictions as f64;
    support / (support + against)
}

/// Self-assertions needed after `contradictions` to reach `target`.
fn closed_form_recovery(
    prior: f64,
    condition: &Condition,
    contradictions: usize,
    target: f64,
) -> u64 {
    let shortfall = target
        * (PRIOR_CONCENTRATION + condition.weight_contradiction() * contradictions as f64)
        - PRIOR_CONCENTRATION * prior;
    if shortfall <= 0.0 {
        return 0;
    }
    let steps = shortfall / (condition.weight_self() * (1.0 - target));
    (steps - TOLERANCE).ceil() as u64
}

// ── Replays ──────────────────────────────────────────────────────────

/// The end state of one sequence, plus the extremes the mean passed through
/// (the prior state included).
#[derive(Debug, Clone, Copy)]
struct Replay {
    edge: EdgeEvidence,
    min_mean: f64,
    max_mean: f64,
}

fn replay(prior: f64, condition: &Condition, steps: &[Step]) -> Replay {
    let mut edge = new_edge(prior);
    let (mut min_mean, mut max_mean) = (mean(edge), mean(edge));
    for &step in steps {
        condition.apply(&mut edge, step);
        min_mean = min_mean.min(mean(edge));
        max_mean = max_mean.max(mean(edge));
    }
    Replay {
        edge,
        min_mean,
        max_mean,
    }
}

/// One cell of the pre-registered grid.
#[derive(Debug, Clone, Copy)]
struct GridCell {
    prior: f64,
    condition: Condition,
    self_assertions: usize,
    contradictions: usize,
    ordering: Ordering,
}

impl GridCell {
    fn replay(&self) -> Replay {
        let steps = self
            .ordering
            .sequence(self.self_assertions, self.contradictions);
        replay(self.prior, &self.condition, &steps)
    }

    fn differs_only_in_ordering(&self, other: &Self) -> bool {
        self.prior == other.prior
            && self.condition.weights == other.condition.weights
            && self.condition.contradiction_source == other.condition.contradiction_source
            && self.self_assertions == other.self_assertions
            && self.contradictions == other.contradictions
    }

    fn closed_form_mean(&self) -> f64 {
        closed_form_mean(
            self.prior,
            &self.condition,
            self.self_assertions,
            self.contradictions,
        )
    }
}

fn grid() -> Vec<GridCell> {
    let mut cells = Vec::new();
    for prior in PRIORS {
        for weight_self in SELF_WEIGHTS {
            for source in CONTRADICTION_SOURCES {
                for self_assertions in SELF_ASSERTION_COUNTS {
                    for contradictions in CONTRADICTION_COUNTS {
                        for ordering in Ordering::ALL {
                            cells.push(GridCell {
                                prior,
                                condition: Condition::new(weight_self, source),
                                self_assertions,
                                contradictions,
                                ordering,
                            });
                        }
                    }
                }
            }
        }
    }
    cells
}

/// What a recovery run is trying to get back to.
#[derive(Debug, Clone, Copy)]
enum Target {
    Fixed(f64),
    /// The claim's confidence before it was contradicted.
    Prior,
}

impl Target {
    const ALL: [Self; 3] = [Self::Fixed(0.5), Self::Prior, Self::Fixed(0.9)];

    fn value(self, prior: f64) -> f64 {
        match self {
            Self::Fixed(value) => value,
            Self::Prior => prior,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Fixed(value) => format!("{value}"),
            Self::Prior => "prior".to_string(),
        }
    }
}

/// Contradict `contradictions` times, then restate until `target` is reached.
struct Recovery {
    mean_after_contradictions: f64,
    /// `None` if the cap was hit first.
    self_assertions_needed: Option<u64>,
    final_mean: f64,
}

fn recover(prior: f64, condition: &Condition, contradictions: usize, target: f64) -> Recovery {
    let mut edge = new_edge(prior);
    for _ in 0..contradictions {
        condition.apply(&mut edge, Step::Contradiction);
    }
    let mean_after_contradictions = mean(edge);

    let mut needed = 0;
    while mean(edge) < target - TOLERANCE {
        if needed == RECOVERY_CAP {
            return Recovery {
                mean_after_contradictions,
                self_assertions_needed: None,
                final_mean: mean(edge),
            };
        }
        condition.apply(&mut edge, Step::SelfAssertion);
        needed += 1;
    }
    Recovery {
        mean_after_contradictions,
        self_assertions_needed: Some(needed),
        final_mean: mean(edge),
    }
}

/// One row of the pre-registered recovery table.
#[derive(Debug, Clone, Copy)]
struct RecoveryCase {
    prior: f64,
    condition: Condition,
    contradictions: usize,
    target: Target,
}

impl RecoveryCase {
    fn target_value(&self) -> f64 {
        self.target.value(self.prior)
    }

    fn run(&self) -> Recovery {
        recover(
            self.prior,
            &self.condition,
            self.contradictions,
            self.target_value(),
        )
    }

    fn closed_form(&self) -> u64 {
        closed_form_recovery(
            self.prior,
            &self.condition,
            self.contradictions,
            self.target_value(),
        )
    }
}

fn recovery_cases() -> Vec<RecoveryCase> {
    let mut cases = Vec::new();
    for prior in PRIORS {
        for weight_self in SELF_WEIGHTS {
            for source in CONTRADICTION_SOURCES {
                for contradictions in RECOVERY_CONTRADICTION_COUNTS {
                    for target in Target::ALL {
                        cases.push(RecoveryCase {
                            prior,
                            condition: Condition::new(weight_self, source),
                            contradictions,
                            target,
                        });
                    }
                }
            }
        }
    }
    cases
}

/// The two pre-registered trajectories.
fn trajectories() -> [(&'static str, Vec<Step>); 2] {
    let restate = |count| std::iter::repeat_n(Step::SelfAssertion, count);
    [
        (
            "T1_repeat_contradict_repeat",
            restate(30)
                .chain([Step::Contradiction])
                .chain(restate(30))
                .collect(),
        ),
        (
            "T2_contradict_then_repeat",
            [Step::Contradiction]
                .into_iter()
                .chain(restate(60))
                .collect(),
        ),
    ]
}

// ── Output ───────────────────────────────────────────────────────────

fn print_grid() {
    println!(
        "prior,weight_self,contradiction_source,n_self,m_contradictions,ordering,\
         final_mean,closed_form_mean,min_mean,max_mean,alpha,beta,concentration,variance,\
         self_reinforcements"
    );
    for cell in grid() {
        let result = cell.replay();
        let evidence = result.edge.evidence();
        println!(
            "{},{},{},{},{},{},{:.10},{:.10},{:.10},{:.10},{:.4},{:.4},{:.4},{:.10},{}",
            cell.prior,
            cell.condition.weight_self(),
            cell.condition.contradiction_source,
            cell.self_assertions,
            cell.contradictions,
            cell.ordering.label(),
            evidence.mean(),
            cell.closed_form_mean(),
            result.min_mean,
            result.max_mean,
            evidence.alpha(),
            evidence.beta(),
            evidence.concentration(),
            evidence.variance(),
            result.edge.self_reinforcements(),
        );
    }
}

fn print_recovery() {
    println!(
        "prior,weight_self,contradiction_source,m_contradictions,target,target_value,\
         mean_after_contradictions,self_assertions_needed,closed_form_needed,final_mean"
    );
    for case in recovery_cases() {
        let recovery = case.run();
        let needed = recovery
            .self_assertions_needed
            .map_or_else(|| format!(">{RECOVERY_CAP}"), |n| n.to_string());
        println!(
            "{},{},{},{},{},{},{:.10},{},{},{:.10}",
            case.prior,
            case.condition.weight_self(),
            case.condition.contradiction_source,
            case.contradictions,
            case.target.label(),
            case.target_value(),
            recovery.mean_after_contradictions,
            needed,
            case.closed_form(),
            recovery.final_mean,
        );
    }
}

fn print_trajectories() {
    println!("trajectory,weight_self,step,observation,mean,alpha,beta,self_reinforcements");
    for (name, steps) in trajectories() {
        for weight_self in SELF_WEIGHTS {
            let condition = Condition::new(weight_self, Provenance::External);
            let mut edge = new_edge(PRIMARY_PRIOR);
            let print_row = |step: usize, observation: &str, edge: EdgeEvidence| {
                let evidence = edge.evidence();
                println!(
                    "{name},{weight_self},{step},{observation},{:.10},{:.4},{:.4},{}",
                    evidence.mean(),
                    evidence.alpha(),
                    evidence.beta(),
                    edge.self_reinforcements(),
                );
            };
            print_row(0, "prior", edge);
            for (index, &step) in steps.iter().enumerate() {
                condition.apply(&mut edge, step);
                print_row(index + 1, step.label(), edge);
            }
        }
    }
}

// ── Hypothesis checks ────────────────────────────────────────────────

/// H1, H2, H3, H4 from the pre-registration, as `check,value,pass` rows.
fn run_checks() -> ExitCode {
    let cells = grid();
    let replays: Vec<Replay> = cells.iter().map(GridCell::replay).collect();

    let h1_mean_error = cells
        .iter()
        .zip(&replays)
        .map(|(cell, result)| (mean(result.edge) - cell.closed_form_mean()).abs())
        .fold(0.0, f64::max);

    let cases = recovery_cases();
    let recoveries: Vec<Recovery> = cases.iter().map(RecoveryCase::run).collect();
    let h1_recovery_mismatches = cases
        .iter()
        .zip(&recoveries)
        .filter(|(case, recovery)| recovery.self_assertions_needed != Some(case.closed_form()))
        .count();
    let h2_unreached = recoveries
        .iter()
        .filter(|recovery| recovery.self_assertions_needed.is_none())
        .count();

    let h3_order_spread = cells
        .chunks(Ordering::ALL.len())
        .zip(replays.chunks(Ordering::ALL.len()))
        .map(|(group, results)| {
            assert!(
                group
                    .iter()
                    .all(|cell| cell.differs_only_in_ordering(&group[0])),
                "grid order changed: orderings are no longer the innermost factor"
            );
            let means = results.iter().map(|result| mean(result.edge));
            let low = means.clone().fold(f64::INFINITY, f64::min);
            let high = means.fold(f64::NEG_INFINITY, f64::max);
            high - low
        })
        .fold(0.0, f64::max);

    let h4_tally_mismatches = cells
        .iter()
        .zip(&replays)
        .filter(|(cell, result)| result.edge.self_reinforcements() != cell.self_assertions as i64)
        .count();

    let checks = [
        (
            "H1_max_abs_mean_error",
            h1_mean_error,
            h1_mean_error <= TOLERANCE,
        ),
        (
            "H1_recovery_mismatches",
            h1_recovery_mismatches as f64,
            h1_recovery_mismatches == 0,
        ),
        (
            "H2_targets_unreached",
            h2_unreached as f64,
            h2_unreached == 0,
        ),
        (
            "H3_max_order_spread",
            h3_order_spread,
            h3_order_spread <= 1e-12,
        ),
        (
            "H4_tally_mismatches",
            h4_tally_mismatches as f64,
            h4_tally_mismatches == 0,
        ),
    ];

    println!("check,value,pass");
    println!("grid_rows,{},true", cells.len());
    println!("recovery_rows,{},true", cases.len());
    for (name, value, pass) in checks {
        println!("{name},{value:e},{pass}");
    }

    if checks.iter().all(|(_, _, pass)| *pass) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("grid") => print_grid(),
        Some("recovery") => print_recovery(),
        Some("trajectory") => print_trajectories(),
        Some("check") => return run_checks(),
        _ => {
            eprintln!("usage: e9_provenance_ablation <grid|recovery|trajectory|check>");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
