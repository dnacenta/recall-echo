// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fact-script format. One file describes the claims, the noise that must not
//! become facts, and the graph those claims should leave behind.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::error::RecallError;
use crate::graph::types::EntityType;

/// A fact history: sessions of claims, the triples they name, and the graph
/// expected once every session has been ingested.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactScript {
    pub name: String,
    pub sessions: Vec<ScriptSession>,
    pub facts: Vec<ScriptFact>,
    #[serde(default)]
    pub expect: Vec<ExpectedFact>,
    #[serde(default)]
    pub questions: Vec<DerivedQuestion>,
}

/// One archive. Kept to a single speaker so chunk classification stays
/// unambiguous: a chunk that contains an Assistant heading is self for every
/// claim in it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptSession {
    pub id: String,
    pub date: String,
    pub turns: Vec<ScriptTurn>,
}

/// One turn inside a session.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptTurn {
    pub role: Speaker,
    pub text: String,
    #[serde(default)]
    pub claims: Vec<Claim>,
    /// Prose that must not become a fact.
    #[serde(default)]
    pub noise: bool,
}

/// Who spoke the turn. Written into the archive as that role's heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    User,
    Assistant,
}

/// What the write path should do with one claim.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub op: ClaimOp,
    pub fact: String,
    /// The fact that replaces `fact` when `op` is supersede.
    #[serde(default)]
    pub replacement: Option<String>,
    /// Speaker credited for a corroboration or a contradiction.
    /// Defaults to the turn's role.
    #[serde(default)]
    pub by: Option<Speaker>,
}

/// The operation a claim asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOp {
    Create,
    Corroborate,
    Supersede,
    Contradict,
    Reject,
}

impl ClaimOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Corroborate => "corroborate",
            Self::Supersede => "supersede",
            Self::Contradict => "contradict",
            Self::Reject => "reject",
        }
    }
}

/// A triple the claims refer to by id.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptFact {
    pub id: String,
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub subject_type: String,
    pub object_type: String,
    #[serde(default)]
    pub subject_abstract: Option<String>,
    #[serde(default)]
    pub object_abstract: Option<String>,
}

/// The edge a fact should occupy when the script is finished.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedFact {
    pub fact: String,
    pub live: bool,
    /// Role the edge's provenance tally should show.
    #[serde(default)]
    pub provenance: Option<EndProvenance>,
    /// Expected `self_reinforcements` on that edge.
    #[serde(default)]
    pub self_reinforcements: Option<i64>,
}

/// How the provenance tally on an edge should read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndProvenance {
    /// No self-reinforcement. User evidence only.
    User,
    /// At least one self-reinforcement.
    Assistant,
    /// User evidence and at least one self-reinforcement.
    Mixed,
}

/// A question whose answer is assembled from more than one event.
///
/// `needs` must be live and `absent` must not be. Keyword search is scored
/// separately, over the archives.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedQuestion {
    pub text: String,
    pub answer: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub absent: Vec<String>,
}

impl FactScript {
    /// The fact named `id`.
    pub fn fact(&self, id: &str) -> Result<&ScriptFact, RecallError> {
        self.facts
            .iter()
            .find(|fact| fact.id == id)
            .ok_or_else(|| RecallError::Config(format!("unknown fact '{id}'")))
    }
}

impl ScriptFact {
    pub(crate) fn abstract_for(&self, side: Side) -> String {
        let (explicit, name) = match side {
            Side::Subject => (&self.subject_abstract, self.subject.as_str()),
            Side::Object => (&self.object_abstract, self.object.as_str()),
        };
        explicit
            .clone()
            .unwrap_or_else(|| format!("{name} ({})", self.id))
    }

    pub(crate) fn type_for(&self, side: Side) -> Result<EntityType, RecallError> {
        let raw = match side {
            Side::Subject => self.subject_type.as_str(),
            Side::Object => self.object_type.as_str(),
        };
        parse_entity_type(raw)
    }
}

/// Which end of a fact a field belongs to.
#[derive(Clone, Copy)]
pub(crate) enum Side {
    Subject,
    Object,
}

pub(crate) fn parse_entity_type(raw: &str) -> Result<EntityType, RecallError> {
    raw.parse().map_err(|err: String| RecallError::Config(err))
}

/// Read a fact script from `path`.
pub fn load_script(path: &Path) -> Result<FactScript, RecallError> {
    let text = fs::read_to_string(path)?;
    parse_script(&text)
}

/// Parse a fact script from TOML text.
pub fn parse_script(text: &str) -> Result<FactScript, RecallError> {
    let script: FactScript =
        toml::from_str(text).map_err(|err| RecallError::Config(format!("fact script: {err}")))?;
    validate(&script)?;
    Ok(script)
}

fn validate(script: &FactScript) -> Result<(), RecallError> {
    frontmatter_token("script name", &script.name)?;
    if script.sessions.is_empty() {
        return Err(invalid("a fact script needs at least one session"));
    }
    unique_ids("fact", script.facts.iter().map(|fact| fact.id.as_str()))?;
    unique_ids(
        "session",
        script.sessions.iter().map(|session| session.id.as_str()),
    )?;
    for fact in &script.facts {
        check_fact(fact)?;
    }
    for session in &script.sessions {
        check_session(script, session)?;
    }
    distinct_turns(script)?;
    expect_covers_facts(script)?;
    check_questions(script)
}

fn check_session(script: &FactScript, session: &ScriptSession) -> Result<(), RecallError> {
    frontmatter_token("session id", &session.id)?;
    nonempty_line("session date", &session.date)?;
    if session.turns.is_empty() {
        return Err(invalid(format!("session '{}' has no turns", session.id)));
    }
    for turn in &session.turns {
        nonempty_line("turn text", &turn.text)?;
        for claim in &turn.claims {
            check_claim(script, turn, claim)?;
        }
    }
    Ok(())
}

fn check_fact(fact: &ScriptFact) -> Result<(), RecallError> {
    nonempty_line("fact id", &fact.id)?;
    nonempty_line("subject", &fact.subject)?;
    nonempty_line("relation", &fact.relation)?;
    nonempty_line("object", &fact.object)?;
    fact.type_for(Side::Subject)
        .map_err(|err| invalid(format!("fact '{}': {err}", fact.id)))?;
    fact.type_for(Side::Object)
        .map_err(|err| invalid(format!("fact '{}': {err}", fact.id)))?;
    Ok(())
}

fn check_claim(script: &FactScript, turn: &ScriptTurn, claim: &Claim) -> Result<(), RecallError> {
    if turn.noise {
        return Err(invalid("a noise turn cannot carry claims"));
    }
    script.fact(&claim.fact)?;
    match claim.op {
        ClaimOp::Supersede => check_replacement(script, claim)?,
        _ if claim.replacement.is_some() => {
            return Err(invalid(format!(
                "{} must not name a replacement",
                claim.op.as_str()
            )));
        }
        _ => {}
    }
    if claim.by.is_some() && !matches!(claim.op, ClaimOp::Corroborate | ClaimOp::Contradict) {
        return Err(invalid("`by` is only valid on corroborate and contradict"));
    }
    Ok(())
}

fn check_replacement(script: &FactScript, claim: &Claim) -> Result<(), RecallError> {
    let replacement = claim
        .replacement
        .as_deref()
        .ok_or_else(|| invalid(format!("supersede of '{}' needs a replacement", claim.fact)))?;
    if replacement == claim.fact {
        return Err(invalid(format!(
            "supersede of '{replacement}' names itself"
        )));
    }
    script.fact(replacement)?;
    Ok(())
}

fn distinct_turns(script: &FactScript) -> Result<(), RecallError> {
    let mut texts: Vec<&str> = Vec::new();
    for session in &script.sessions {
        for turn in &session.turns {
            if texts
                .iter()
                .any(|earlier| turn.text.contains(earlier) || earlier.contains(turn.text.as_str()))
            {
                return Err(invalid(
                    "turn texts must not contain one another; claims are matched on them",
                ));
            }
            texts.push(&turn.text);
        }
    }
    Ok(())
}

fn expect_covers_facts(script: &FactScript) -> Result<(), RecallError> {
    let mut seen = HashSet::new();
    for expected in &script.expect {
        if !seen.insert(expected.fact.as_str()) {
            return Err(invalid(format!(
                "fact '{}' is listed twice in expect",
                expected.fact
            )));
        }
        if expected.self_reinforcements.is_some_and(|n| n < 0) {
            return Err(invalid(format!(
                "fact '{}' has a negative provenance tally",
                expected.fact
            )));
        }
        script.fact(&expected.fact)?;
    }
    for fact in &script.facts {
        if !seen.contains(fact.id.as_str()) {
            return Err(invalid(format!(
                "fact '{}' is missing from expect",
                fact.id
            )));
        }
    }
    Ok(())
}

fn check_questions(script: &FactScript) -> Result<(), RecallError> {
    for question in &script.questions {
        nonempty_line("question", &question.text)?;
        nonempty_line("answer", &question.answer)?;
        for id in question.needs.iter().chain(question.absent.iter()) {
            script.fact(id)?;
        }
    }
    Ok(())
}

fn unique_ids<'a>(kind: &str, ids: impl Iterator<Item = &'a str>) -> Result<(), RecallError> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(invalid(format!("duplicate {kind} '{id}'")));
        }
    }
    Ok(())
}

fn frontmatter_token(label: &str, value: &str) -> Result<(), RecallError> {
    nonempty_line(label, value)?;
    if value.contains('"') {
        return Err(invalid(format!("{label} cannot contain a quote")));
    }
    Ok(())
}

fn nonempty_line(label: &str, value: &str) -> Result<(), RecallError> {
    if value.trim().is_empty() || value.contains('\n') {
        return Err(invalid(format!("{label} must be a single non-empty line")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> RecallError {
    RecallError::Config(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/writes/history.toml");
        fs::read_to_string(path).unwrap()
    }

    fn ops(script: &FactScript, op: ClaimOp) -> usize {
        script
            .sessions
            .iter()
            .flat_map(|session| &session.turns)
            .flat_map(|turn| &turn.claims)
            .filter(|claim| claim.op == op)
            .count()
    }

    #[test]
    fn history_fixture_names_every_operation() {
        let script = parse_script(&history()).unwrap();
        assert_eq!(script.name, "host-history");
        assert_eq!(script.sessions.len(), 7);
        assert_eq!(ops(&script, ClaimOp::Create), 1);
        assert_eq!(ops(&script, ClaimOp::Corroborate), 2);
        assert_eq!(ops(&script, ClaimOp::Supersede), 1);
        assert_eq!(ops(&script, ClaimOp::Contradict), 1);
        assert_eq!(ops(&script, ClaimOp::Reject), 1);
        assert_eq!(
            script
                .sessions
                .iter()
                .flat_map(|session| &session.turns)
                .filter(|turn| turn.noise)
                .count(),
            1
        );
        assert_eq!(script.facts.len(), 3);
        assert_eq!(script.expect.len(), 3);
        assert_eq!(script.questions.len(), 2);
        let supersede = script
            .sessions
            .iter()
            .flat_map(|session| &session.turns)
            .flat_map(|turn| &turn.claims)
            .find(|claim| claim.op == ClaimOp::Supersede)
            .unwrap();
        assert_eq!(supersede.replacement.as_deref(), Some("deploy-host-oak"));
    }

    #[test]
    fn malformed_script_is_rejected() {
        let err = parse_script("name = ").unwrap_err();
        assert!(err.to_string().contains("fact script"), "{err}");
    }

    #[test]
    fn an_unknown_operation_is_rejected() {
        let text = r#"
name = "bad"

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "The deploy host is pine."

[[sessions.turns.claims]]
op = "forget"
fact = "host"

[[facts]]
id = "host"
subject = "deploy"
relation = "setting"
object = "pine"
subject_type = "project"
object_type = "concept"

[[expect]]
fact = "host"
live = true
"#;
        assert!(parse_script(text).is_err());
    }

    #[test]
    fn an_empty_session_list_is_rejected() {
        let text = "name = \"empty\"\nsessions = []\nfacts = []\nexpect = []\n";
        let err = parse_script(text).unwrap_err();
        assert!(err.to_string().contains("at least one session"), "{err}");
    }

    #[test]
    fn noise_cannot_carry_a_claim() {
        let text = r#"
name = "noisy"

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "The kettle boiled."
noise = true

[[sessions.turns.claims]]
op = "create"
fact = "host"

[[facts]]
id = "host"
subject = "deploy"
relation = "setting"
object = "pine"
subject_type = "project"
object_type = "concept"

[[expect]]
fact = "host"
live = false
"#;
        let err = parse_script(text).unwrap_err();
        assert!(err.to_string().contains("noise"), "{err}");
    }

    #[test]
    fn a_turn_that_contains_another_is_rejected() {
        let text = r#"
name = "overlap"

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "pine"

[[sessions.turns.claims]]
op = "create"
fact = "host"

[[sessions]]
id = "s2"
date = "2026-01-02T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "pine tree"

[[sessions.turns.claims]]
op = "corroborate"
fact = "host"

[[facts]]
id = "host"
subject = "deploy"
relation = "setting"
object = "pine"
subject_type = "project"
object_type = "concept"

[[expect]]
fact = "host"
live = true
"#;
        let err = parse_script(text).unwrap_err();
        assert!(err.to_string().contains("contain"), "{err}");
    }

    #[test]
    fn a_fact_missing_from_expect_is_rejected() {
        let text = r#"
name = "gap"
expect = []

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "The deploy host is pine."

[[sessions.turns.claims]]
op = "create"
fact = "host"

[[facts]]
id = "host"
subject = "deploy"
relation = "setting"
object = "pine"
subject_type = "project"
object_type = "concept"
"#;
        let err = parse_script(text).unwrap_err();
        assert!(err.to_string().contains("missing from expect"), "{err}");
    }
}
