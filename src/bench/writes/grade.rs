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
    Claim, ClaimOp, EndProvenance, FactScript, ScriptFact, ScriptSession, ScriptTurn, Speaker,
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
    pub millis: u64,
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
}

impl Effect {
    fn op(&self) -> WriteOp {
        match self {
            Self::Created { .. } => WriteOp::Create,
            Self::Corroborated { .. } => WriteOp::Corroborate,
            Self::Retired { .. } => WriteOp::Supersede,
            Self::Contradicted { .. } => WriteOp::Contradict,
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
        }
    }

    fn counts(&mut self, op: WriteOp) -> &mut Counts {
        match op {
            WriteOp::Create => &mut self.create,
            WriteOp::Corroborate => &mut self.corroborate,
            WriteOp::Supersede => &mut self.supersede,
            WriteOp::Contradict => &mut self.contradict,
            WriteOp::NoOp => &mut self.no_op,
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
    let mut wrong_facts = end_state(script, final_edges)?;
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
                score_claim(&mut ctx, turn, claim)?;
            }
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

fn score_claim(ctx: &mut Score<'_>, turn: &ScriptTurn, claim: &Claim) -> Result<(), RecallError> {
    match claim.op {
        ClaimOp::Create => score_create(ctx, claim),
        ClaimOp::Corroborate => score_corroborate(ctx, turn, claim),
        ClaimOp::Supersede => score_supersede(ctx, claim),
        ClaimOp::Contradict => score_contradict(ctx, turn, claim),
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
    turn: &ScriptTurn,
    claim: &Claim,
) -> Result<(), RecallError> {
    let key = fact_key(ctx.script.fact(&claim.fact)?);
    let speaker = claim.by.unwrap_or(turn.role);
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
    turn: &ScriptTurn,
    claim: &Claim,
) -> Result<(), RecallError> {
    let key = fact_key(ctx.script.fact(&claim.fact)?);
    let speaker = claim.by.unwrap_or(turn.role);
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
        Speaker::Assistant => {
            let steps = u32::try_from(delta_self).unwrap_or(0);
            delta_self >= 1 && approx(delta_alpha, weight * f64::from(steps))
        }
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
    let mut effects = Vec::new();
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

fn end_state(script: &FactScript, edges: &[ObservedEdge]) -> Result<Vec<WrongFact>, RecallError> {
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
        if provenance_miss(expected, matching.as_slice()) {
            wrong.push(WrongFact {
                fact: expected.fact.clone(),
                reason: WrongReason::Provenance,
            });
        }
    }
    Ok(wrong)
}

fn provenance_miss(expected: &super::script::ExpectedFact, matching: &[&ObservedEdge]) -> bool {
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
        EndProvenance::Assistant | EndProvenance::Mixed => edge.self_reinforcements >= 1,
    });
    !tally_ok || !role_ok
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

#[cfg(test)]
mod tests {
    use super::super::script::{
        Claim, ClaimOp, DerivedQuestion, EndProvenance, ExpectedFact, FactScript, ScriptFact,
        ScriptSession, ScriptTurn, Speaker,
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
        }
    }

    fn session(id: &str, turn: ScriptTurn) -> ScriptSession {
        ScriptSession {
            id: id.to_string(),
            date: "2026-01-01T00:00:00Z".to_string(),
            turns: vec![turn],
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
    fn a_session_count_mismatch_is_rejected() {
        let err = grade(&history(), &[], &weights(), &[], ExtractorLabel::Scripted).unwrap_err();
        assert!(err.to_string().contains("session observations"), "{err}");
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
