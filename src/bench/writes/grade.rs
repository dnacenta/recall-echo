// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Diff ingested edges against a fact script.
//!
//! Scoring is a pure function of the script and the edges observed before and
//! after each session. A claim counts as a true positive only when the edge
//! moved the way that operation moves it, including the provenance weight on
//! a corroboration or a contradiction.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::script::{
    compile_pattern, Claim, ClaimOp, EndProvenance, EndState, FactScript, ScriptFact,
    ScriptSession, ScriptTurn, Speaker,
};
use crate::error::RecallError;
use crate::graph::ProvenanceWeights;

const EPS: f64 = 1e-6;

/// Which extractor produced the claims that were graded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractorLabel {
    Scripted,
    Provider,
}

/// An operation the grader can score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOp {
    Create,
    Corroborate,
    Supersede,
    Contradict,
    #[serde(rename = "no-op")]
    NoOp,
    /// An edge that was present and then gone. Never expected by a claim.
    Removed,
    /// An edge that was not live and then was. Never expected by a claim.
    Revived,
}

/// One edge as read back from the store.
#[derive(Debug, Clone)]
pub struct ObservedEdge {
    pub id: String,
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub live: bool,
    pub alpha: f64,
    pub beta: f64,
    pub self_reinforcements: i64,
    /// α at creation. Filled from β on the first snapshot that contained this
    /// edge (`PRIOR_CONCENTRATION - β`) and kept on every later snapshot: a
    /// contradiction moves β, so the running β is not the prior.
    pub prior_alpha: f64,
}

/// Edges around one ingested session, plus what that session cost.
#[derive(Debug, Clone)]
pub struct SessionObservation {
    pub id: String,
    pub before: Vec<ObservedEdge>,
    pub after: Vec<ObservedEdge>,
    pub calls: u64,
    pub millis: u64,
    pub warnings: Vec<String>,
}

/// Precision and recall for one operation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpScore {
    pub tp: u32,
    pub fp: u32,
    #[serde(rename = "fn")]
    pub missed: u32,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
}

/// Per-operation scores.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpTable {
    pub create: OpScore,
    pub corroborate: OpScore,
    pub supersede: OpScore,
    pub contradict: OpScore,
    #[serde(rename = "no-op")]
    pub no_op: OpScore,
    pub removed: OpScore,
    pub revived: OpScore,
}

/// A fact whose end state does not match the script.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct WrongFact {
    pub fact: String,
    pub reason: WrongReason,
}

/// Why an expected fact was counted wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WrongReason {
    StillLive,
    Missing,
    Provenance,
}

/// An edge that matches no fact in the script.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct JunkEdge {
    pub subject: String,
    pub relation: String,
    pub object: String,
}

/// One claim, and the operation the graph actually performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepLog {
    pub session: String,
    pub fact: String,
    pub expected: WriteOp,
    pub actual: WriteOp,
    pub matched: bool,
}

/// Calls, time and warnings for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReport {
    pub id: String,
    pub calls: u64,
    pub millis: u64,
    pub warnings: Vec<String>,
}

/// Keyword search versus the facts a derived question needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuestionScore {
    pub text: String,
    pub keyword_hit: bool,
    pub graph_hit: bool,
}

/// The grader's report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteReport {
    pub name: String,
    pub extractor: ExtractorLabel,
    pub operations: OpTable,
    pub wrong_facts: Vec<WrongFact>,
    pub junk_edges: Vec<JunkEdge>,
    pub sessions: Vec<SessionReport>,
    pub steps: Vec<StepLog>,
    pub questions: Vec<QuestionScore>,
    pub calls: u64,
    /// Sum of per-session ingest time. Model load is [`WriteReport::setup_millis`].
    pub millis: u64,
    /// Time to open the embedder before the first session is timed.
    pub setup_millis: u64,
    /// Pattern score. Absent when no fact carries a match block, so a turn
    /// script's report keeps its previous shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patterns: Option<PatternScore>,
}

/// How many facts, pairs and noise items landed in each pattern bucket.
///
/// Counts are the bucket sizes. `ids` names the facts, pairs (`old->new`),
/// noise items, or unmatched record lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatternScore {
    pub facts: u32,
    pub pairs: u32,
    pub noise: u32,
    pub found: PatternBucket,
    pub complete: PatternBucket,
    pub wrong: PatternBucket,
    pub missing: PatternBucket,
    /// Superseded pairs whose old fact was retired in `retire_in` or later.
    pub retired: PatternBucket,
    /// Superseded pairs whose old fact was retired before `retire_in`.
    pub premature: PatternBucket,
    pub noise_hits: PatternBucket,
    /// Live records that match no fact's core patterns. A proxy for junk.
    pub unmatched: PatternBucket,
}

/// One pattern bucket: a count and the ids in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatternBucket {
    pub count: u32,
    pub ids: Vec<String>,
}

type EdgeKey = (String, String, String);

#[derive(Clone)]
enum Effect {
    Created {
        key: EdgeKey,
    },
    Corroborated {
        key: EdgeKey,
        delta_alpha: f64,
        delta_self: i64,
        delta_beta: f64,
    },
    Retired {
        key: EdgeKey,
    },
    Contradicted {
        key: EdgeKey,
        delta_beta: f64,
    },
    Removed,
    Revived,
}

impl Effect {
    fn op(&self) -> WriteOp {
        match self {
            Self::Created { .. } => WriteOp::Create,
            Self::Corroborated { .. } => WriteOp::Corroborate,
            Self::Retired { .. } => WriteOp::Supersede,
            Self::Contradicted { .. } => WriteOp::Contradict,
            Self::Removed => WriteOp::Removed,
            Self::Revived => WriteOp::Revived,
        }
    }
}

struct Counts {
    tp: u32,
    fp: u32,
    missed: u32,
}

impl Counts {
    fn score(self) -> OpScore {
        OpScore {
            precision: ratio(self.tp, self.tp + self.fp),
            recall: ratio(self.tp, self.tp + self.missed),
            tp: self.tp,
            fp: self.fp,
            missed: self.missed,
        }
    }
}

struct Tally {
    create: Counts,
    corroborate: Counts,
    supersede: Counts,
    contradict: Counts,
    no_op: Counts,
    removed: Counts,
    revived: Counts,
}

impl Tally {
    fn new() -> Self {
        Self {
            create: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
            corroborate: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
            supersede: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
            contradict: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
            no_op: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
            removed: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
            revived: Counts {
                tp: 0,
                fp: 0,
                missed: 0,
            },
        }
    }

    fn counts(&mut self, op: WriteOp) -> &mut Counts {
        match op {
            WriteOp::Create => &mut self.create,
            WriteOp::Corroborate => &mut self.corroborate,
            WriteOp::Supersede => &mut self.supersede,
            WriteOp::Contradict => &mut self.contradict,
            WriteOp::NoOp => &mut self.no_op,
            WriteOp::Removed => &mut self.removed,
            WriteOp::Revived => &mut self.revived,
        }
    }

    fn hit(&mut self, op: WriteOp) {
        self.counts(op).tp += 1;
    }

    fn miss(&mut self, op: WriteOp) {
        self.counts(op).missed += 1;
    }

    fn false_positive(&mut self, op: WriteOp) {
        self.counts(op).fp += 1;
    }
}

/// Grade `sessions` against `script`.
///
/// `keyword_hits` is parallel to `script.questions`: whether keyword search
/// over the archives returned a file containing the answer.
pub fn grade(
    script: &FactScript,
    sessions: &[SessionObservation],
    weights: &ProvenanceWeights,
    keyword_hits: &[bool],
    extractor: ExtractorLabel,
) -> Result<WriteReport, RecallError> {
    if sessions.len() != script.sessions.len() {
        return Err(RecallError::Config(format!(
            "expected {} session observations, got {}",
            script.sessions.len(),
            sessions.len()
        )));
    }
    if keyword_hits.len() != script.questions.len() {
        return Err(RecallError::Config(format!(
            "expected {} keyword results, got {}",
            script.questions.len(),
            keyword_hits.len()
        )));
    }

    let mut tally = Tally::new();
    let mut steps = Vec::new();
    for (session, observed) in script.sessions.iter().zip(sessions) {
        if session.id != observed.id {
            return Err(RecallError::Config(format!(
                "observation '{}' does not match session '{}'",
                observed.id, session.id
            )));
        }
        grade_session(script, session, observed, weights, &mut tally, &mut steps)?;
    }

    let final_edges = sessions.last().map(|s| s.after.as_slice()).unwrap_or(&[]);
    let mut wrong_facts = end_state(script, final_edges, weights)?;
    let mut junk_edges = junk(script, final_edges)?;
    wrong_facts.sort();
    junk_edges.sort();
    let questions = questions(script, final_edges, keyword_hits)?;

    Ok(WriteReport {
        name: script.name.clone(),
        extractor,
        operations: OpTable {
            create: tally.create.score(),
            corroborate: tally.corroborate.score(),
            supersede: tally.supersede.score(),
            contradict: tally.contradict.score(),
            no_op: tally.no_op.score(),
            removed: tally.removed.score(),
            revived: tally.revived.score(),
        },
        wrong_facts,
        junk_edges,
        sessions: sessions
            .iter()
            .map(|s| SessionReport {
                id: s.id.clone(),
                calls: s.calls,
                millis: s.millis,
                warnings: s.warnings.clone(),
            })
            .collect(),
        steps,
        questions,
        calls: sessions.iter().map(|s| s.calls).sum(),
        millis: sessions.iter().map(|s| s.millis).sum(),
        setup_millis: 0,
        patterns: score_patterns(script, sessions)?,
    })
}

/// Everything one session's claims are scored against.
struct Score<'a> {
    script: &'a FactScript,
    session: &'a ScriptSession,
    weights: &'a ProvenanceWeights,
    tally: &'a mut Tally,
    steps: &'a mut Vec<StepLog>,
    unused: &'a mut [(Effect, bool)],
}

impl Score<'_> {
    fn record(&mut self, fact: &str, expected: WriteOp, actual: WriteOp, matched: bool) {
        if matched {
            self.tally.hit(expected);
        } else {
            self.tally.miss(expected);
        }
        self.steps.push(StepLog {
            session: self.session.id.clone(),
            fact: fact.to_string(),
            expected,
            actual,
            matched,
        });
    }
}

fn grade_session(
    script: &FactScript,
    session: &ScriptSession,
    observed: &SessionObservation,
    weights: &ProvenanceWeights,
    tally: &mut Tally,
    steps: &mut Vec<StepLog>,
) -> Result<(), RecallError> {
    let mut unused: Vec<(Effect, bool)> = diff(&observed.before, &observed.after)
        .into_iter()
        .map(|effect| (effect, false))
        .collect();

    {
        let mut ctx = Score {
            script,
            session,
            weights,
            tally,
            steps,
            unused: &mut unused,
        };
        for turn in &session.turns {
            for claim in &turn.claims {
                let speaker = claim.by.unwrap_or(turn.role);
                score_claim(&mut ctx, speaker, claim)?;
            }
        }
        for claim in &session.claims {
            // A transcript claim has no turn. `by` overrides; otherwise the
            // claim is graded as user evidence.
            let speaker = claim.by.unwrap_or(Speaker::User);
            score_claim(&mut ctx, speaker, claim)?;
        }
        score_noise(&mut ctx);
    }
    for (effect, taken) in &unused {
        if !taken {
            tally.false_positive(effect.op());
        }
    }
    Ok(())
}

fn score_claim(ctx: &mut Score<'_>, speaker: Speaker, claim: &Claim) -> Result<(), RecallError> {
    match claim.op {
        ClaimOp::Create => score_create(ctx, claim),
        ClaimOp::Corroborate => score_corroborate(ctx, speaker, claim),
        ClaimOp::Supersede => score_supersede(ctx, claim),
        ClaimOp::Contradict => score_contradict(ctx, speaker, claim),
        ClaimOp::Reject => score_reject(ctx, claim),
    }
}

fn score_create(ctx: &mut Score<'_>, claim: &Claim) -> Result<(), RecallError> {
    let key = fact_key(ctx.script.fact(&claim.fact)?);
    let matched = take(
        ctx.unused,
        |effect| matches!(effect, Effect::Created { key: got } if *got == key),
    );
    ctx.record(&claim.fact, WriteOp::Create, WriteOp::Create, matched);
    if !matched {
        // The miss is already counted. Nothing appeared, so the step is a no-op.
        if let Some(step) = ctx.steps.last_mut() {
            step.actual = WriteOp::NoOp;
        }
    }
    Ok(())
}

fn score_corroborate(
    ctx: &mut Score<'_>,
    speaker: Speaker,
    claim: &Claim,
) -> Result<(), RecallError> {
    let key = fact_key(ctx.script.fact(&claim.fact)?);
    let weights = ctx.weights;
    let matched = take(ctx.unused, |effect| match effect {
        Effect::Corroborated {
            key: got,
            delta_alpha,
            delta_self,
            delta_beta,
        } if *got == key => {
            corroboration_fits(*delta_alpha, *delta_self, *delta_beta, speaker, weights)
        }
        _ => false,
    });
    // A corroboration at the wrong provenance weight is still a corroboration.
    // It stays untaken so the leftover pass counts it as a false positive.
    let wrong_weight = !matched
        && ctx.unused.iter().any(|(effect, taken)| {
            !taken && matches!(effect, Effect::Corroborated { key: got, .. } if *got == key)
        });
    let actual = if matched || wrong_weight {
        WriteOp::Corroborate
    } else {
        WriteOp::NoOp
    };
    ctx.record(&claim.fact, WriteOp::Corroborate, actual, matched);
    Ok(())
}

fn score_supersede(ctx: &mut Score<'_>, claim: &Claim) -> Result<(), RecallError> {
    let old = fact_key(ctx.script.fact(&claim.fact)?);
    let replacement = claim.replacement.as_deref().ok_or_else(|| {
        RecallError::Config(format!("supersede of '{}' has no replacement", claim.fact))
    })?;
    let new = fact_key(ctx.script.fact(replacement)?);
    let created = take(
        ctx.unused,
        |effect| matches!(effect, Effect::Created { key } if *key == new),
    );
    let retired = take(
        ctx.unused,
        |effect| matches!(effect, Effect::Retired { key } if *key == old),
    );
    let matched = created && retired;
    // The new edge is the replacement half of the supersede, so a failure
    // that still created it is a create, not an extra false create.
    let actual = if matched {
        WriteOp::Supersede
    } else if created {
        WriteOp::Create
    } else if retired {
        WriteOp::Supersede
    } else {
        WriteOp::NoOp
    };
    let label = format!("{}->{replacement}", claim.fact);
    ctx.record(&label, WriteOp::Supersede, actual, matched);
    Ok(())
}

fn score_contradict(
    ctx: &mut Score<'_>,
    speaker: Speaker,
    claim: &Claim,
) -> Result<(), RecallError> {
    let key = fact_key(ctx.script.fact(&claim.fact)?);
    let weight = speaker_weight(speaker, ctx.weights);
    let matched = take(ctx.unused, |effect| match effect {
        Effect::Contradicted {
            key: got,
            delta_beta,
        } if *got == key => approx(*delta_beta, weight),
        _ => false,
    });
    let corroborated = !matched
        && ctx.unused.iter().any(|(effect, taken)| {
            !taken && matches!(effect, Effect::Corroborated { key: got, .. } if *got == key)
        });
    let actual = if matched {
        WriteOp::Contradict
    } else if corroborated {
        WriteOp::Corroborate
    } else {
        WriteOp::NoOp
    };
    ctx.record(&claim.fact, WriteOp::Contradict, actual, matched);
    Ok(())
}

fn score_reject(ctx: &mut Score<'_>, claim: &Claim) -> Result<(), RecallError> {
    let key = fact_key(ctx.script.fact(&claim.fact)?);
    let created = take(
        ctx.unused,
        |effect| matches!(effect, Effect::Created { key: got } if *got == key),
    );
    if created {
        ctx.tally.false_positive(WriteOp::Create);
        ctx.tally.miss(WriteOp::NoOp);
        ctx.steps.push(StepLog {
            session: ctx.session.id.clone(),
            fact: claim.fact.clone(),
            expected: WriteOp::NoOp,
            actual: WriteOp::Create,
            matched: false,
        });
    } else {
        ctx.tally.hit(WriteOp::NoOp);
        ctx.steps.push(StepLog {
            session: ctx.session.id.clone(),
            fact: claim.fact.clone(),
            expected: WriteOp::NoOp,
            actual: WriteOp::NoOp,
            matched: true,
        });
    }
    Ok(())
}

fn score_noise(ctx: &mut Score<'_>) {
    let noise: Vec<&ScriptTurn> = ctx.session.turns.iter().filter(|turn| turn.noise).collect();
    if noise.is_empty() {
        return;
    }
    let leftover = ctx
        .unused
        .iter()
        .find(|(_, taken)| !taken)
        .map(|(effect, _)| effect.op());
    let session_id = ctx.session.id.clone();
    for _turn in noise {
        let matched = leftover.is_none();
        if matched {
            ctx.tally.hit(WriteOp::NoOp);
        } else {
            ctx.tally.miss(WriteOp::NoOp);
        }
        ctx.steps.push(StepLog {
            session: session_id.clone(),
            fact: "noise".to_string(),
            expected: WriteOp::NoOp,
            actual: leftover.unwrap_or(WriteOp::NoOp),
            matched,
        });
    }
}

fn take(unused: &mut [(Effect, bool)], pred: impl Fn(&Effect) -> bool) -> bool {
    for (effect, taken) in unused.iter_mut() {
        if *taken || !pred(effect) {
            continue;
        }
        *taken = true;
        return true;
    }
    false
}

fn corroboration_fits(
    delta_alpha: f64,
    delta_self: i64,
    delta_beta: f64,
    speaker: Speaker,
    weights: &ProvenanceWeights,
) -> bool {
    if delta_beta.abs() > EPS {
        return false;
    }
    let weight = speaker_weight(speaker, weights);
    match speaker {
        // One claim is one reinforcement. A larger jump is a different event.
        Speaker::Assistant => delta_self == 1 && approx(delta_alpha, weight),
        Speaker::User => delta_self == 0 && approx(delta_alpha, weight),
    }
}

fn speaker_weight(speaker: Speaker, weights: &ProvenanceWeights) -> f64 {
    match speaker {
        Speaker::User => weights.weight_user,
        Speaker::Assistant => weights.weight_self,
    }
}

fn diff(before: &[ObservedEdge], after: &[ObservedEdge]) -> Vec<Effect> {
    let prev: HashMap<&str, &ObservedEdge> =
        before.iter().map(|edge| (edge.id.as_str(), edge)).collect();
    let still_there: HashSet<&str> = after.iter().map(|edge| edge.id.as_str()).collect();
    let mut effects = Vec::new();
    for edge in before {
        if !still_there.contains(edge.id.as_str()) {
            effects.push(Effect::Removed);
        }
    }
    for edge in after {
        let key = edge.key();
        match prev.get(edge.id.as_str()) {
            None => effects.push(Effect::Created { key }),
            Some(old) => push_update(&mut effects, old, edge, key),
        }
    }
    effects
}

fn push_update(effects: &mut Vec<Effect>, old: &ObservedEdge, edge: &ObservedEdge, key: EdgeKey) {
    if !old.live && edge.live {
        effects.push(Effect::Revived);
    }
    if old.live && !edge.live {
        effects.push(Effect::Retired { key });
        return;
    }
    let delta_alpha = edge.alpha - old.alpha;
    let delta_beta = edge.beta - old.beta;
    let delta_self = edge.self_reinforcements - old.self_reinforcements;
    if delta_beta > EPS {
        effects.push(Effect::Contradicted { key, delta_beta });
        return;
    }
    if delta_alpha > EPS || delta_self > 0 {
        effects.push(Effect::Corroborated {
            key,
            delta_alpha,
            delta_self,
            delta_beta,
        });
    }
}

fn end_state(
    script: &FactScript,
    edges: &[ObservedEdge],
    weights: &ProvenanceWeights,
) -> Result<Vec<WrongFact>, RecallError> {
    let mut wrong = Vec::new();
    for expected in &script.expect {
        let key = fact_key(script.fact(&expected.fact)?);
        let matching: Vec<&ObservedEdge> = edges.iter().filter(|edge| edge.key() == key).collect();
        let live = matching.iter().any(|edge| edge.live);
        if expected.live && !live {
            wrong.push(WrongFact {
                fact: expected.fact.clone(),
                reason: WrongReason::Missing,
            });
        }
        if !expected.live && live {
            wrong.push(WrongFact {
                fact: expected.fact.clone(),
                reason: WrongReason::StillLive,
            });
        }
        if provenance_miss(expected, matching.as_slice(), weights) {
            wrong.push(WrongFact {
                fact: expected.fact.clone(),
                reason: WrongReason::Provenance,
            });
        }
    }
    Ok(wrong)
}

fn provenance_miss(
    expected: &super::script::ExpectedFact,
    matching: &[&ObservedEdge],
    weights: &ProvenanceWeights,
) -> bool {
    if expected.provenance.is_none() && expected.self_reinforcements.is_none() {
        return false;
    }
    let Some(edge) = matching
        .iter()
        .max_by_key(|edge| (edge.live, edge.self_reinforcements))
    else {
        return true;
    };
    let tally_ok = expected
        .self_reinforcements
        .is_none_or(|n| edge.self_reinforcements == n);
    let role_ok = expected.provenance.is_none_or(|role| match role {
        EndProvenance::User => edge.self_reinforcements == 0,
        EndProvenance::Assistant => edge.self_reinforcements >= 1,
        // Mixed is both sides. Self-reinforcement alone is Assistant.
        EndProvenance::Mixed => edge.self_reinforcements >= 1 && has_user_evidence(edge, weights),
    });
    !tally_ok || !role_ok
}

/// Create stores the extraction prior and does not apply provenance. User
/// evidence is whatever alpha remains after that prior and the self-weight
/// tally are removed.
fn has_user_evidence(edge: &ObservedEdge, weights: &ProvenanceWeights) -> bool {
    let steps = u32::try_from(edge.self_reinforcements.max(0)).unwrap_or(0);
    let self_alpha = weights.weight_self * f64::from(steps);
    edge.alpha - edge.prior_alpha - self_alpha >= weights.weight_user - EPS
}

fn junk(script: &FactScript, edges: &[ObservedEdge]) -> Result<Vec<JunkEdge>, RecallError> {
    let known: HashSet<EdgeKey> = script.facts.iter().map(fact_key).collect();
    Ok(edges
        .iter()
        .filter(|edge| !known.contains(&edge.key()))
        .map(|edge| JunkEdge {
            subject: edge.subject.clone(),
            relation: edge.relation.clone(),
            object: edge.object.clone(),
        })
        .collect())
}

fn questions(
    script: &FactScript,
    edges: &[ObservedEdge],
    keyword_hits: &[bool],
) -> Result<Vec<QuestionScore>, RecallError> {
    let mut scores = Vec::with_capacity(script.questions.len());
    for (question, keyword_hit) in script.questions.iter().zip(keyword_hits) {
        let needs_live = question.needs.iter().try_fold(true, |ok, id| {
            Ok::<bool, RecallError>(ok && fact_live(script, edges, id)?)
        })?;
        let absent_clear = question.absent.iter().try_fold(true, |ok, id| {
            Ok::<bool, RecallError>(ok && !fact_live(script, edges, id)?)
        })?;
        scores.push(QuestionScore {
            text: question.text.clone(),
            keyword_hit: *keyword_hit,
            graph_hit: needs_live && absent_clear,
        });
    }
    Ok(scores)
}

fn fact_live(script: &FactScript, edges: &[ObservedEdge], id: &str) -> Result<bool, RecallError> {
    let key = fact_key(script.fact(id)?);
    Ok(edges.iter().any(|edge| edge.live && edge.key() == key))
}

fn fact_key(fact: &ScriptFact) -> EdgeKey {
    (
        fold(&fact.subject),
        fold(&fact.relation),
        fold(&fact.object),
    )
}

impl ObservedEdge {
    fn key(&self) -> EdgeKey {
        (
            fold(&self.subject),
            fold(&self.relation),
            fold(&self.object),
        )
    }
}

fn fold(text: &str) -> String {
    text.trim().to_lowercase()
}

fn approx(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= EPS
}

fn ratio(numerator: u32, denominator: u32) -> Option<f64> {
    (denominator > 0).then(|| f64::from(numerator) / f64::from(denominator))
}

/// Pattern score for `script`, or `None` when no fact has a match block.
///
/// Records are `subject | relation | object`. Live and retired records are
/// both visible to `found`; stale and noise checks use live records only.
/// A rejected fact is found or missing: there is no rejection rule yet.
fn score_patterns(
    script: &FactScript,
    sessions: &[SessionObservation],
) -> Result<Option<PatternScore>, RecallError> {
    if script.facts.iter().all(|fact| fact.pattern.is_none()) {
        return Ok(None);
    }
    let final_edges = sessions
        .last()
        .map(|session| session.after.as_slice())
        .unwrap_or(&[]);
    let mut found = Vec::new();
    let mut complete = Vec::new();
    let mut wrong = Vec::new();
    let mut missing = Vec::new();
    let mut compiled = Vec::new();
    for fact in &script.facts {
        let Some(pattern) = &fact.pattern else {
            continue;
        };
        let core = compile_all(&pattern.core_all)?;
        let detail = compile_all(&pattern.detail_all)?;
        let wrong_any = compile_all(&pattern.wrong_any)?;
        let hits: Vec<&ObservedEdge> = final_edges
            .iter()
            .filter(|edge| matches_all(&core, &record_line(edge)))
            .collect();
        if hits.is_empty() {
            missing.push(fact.id.clone());
        } else {
            found.push(fact.id.clone());
            if hits
                .iter()
                .any(|edge| matches_all(&detail, &record_line(edge)))
            {
                complete.push(fact.id.clone());
            }
        }
        if fact_is_wrong(fact, &hits, final_edges, &core, &wrong_any) {
            wrong.push(fact.id.clone());
        }
        compiled.push(core);
    }

    let mut retired = Vec::new();
    let mut premature = Vec::new();
    for pair in &script.superseded_pairs {
        let old = script.fact(&pair.old)?;
        let Some(pattern) = &old.pattern else {
            continue;
        };
        let core = compile_all(&pattern.core_all)?;
        let Some(when) = retirement_index(script, sessions, &core)? else {
            continue;
        };
        let at = session_index(script, &pair.retire_in)?;
        let label = format!("{}->{}", pair.old, pair.new);
        if when < at {
            premature.push(label);
        } else {
            retired.push(label);
        }
    }

    let live_lines: Vec<String> = final_edges
        .iter()
        .filter(|edge| edge.live)
        .map(record_line)
        .collect();
    let mut noise_hits = Vec::new();
    for item in &script.noise {
        let patterns = compile_all(&item.match_any)?;
        if live_lines.iter().any(|line| matches_any(&patterns, line)) {
            noise_hits.push(item.id.clone());
        }
    }
    let unmatched = live_lines
        .into_iter()
        .filter(|line| !compiled.iter().any(|core| matches_all(core, line)))
        .collect();

    Ok(Some(PatternScore {
        facts: count_of(
            script
                .facts
                .iter()
                .filter(|fact| fact.pattern.is_some())
                .count(),
        ),
        pairs: count_of(script.superseded_pairs.len()),
        noise: count_of(script.noise.len()),
        found: bucket(found),
        complete: bucket(complete),
        wrong: bucket(wrong),
        missing: bucket(missing),
        retired: bucket(retired),
        premature: bucket(premature),
        noise_hits: bucket(noise_hits),
        unmatched: bucket(unmatched),
    }))
}

fn fact_is_wrong(
    fact: &ScriptFact,
    hits: &[&ObservedEdge],
    edges: &[ObservedEdge],
    core: &[regex::Regex],
    wrong_any: &[regex::Regex],
) -> bool {
    match fact.end_state {
        // A live fact is wrong when one of its own live records matches a
        // wrong pattern. Another fact's record does not count.
        EndState::Live => hits
            .iter()
            .any(|edge| edge.live && matches_any(wrong_any, &record_line(edge))),
        // Strict stale rule: a superseded fact is still wrong while a live
        // record matches its core.
        EndState::Superseded => edges
            .iter()
            .any(|edge| edge.live && matches_all(core, &record_line(edge))),
        EndState::Rejected => false,
    }
}

/// Session index at which the old fact's live records disappear for good.
///
/// `None` when a live record still asserts it at the end, or it was never live.
fn retirement_index(
    script: &FactScript,
    sessions: &[SessionObservation],
    core: &[regex::Regex],
) -> Result<Option<usize>, RecallError> {
    let mut seen_live = false;
    let mut cleared: Option<usize> = None;
    for (index, session) in script.sessions.iter().enumerate() {
        let observed = sessions
            .iter()
            .find(|item| item.id == session.id)
            .ok_or_else(|| {
                RecallError::Config(format!("missing observation for session '{}'", session.id))
            })?;
        let before_live = any_live(&observed.before, core);
        let after_live = any_live(&observed.after, core);
        if after_live {
            seen_live = true;
            cleared = None;
            continue;
        }
        if before_live || (!seen_live && newly_retired(observed, core)) {
            seen_live = true;
            cleared = Some(index);
        }
    }
    Ok(cleared)
}

fn newly_retired(observed: &SessionObservation, core: &[regex::Regex]) -> bool {
    let before: HashSet<&str> = observed
        .before
        .iter()
        .map(|edge| edge.id.as_str())
        .collect();
    observed.after.iter().any(|edge| {
        !edge.live && !before.contains(edge.id.as_str()) && matches_all(core, &record_line(edge))
    })
}

fn any_live(edges: &[ObservedEdge], core: &[regex::Regex]) -> bool {
    edges
        .iter()
        .any(|edge| edge.live && matches_all(core, &record_line(edge)))
}

fn session_index(script: &FactScript, id: &str) -> Result<usize, RecallError> {
    script
        .sessions
        .iter()
        .position(|session| session.id == id)
        .ok_or_else(|| RecallError::Config(format!("unknown session '{id}'")))
}

fn record_line(edge: &ObservedEdge) -> String {
    format!("{} | {} | {}", edge.subject, edge.relation, edge.object)
}

fn matches_all(patterns: &[regex::Regex], line: &str) -> bool {
    patterns.iter().all(|pattern| pattern.is_match(line))
}

fn matches_any(patterns: &[regex::Regex], line: &str) -> bool {
    patterns.iter().any(|pattern| pattern.is_match(line))
}

fn compile_all(sources: &[String]) -> Result<Vec<regex::Regex>, RecallError> {
    sources
        .iter()
        .map(|source| {
            compile_pattern(source)
                .map_err(|err| RecallError::Config(format!("regex '{source}': {err}")))
        })
        .collect()
}

fn bucket(mut ids: Vec<String>) -> PatternBucket {
    ids.sort();
    PatternBucket {
        count: count_of(ids.len()),
        ids,
    }
}

fn count_of(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::super::script::{
        Claim, ClaimOp, DerivedQuestion, EndProvenance, EndState, ExpectedFact, FactPattern,
        FactScript, ScriptFact, ScriptSession, ScriptTurn, Speaker,
    };
    use super::*;

    fn weights() -> ProvenanceWeights {
        ProvenanceWeights::default()
    }

    fn fact(id: &str, object: &str) -> ScriptFact {
        ScriptFact {
            id: id.to_string(),
            subject: "deploy".to_string(),
            relation: "setting".to_string(),
            object: object.to_string(),
            subject_type: "project".to_string(),
            object_type: "concept".to_string(),
            subject_abstract: None,
            object_abstract: None,
            end_state: EndState::Live,
            superseded_in: None,
            pattern: None,
        }
    }

    fn turn(role: Speaker, text: &str, claims: Vec<Claim>, noise: bool) -> ScriptTurn {
        ScriptTurn {
            role,
            text: text.to_string(),
            claims,
            noise,
        }
    }

    fn claim(op: ClaimOp, fact_id: &str, replacement: Option<&str>) -> Claim {
        Claim {
            op,
            fact: fact_id.to_string(),
            replacement: replacement.map(str::to_string),
            by: None,
            quote: None,
        }
    }

    fn session(id: &str, turn: ScriptTurn) -> ScriptSession {
        ScriptSession {
            id: id.to_string(),
            date: "2026-01-01T00:00:00Z".to_string(),
            turns: vec![turn],
            transcript: None,
            transcript_path: None,
            claims: Vec::new(),
        }
    }

    fn edge(
        id: &str,
        object: &str,
        live: bool,
        alpha: f64,
        beta: f64,
        self_reinforcements: i64,
    ) -> ObservedEdge {
        ObservedEdge {
            id: id.to_string(),
            subject: "deploy".to_string(),
            relation: "setting".to_string(),
            object: object.to_string(),
            live,
            alpha,
            beta,
            self_reinforcements,
            // Explicit. The scripted extractor always starts here.
            prior_alpha: 9.0,
        }
    }

    fn obs(id: &str, before: Vec<ObservedEdge>, after: Vec<ObservedEdge>) -> SessionObservation {
        SessionObservation {
            id: id.to_string(),
            before,
            after,
            calls: 1,
            millis: 5,
            warnings: Vec::new(),
        }
    }

    fn history() -> FactScript {
        FactScript {
            name: "host".to_string(),
            project: None,
            cwd: None,
            sessions: vec![
                session(
                    "s1",
                    turn(
                        Speaker::User,
                        "create pine",
                        vec![claim(ClaimOp::Create, "host", None)],
                        false,
                    ),
                ),
                session(
                    "s2",
                    turn(
                        Speaker::User,
                        "repeat pine",
                        vec![claim(ClaimOp::Corroborate, "host", None)],
                        false,
                    ),
                ),
                session(
                    "s3",
                    turn(
                        Speaker::User,
                        "change to oak",
                        vec![claim(ClaimOp::Supersede, "host", Some("oak"))],
                        false,
                    ),
                ),
                session(
                    "s4",
                    turn(
                        Speaker::User,
                        "not oak",
                        vec![claim(ClaimOp::Contradict, "oak", None)],
                        false,
                    ),
                ),
                session(
                    "s5",
                    turn(
                        Speaker::User,
                        "reject birch",
                        vec![claim(ClaimOp::Reject, "birch", None)],
                        false,
                    ),
                ),
                session("s6", turn(Speaker::User, "kettle", vec![], true)),
            ],
            facts: vec![
                fact("host", "pine"),
                fact("oak", "oak"),
                fact("birch", "birch"),
            ],
            expect: vec![
                ExpectedFact {
                    fact: "host".into(),
                    live: false,
                    provenance: Some(EndProvenance::User),
                    self_reinforcements: Some(0),
                },
                ExpectedFact {
                    fact: "oak".into(),
                    live: true,
                    provenance: Some(EndProvenance::User),
                    self_reinforcements: Some(0),
                },
                ExpectedFact {
                    fact: "birch".into(),
                    live: false,
                    provenance: None,
                    self_reinforcements: None,
                },
            ],
            questions: vec![DerivedQuestion {
                text: "what changed about the host".into(),
                answer: "oak".into(),
                needs: vec!["oak".into()],
                absent: vec!["host".into()],
            }],
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        }
    }

    fn perfect_observations(user_weight: f64) -> Vec<SessionObservation> {
        let pine = |live, alpha, beta| edge("pine", "pine", live, alpha, beta, 0);
        let oak = |live, alpha, beta| edge("oak", "oak", live, alpha, beta, 0);
        let created = pine(true, 9.0, 1.0);
        let corroborated = pine(true, 9.0 + user_weight, 1.0);
        let retired = pine(false, 9.0 + user_weight, 1.0);
        let replacement = oak(true, 9.0, 1.0);
        let contradicted = oak(true, 9.0, 1.0 + user_weight);
        vec![
            obs("s1", vec![], vec![created.clone()]),
            obs("s2", vec![created], vec![corroborated.clone()]),
            obs(
                "s3",
                vec![corroborated],
                vec![retired.clone(), replacement.clone()],
            ),
            obs(
                "s4",
                vec![retired.clone(), replacement],
                vec![retired.clone(), contradicted.clone()],
            ),
            obs(
                "s5",
                vec![retired.clone(), contradicted.clone()],
                vec![retired.clone(), contradicted.clone()],
            ),
            obs(
                "s6",
                vec![retired.clone(), contradicted.clone()],
                vec![retired, contradicted],
            ),
        ]
    }

    fn grade_history(sessions: Vec<SessionObservation>, keyword: bool) -> WriteReport {
        grade(
            &history(),
            &sessions,
            &weights(),
            &[keyword],
            ExtractorLabel::Scripted,
        )
        .unwrap()
    }

    #[test]
    fn a_graph_that_followed_the_script_scores_perfectly() {
        let user = weights().weight_user;
        let report = grade_history(perfect_observations(user), true);
        assert_eq!(report.operations.create, full(1));
        assert_eq!(report.operations.corroborate, full(1));
        assert_eq!(report.operations.supersede, full(1));
        assert_eq!(report.operations.contradict, full(1));
        assert_eq!(report.operations.no_op, full(2));
        assert!(report.wrong_facts.is_empty());
        assert!(report.junk_edges.is_empty());
        assert!(report.steps.iter().all(|step| step.matched));
        assert!(report.questions[0].graph_hit);
        assert!(report.questions[0].keyword_hit);
        assert!(report.patterns.is_none());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("patterns"), "{json}");
    }

    #[test]
    fn a_replacement_that_leaves_the_old_edge_live_misses_supersede() {
        let user = weights().weight_user;
        let mut sessions = perfect_observations(user);
        let mut after = sessions[2].after.clone();
        after[0].live = true;
        sessions[2].after = after.clone();
        sessions[3].before = after.clone();
        sessions[3].after[0].live = true;
        sessions[4].before[0].live = true;
        sessions[4].after[0].live = true;
        sessions[5].before[0].live = true;
        sessions[5].after[0].live = true;

        let report = grade_history(sessions, true);
        assert_eq!(report.operations.supersede.tp, 0);
        assert_eq!(report.operations.supersede.missed, 1);
        assert_eq!(report.operations.supersede.precision, None);
        assert_eq!(report.operations.supersede.recall, Some(0.0));
        let step = report
            .steps
            .iter()
            .find(|step| step.expected == WriteOp::Supersede)
            .unwrap();
        assert!(!step.matched);
        assert_eq!(step.actual, WriteOp::Create);
        assert!(report
            .wrong_facts
            .iter()
            .any(|wrong| { wrong.fact == "host" && wrong.reason == WrongReason::StillLive }));
        assert!(!report.questions[0].graph_hit);
    }

    #[test]
    fn an_unknown_edge_is_junk_and_a_false_create() {
        let user = weights().weight_user;
        let mut sessions = perfect_observations(user);
        let mut after = sessions[5].after.clone();
        after.push(ObservedEdge {
            id: "junk".into(),
            subject: "weather".into(),
            relation: "mentions".into(),
            object: "kettle".into(),
            live: true,
            alpha: 9.0,
            beta: 1.0,
            self_reinforcements: 0,
            prior_alpha: 9.0,
        });
        sessions[5].after = after;

        let report = grade_history(sessions, true);
        assert_eq!(report.junk_edges.len(), 1);
        assert_eq!(report.junk_edges[0].object, "kettle");
        assert_eq!(report.operations.create.tp, 1);
        assert_eq!(report.operations.create.fp, 1);
        assert_eq!(report.operations.create.precision, Some(0.5));
        assert_eq!(report.operations.no_op.tp, 1);
        assert_eq!(report.operations.no_op.missed, 1);
    }

    #[test]
    fn a_self_reinforcement_does_not_count_as_user_corroboration() {
        let script = FactScript {
            name: "prov".into(),
            sessions: vec![
                session(
                    "s1",
                    turn(
                        Speaker::User,
                        "create",
                        vec![claim(ClaimOp::Create, "host", None)],
                        false,
                    ),
                ),
                session(
                    "s2",
                    turn(
                        Speaker::User,
                        "repeat",
                        vec![claim(ClaimOp::Corroborate, "host", None)],
                        false,
                    ),
                ),
            ],
            facts: vec![fact("host", "pine")],
            expect: vec![ExpectedFact {
                fact: "host".into(),
                live: true,
                provenance: Some(EndProvenance::User),
                self_reinforcements: Some(0),
            }],
            questions: vec![],
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        };
        let created = edge("pine", "pine", true, 9.0, 1.0, 0);
        let restated = edge("pine", "pine", true, 9.0 + weights().weight_self, 1.0, 1);
        let sessions = vec![
            obs("s1", vec![], vec![created.clone()]),
            obs("s2", vec![created], vec![restated]),
        ];
        let report = grade(
            &script,
            &sessions,
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        assert_eq!(report.operations.corroborate.tp, 0);
        assert_eq!(report.operations.corroborate.missed, 1);
        assert_eq!(report.operations.corroborate.fp, 1);
        assert_eq!(report.operations.corroborate.precision, Some(0.0));
        assert_eq!(report.operations.corroborate.recall, Some(0.0));
    }

    #[test]
    fn an_assistant_restatement_matches_the_self_weight() {
        let script = FactScript {
            name: "self".into(),
            sessions: vec![
                session(
                    "s1",
                    turn(
                        Speaker::User,
                        "create",
                        vec![claim(ClaimOp::Create, "host", None)],
                        false,
                    ),
                ),
                session(
                    "s2",
                    turn(
                        Speaker::Assistant,
                        "note",
                        vec![claim(ClaimOp::Corroborate, "host", None)],
                        false,
                    ),
                ),
            ],
            facts: vec![fact("host", "pine")],
            expect: vec![ExpectedFact {
                fact: "host".into(),
                live: true,
                provenance: Some(EndProvenance::Assistant),
                self_reinforcements: Some(1),
            }],
            questions: vec![],
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        };
        let created = edge("pine", "pine", true, 9.0, 1.0, 0);
        let restated = edge("pine", "pine", true, 9.0 + weights().weight_self, 1.0, 1);
        let sessions = vec![
            obs("s1", vec![], vec![created.clone()]),
            obs("s2", vec![created], vec![restated]),
        ];
        let report = grade(
            &script,
            &sessions,
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        assert_eq!(report.operations.corroborate, full(1));
        assert!(report.wrong_facts.is_empty());
    }

    #[test]
    fn two_self_reinforcements_are_not_one_assistant_claim() {
        let script = FactScript {
            name: "self".into(),
            sessions: vec![session(
                "s1",
                turn(
                    Speaker::Assistant,
                    "note",
                    vec![claim(ClaimOp::Corroborate, "host", None)],
                    false,
                ),
            )],
            facts: vec![fact("host", "pine")],
            expect: vec![ExpectedFact {
                fact: "host".into(),
                live: true,
                provenance: None,
                self_reinforcements: None,
            }],
            questions: vec![],
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        };
        let before = edge("pine", "pine", true, 9.0, 1.0, 0);
        let after = edge(
            "pine",
            "pine",
            true,
            9.0 + 2.0 * weights().weight_self,
            1.0,
            2,
        );
        let report = grade(
            &script,
            &[obs("s1", vec![before], vec![after])],
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        assert_eq!(report.operations.corroborate.tp, 0);
        assert_eq!(report.operations.corroborate.fp, 1);
        assert_eq!(report.operations.corroborate.missed, 1);
    }

    #[test]
    fn a_deleted_edge_and_a_revived_edge_are_false_positives() {
        let script = FactScript {
            name: "gone".into(),
            sessions: vec![
                session("s1", turn(Speaker::User, "quiet", vec![], true)),
                session("s2", turn(Speaker::User, "again", vec![], true)),
            ],
            facts: vec![fact("host", "pine")],
            expect: vec![ExpectedFact {
                fact: "host".into(),
                live: true,
                provenance: None,
                self_reinforcements: None,
            }],
            questions: vec![],
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        };
        let dead = edge("pine", "pine", false, 9.0, 1.0, 0);
        let live = edge("pine", "pine", true, 9.0, 1.0, 0);
        let report = grade(
            &script,
            &[
                obs("s1", vec![live.clone()], vec![]),
                obs("s2", vec![dead.clone()], vec![live]),
            ],
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        assert_eq!(report.operations.removed.fp, 1);
        assert_eq!(report.operations.removed.tp, 0);
        assert_eq!(report.operations.revived.fp, 1);
        assert_eq!(report.operations.revived.tp, 0);
    }

    #[test]
    fn an_assistant_only_edge_fails_mixed() {
        let script = FactScript {
            name: "mixed".into(),
            sessions: vec![session(
                "s1",
                turn(
                    Speaker::User,
                    "create",
                    vec![claim(ClaimOp::Create, "host", None)],
                    false,
                ),
            )],
            facts: vec![fact("host", "pine")],
            expect: vec![ExpectedFact {
                fact: "host".into(),
                live: true,
                provenance: Some(EndProvenance::Mixed),
                self_reinforcements: Some(1),
            }],
            questions: vec![],
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
        };
        let only_self = edge("pine", "pine", true, 9.0 + weights().weight_self, 1.0, 1);
        let report = grade(
            &script,
            &[obs("s1", vec![], vec![only_self])],
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        assert!(report
            .wrong_facts
            .iter()
            .any(|wrong| wrong.reason == WrongReason::Provenance));

        let both = edge(
            "pine",
            "pine",
            true,
            9.0 + weights().weight_user + weights().weight_self,
            1.0,
            1,
        );
        let report = grade(
            &script,
            &[obs("s1", vec![], vec![both])],
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        assert!(report.wrong_facts.is_empty());
    }

    #[test]
    fn an_authoritative_edge_with_self_only_fails_mixed() {
        let mut only_self = edge("pine", "pine", true, 10.0 + weights().weight_self, 0.0, 1);
        only_self.prior_alpha = 10.0;
        let report = grade_mixed(only_self);
        assert!(report
            .wrong_facts
            .iter()
            .any(|wrong| wrong.reason == WrongReason::Provenance));
    }

    #[test]
    fn an_inferred_edge_with_user_and_self_passes_mixed() {
        let weights = weights();
        let mut both = edge(
            "pine",
            "pine",
            true,
            6.0 + weights.weight_user + weights.weight_self,
            4.0,
            1,
        );
        both.prior_alpha = 6.0;
        let report = grade_mixed(both);
        assert!(report.wrong_facts.is_empty());
    }

    fn grade_mixed(edge: ObservedEdge) -> WriteReport {
        let script = FactScript {
            name: "mixed".into(),
            project: None,
            cwd: None,
            superseded_pairs: Vec::new(),
            noise: Vec::new(),
            sessions: vec![session(
                "s1",
                turn(
                    Speaker::User,
                    "create",
                    vec![claim(ClaimOp::Create, "host", None)],
                    false,
                ),
            )],
            facts: vec![fact("host", "pine")],
            expect: vec![ExpectedFact {
                fact: "host".into(),
                live: true,
                provenance: Some(EndProvenance::Mixed),
                self_reinforcements: Some(1),
            }],
            questions: vec![],
        };
        grade(
            &script,
            &[obs("s1", vec![], vec![edge])],
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap()
    }

    #[test]
    fn a_session_count_mismatch_is_rejected() {
        let err = grade(&history(), &[], &weights(), &[], ExtractorLabel::Scripted).unwrap_err();
        assert!(err.to_string().contains("session observations"), "{err}");
    }

    #[test]
    fn pattern_scorer_counts_found_complete_wrong_stale_premature_noise() {
        let script = pattern_script();
        let report = grade(
            &script,
            &pattern_observations(),
            &weights(),
            &[],
            ExtractorLabel::Scripted,
        )
        .unwrap();
        let patterns = report.patterns.expect("pattern section");
        assert_eq!(patterns.facts, 8);
        assert_eq!(patterns.pairs, 3);
        assert_eq!(patterns.noise, 2);
        assert_eq!(
            patterns.found.ids,
            ids(&["alpha", "beta", "delta", "epsilon", "gamma", "rho", "zeta"])
        );
        assert_eq!(patterns.found.count, 7);
        assert_eq!(
            patterns.complete.ids,
            ids(&["alpha", "delta", "epsilon", "gamma", "rho", "zeta"])
        );
        // gamma matches wrong_any; epsilon is superseded and still live.
        assert_eq!(patterns.wrong.ids, ids(&["epsilon", "gamma"]));
        assert_eq!(patterns.missing.ids, ids(&["absent"]));
        assert_eq!(patterns.retired.ids, ids(&["delta->delta2"]));
        assert_eq!(patterns.premature.ids, ids(&["zeta->zeta2"]));
        assert_eq!(patterns.noise_hits.ids, ids(&["N1"]));
        assert_eq!(
            patterns.unmatched.ids,
            ids(&["deploy | setting | leftover"])
        );
        // Rejected rho is found, not wrong.
        assert!(!patterns.wrong.ids.iter().any(|id| id == "rho"));
    }

    fn pattern_script() -> FactScript {
        FactScript {
            name: "patterns".into(),
            project: None,
            cwd: None,
            sessions: vec![
                session("s1", turn(Speaker::User, "s1", vec![], false)),
                session("s2", turn(Speaker::User, "s2", vec![], false)),
            ],
            facts: vec![
                patterned(
                    "alpha",
                    "alpha detail",
                    EndState::Live,
                    None,
                    &["alpha"],
                    &["detail"],
                    &[],
                ),
                patterned(
                    "beta",
                    "beta",
                    EndState::Live,
                    None,
                    &["beta"],
                    &["extra"],
                    &[],
                ),
                patterned(
                    "gamma",
                    "gamma evil",
                    EndState::Live,
                    None,
                    &["gamma"],
                    &[],
                    &["evil"],
                ),
                patterned(
                    "delta",
                    "delta",
                    EndState::Superseded,
                    Some("s2"),
                    &["delta"],
                    &[],
                    &[],
                ),
                patterned(
                    "epsilon",
                    "epsilon",
                    EndState::Superseded,
                    Some("s2"),
                    &["epsilon"],
                    &[],
                    &[],
                ),
                patterned(
                    "zeta",
                    "zeta",
                    EndState::Superseded,
                    Some("s2"),
                    &["zeta"],
                    &[],
                    &[],
                ),
                patterned("rho", "rho", EndState::Rejected, None, &["rho"], &[], &[]),
                patterned(
                    "absent",
                    "absent",
                    EndState::Live,
                    None,
                    &["absent"],
                    &[],
                    &[],
                ),
                fact("delta2", "next"),
                fact("epsilon2", "next"),
                fact("zeta2", "next"),
            ],
            expect: vec![],
            questions: vec![],
            superseded_pairs: vec![
                super::super::script::SupersededPair {
                    old: "delta".into(),
                    new: "delta2".into(),
                    retire_in: "s2".into(),
                },
                super::super::script::SupersededPair {
                    old: "epsilon".into(),
                    new: "epsilon2".into(),
                    retire_in: "s2".into(),
                },
                super::super::script::SupersededPair {
                    old: "zeta".into(),
                    new: "zeta2".into(),
                    retire_in: "s2".into(),
                },
            ],
            noise: vec![
                super::super::script::NoiseItem {
                    id: "N1".into(),
                    session: "s1".into(),
                    match_any: vec!["alpha".into()],
                },
                super::super::script::NoiseItem {
                    id: "N2".into(),
                    session: "s2".into(),
                    match_any: vec!["zzz".into()],
                },
            ],
        }
    }

    fn patterned(
        id: &str,
        object: &str,
        end_state: EndState,
        superseded_in: Option<&str>,
        core: &[&str],
        detail: &[&str],
        wrong: &[&str],
    ) -> ScriptFact {
        let mut fact = fact(id, object);
        fact.end_state = end_state;
        fact.superseded_in = superseded_in.map(str::to_string);
        fact.pattern = Some(FactPattern {
            core_all: core.iter().map(|item| (*item).to_string()).collect(),
            detail_all: detail.iter().map(|item| (*item).to_string()).collect(),
            wrong_any: wrong.iter().map(|item| (*item).to_string()).collect(),
        });
        fact
    }

    fn pattern_observations() -> Vec<SessionObservation> {
        let zeta_live = edge("zeta", "zeta", true, 9.0, 1.0, 0);
        let after_s1 = vec![
            edge("alpha", "alpha detail", true, 9.0, 1.0, 0),
            edge("beta", "beta", true, 9.0, 1.0, 0),
            edge("gamma", "gamma evil", true, 9.0, 1.0, 0),
            edge("delta", "delta", true, 9.0, 1.0, 0),
            edge("epsilon", "epsilon", true, 9.0, 1.0, 0),
            edge("zeta", "zeta", false, 9.0, 1.0, 0),
            edge("rho", "rho", true, 9.0, 1.0, 0),
            edge("left", "leftover", true, 9.0, 1.0, 0),
        ];
        let mut after_s2 = after_s1.clone();
        after_s2
            .iter_mut()
            .find(|item| item.id == "delta")
            .expect("delta")
            .live = false;
        vec![
            obs("s1", vec![zeta_live], after_s1.clone()),
            obs("s2", after_s1, after_s2),
        ]
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn full(n: u32) -> OpScore {
        OpScore {
            tp: n,
            fp: 0,
            missed: 0,
            precision: Some(1.0),
            recall: Some(1.0),
        }
    }
}
