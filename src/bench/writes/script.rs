// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fact-script format. One file describes the claims, the noise that must not
//! become facts, and the graph those claims should leave behind.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::conversation::{Conversation, ConversationEntry};
use crate::error::RecallError;
use crate::graph::types::EntityType;

/// A fact history: sessions of claims, the triples they name, and the graph
/// expected once every session has been ingested.
///
/// `project` and `cwd` are carried for later pipeline work. Pattern grading
/// reads [`FactScript::superseded_pairs`] and [`FactScript::noise`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactScript {
    pub name: String,
    /// Project the sessions are about.
    #[serde(default)]
    pub project: Option<String>,
    /// Working directory recorded for the sessions.
    #[serde(default)]
    pub cwd: Option<String>,
    pub sessions: Vec<ScriptSession>,
    pub facts: Vec<ScriptFact>,
    #[serde(default)]
    pub expect: Vec<ExpectedFact>,
    #[serde(default)]
    pub questions: Vec<DerivedQuestion>,
    /// Old fact retired in favour of a new one, and the session that may do it.
    #[serde(default)]
    pub superseded_pairs: Vec<SupersededPair>,
    /// Prose that must not become a live fact. Graded by pattern, not by turn.
    #[serde(default)]
    pub noise: Vec<NoiseItem>,
}

/// One archive. Kept to a single speaker so chunk classification stays
/// unambiguous: a chunk that contains an Assistant heading is self for every
/// claim in it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptSession {
    pub id: String,
    /// Frontmatter date. Optional on a transcript session: the first timestamp
    /// in the file is used when this is empty.
    #[serde(default)]
    pub date: String,
    /// Hand-written turns. Mutually exclusive with [`ScriptSession::transcript`].
    #[serde(default)]
    pub turns: Vec<ScriptTurn>,
    /// JSONL transcript path, relative to the script file.
    #[serde(default)]
    pub transcript: Option<String>,
    /// Absolute path set by [`load_script`]. The runner parses this file.
    #[serde(skip)]
    pub transcript_path: Option<PathBuf>,
    /// Claims for a transcript session. Each `quote` is the needle.
    #[serde(default)]
    pub claims: Vec<Claim>,
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
    /// Verbatim substring of one text message. Required on a transcript claim.
    /// The scripted extractor uses it as the needle.
    #[serde(default)]
    pub quote: Option<String>,
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
    /// Where the fact should sit once every session has been ingested.
    #[serde(default)]
    pub end_state: EndState,
    /// Session that supersedes this fact. Required when [`ScriptFact::end_state`]
    /// is [`EndState::Superseded`].
    #[serde(default)]
    pub superseded_in: Option<String>,
    /// Regexes the pattern grader applies to rendered records.
    #[serde(default, rename = "match")]
    pub pattern: Option<FactPattern>,
}

/// Where a fact should sit at the end of the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EndState {
    /// Still asserted.
    #[default]
    Live,
    /// Replaced. A live record that still matches the old value is stale,
    /// except one assigned to the successor that matches strictly more of
    /// the successor's patterns than of this fact's.
    Superseded,
    /// Refused. A live core match is `rejected_live` (wrong, not found);
    /// no live match is correct-absent. The fact does not take a record
    /// from a live fact.
    Rejected,
}

/// Case-insensitive patterns against one rendered record.
///
/// `found` needs every [`FactPattern::core_all`] on one record. `complete`
/// also needs every [`FactPattern::detail_all`] on that same record.
/// [`FactPattern::wrong_any`] marks a live fact wrong.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactPattern {
    #[serde(default)]
    pub core_all: Vec<String>,
    #[serde(default)]
    pub detail_all: Vec<String>,
    #[serde(default)]
    pub wrong_any: Vec<String>,
}

impl FactPattern {
    fn sources(&self) -> impl Iterator<Item = &str> {
        self.core_all
            .iter()
            .chain(self.detail_all.iter())
            .chain(self.wrong_any.iter())
            .map(String::as_str)
    }
}

/// An old fact that must be retired in `retire_in` or later, never earlier.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupersededPair {
    pub old: String,
    pub new: String,
    pub retire_in: String,
}

/// A noise item. A hit is a live record matching any [`NoiseItem::match_any`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseItem {
    pub id: String,
    pub session: String,
    #[serde(default)]
    pub match_any: Vec<String>,
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
/// separately, over the archives. `answer` is also looked for as an exact
/// value on the final store and in the recall render.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedQuestion {
    pub text: String,
    pub answer: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub absent: Vec<String>,
    /// How the answer is graded. Defaults to [`QuestionKind::Exact`].
    #[serde(default)]
    pub kind: QuestionKind,
    /// Superseded or rejected values the recall render is scanned for. Only
    /// valid on a current-value question. Omitted there, it is filled from
    /// [`FactScript::superseded_pairs`]: the old value of each pair whose new
    /// fact the question needs, or whose new fact's object is the answer.
    #[serde(default)]
    pub stale: Option<Vec<StaleRef>>,
}

/// How a derived question's answer is graded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// One exact value.
    #[default]
    Exact,
    /// The value now in force. Stale values are scanned for as well.
    CurrentValue,
}

/// A value that must not steer a current-value answer.
///
/// A bare string names a fact, and that fact's object is the value.
/// `{ text = "..." }` is the value itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum StaleRef {
    Fact(String),
    Text(StaleText),
}

/// A literal stale value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaleText {
    pub text: String,
}

impl FactScript {
    /// The fact named `id`.
    pub fn fact(&self, id: &str) -> Result<&ScriptFact, RecallError> {
        self.facts
            .iter()
            .find(|fact| fact.id == id)
            .ok_or_else(|| RecallError::Config(format!("unknown fact '{id}'")))
    }

    /// The stale values for `question`. Empty unless it is a current-value
    /// question.
    pub(crate) fn stale_values(
        &self,
        question: &DerivedQuestion,
    ) -> Result<Vec<String>, RecallError> {
        if question.kind != QuestionKind::CurrentValue {
            return Ok(Vec::new());
        }
        let Some(listed) = &question.stale else {
            return self.paired_stale(question);
        };
        listed
            .iter()
            .map(|item| match item {
                StaleRef::Fact(id) => Ok(self.fact(id)?.object.clone()),
                StaleRef::Text(text) => Ok(text.text.clone()),
            })
            .collect()
    }

    /// Old values of the pairs whose new fact `question` needs or answers.
    fn paired_stale(&self, question: &DerivedQuestion) -> Result<Vec<String>, RecallError> {
        let answer = question.answer.trim().to_lowercase();
        let mut values: Vec<String> = Vec::new();
        for pair in &self.superseded_pairs {
            let new = self.fact(&pair.new)?;
            let needed = question.needs.contains(&pair.new);
            if !needed && new.object.trim().to_lowercase() != answer {
                continue;
            }
            let old = &self.fact(&pair.old)?.object;
            if !values.contains(old) {
                values.push(old.clone());
            }
        }
        Ok(values)
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
///
/// Transcript paths are resolved against the script's directory, parsed with
/// the JSONL reader, and checked: every claim quote must occur in a user or
/// assistant text message.
pub fn load_script(path: &Path) -> Result<FactScript, RecallError> {
    let text = fs::read_to_string(path)?;
    let mut script = deserialize(&text)?;
    let base = path.parent().unwrap_or_else(|| Path::new(""));
    resolve_transcripts(&mut script, base)?;
    validate(&script)?;
    Ok(script)
}

/// Largest fact script `parse_script` will read.
const MAX_SCRIPT_BYTES: usize = 1024 * 1024;
/// Largest number of turns one script may name.
const MAX_TURNS: usize = 500;

/// Parse a fact script from TOML text.
///
/// Transcript files are not read here. [`load_script`] resolves them.
pub fn parse_script(text: &str) -> Result<FactScript, RecallError> {
    let script = deserialize(text)?;
    validate(&script)?;
    Ok(script)
}

fn deserialize(text: &str) -> Result<FactScript, RecallError> {
    if text.len() > MAX_SCRIPT_BYTES {
        return Err(invalid("fact script exceeds 1 MiB"));
    }
    toml::from_str(text).map_err(|err| RecallError::Config(format!("fact script: {err}")))
}

/// Compile one pattern the way the grader matches records: case-insensitive.
pub(crate) fn compile_pattern(source: &str) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(source)
        .case_insensitive(true)
        .build()
        .map_err(|err| err.to_string())
}

fn validate(script: &FactScript) -> Result<(), RecallError> {
    frontmatter_token("script name", &script.name)?;
    if let Some(project) = &script.project {
        nonempty_line("project", project)?;
    }
    if let Some(cwd) = &script.cwd {
        nonempty_line("cwd", cwd)?;
    }
    if script.sessions.is_empty() {
        return Err(invalid("a fact script needs at least one session"));
    }
    let turns = script
        .sessions
        .iter()
        .map(|session| session.turns.len())
        .sum::<usize>();
    if turns > MAX_TURNS {
        return Err(invalid(format!("fact script exceeds {MAX_TURNS} turns")));
    }
    unique_ids("fact", script.facts.iter().map(|fact| fact.id.as_str()))?;
    unique_ids(
        "session",
        script.sessions.iter().map(|session| session.id.as_str()),
    )?;
    unique_ids("noise", script.noise.iter().map(|item| item.id.as_str()))?;
    for fact in &script.facts {
        check_fact(script, fact)?;
    }
    for session in &script.sessions {
        check_session(script, session)?;
    }
    distinct_turns(script)?;
    expect_covers_facts(script)?;
    check_questions(script)?;
    check_pairs(script)?;
    check_noise(script)
}

fn resolve_transcripts(script: &mut FactScript, base: &Path) -> Result<(), RecallError> {
    for session in &mut script.sessions {
        let Some(rel) = session.transcript.clone() else {
            continue;
        };
        if rel.trim().is_empty() {
            return Err(invalid(format!(
                "session '{}' transcript path is empty",
                session.id
            )));
        }
        let full = base.join(&rel);
        if !full.is_file() {
            return Err(invalid(format!(
                "session '{}' transcript '{}' does not exist",
                session.id, rel
            )));
        }
        let path = full.to_str().ok_or_else(|| {
            invalid(format!(
                "session '{}' transcript '{}' is not utf-8",
                session.id, rel
            ))
        })?;
        let conversation = crate::jsonl::parse_transcript(path, &session.id).map_err(|err| {
            invalid(format!(
                "session '{}' transcript '{}': {err}",
                session.id, rel
            ))
        })?;
        if session.date.trim().is_empty() {
            session.date = conversation.first_timestamp.clone().ok_or_else(|| {
                invalid(format!(
                    "session '{}' has no date and its transcript has no timestamp",
                    session.id
                ))
            })?;
        }
        check_quotes(&session.id, &session.claims, &conversation)?;
        session.transcript_path = Some(full);
    }
    Ok(())
}

fn check_quotes(
    session_id: &str,
    claims: &[Claim],
    conversation: &Conversation,
) -> Result<(), RecallError> {
    let texts = text_messages(conversation);
    for claim in claims {
        let Some(quote) = claim.quote.as_deref().filter(|quote| !quote.is_empty()) else {
            return Err(invalid(format!(
                "claim '{}' in session '{session_id}' needs a quote",
                claim.fact
            )));
        };
        if !texts.iter().any(|text| text.contains(quote)) {
            return Err(invalid(format!(
                "claim '{}' quote is not in a text message of session '{session_id}'",
                claim.fact
            )));
        }
    }
    Ok(())
}

fn text_messages(conversation: &Conversation) -> Vec<&str> {
    conversation
        .entries
        .iter()
        .filter_map(|entry| match entry {
            ConversationEntry::UserMessage(text) | ConversationEntry::AssistantText(text) => {
                Some(text.as_str())
            }
            ConversationEntry::ToolUse { .. } | ConversationEntry::ToolResult { .. } => None,
        })
        .collect()
}

fn check_session(script: &FactScript, session: &ScriptSession) -> Result<(), RecallError> {
    frontmatter_token("session id", &session.id)?;
    let transcript = session.transcript.is_some();
    let turns = !session.turns.is_empty();
    if transcript && turns {
        return Err(invalid(format!(
            "session '{}' sets both transcript and turns",
            session.id
        )));
    }
    if !transcript && !turns {
        return Err(invalid(format!("session '{}' has no turns", session.id)));
    }
    if transcript {
        return check_transcript_session(script, session);
    }
    if !session.claims.is_empty() {
        return Err(invalid(format!(
            "session '{}' claims belong on a transcript session",
            session.id
        )));
    }
    frontmatter_token("session date", &session.date)?;
    for turn in &session.turns {
        check_turn_text(&turn.text)?;
        for claim in &turn.claims {
            check_claim(script, turn, claim)?;
        }
    }
    Ok(())
}

fn check_transcript_session(
    script: &FactScript,
    session: &ScriptSession,
) -> Result<(), RecallError> {
    // A date omitted in the file is filled from the transcript before this runs.
    frontmatter_token("session date", &session.date)?;
    if session.transcript_path.is_none() {
        return Err(invalid(format!(
            "session '{}' transcript was not resolved; load the script from its file",
            session.id
        )));
    }
    for claim in &session.claims {
        check_claim_shape(script, claim)?;
        if claim.quote.as_deref().is_none_or(str::is_empty) {
            return Err(invalid(format!(
                "claim '{}' in session '{}' needs a quote",
                claim.fact, session.id
            )));
        }
    }
    Ok(())
}

fn check_fact(script: &FactScript, fact: &ScriptFact) -> Result<(), RecallError> {
    nonempty_line("fact id", &fact.id)?;
    nonempty_line("subject", &fact.subject)?;
    nonempty_line("relation", &fact.relation)?;
    nonempty_line("object", &fact.object)?;
    fact.type_for(Side::Subject)
        .map_err(|err| invalid(format!("fact '{}': {err}", fact.id)))?;
    fact.type_for(Side::Object)
        .map_err(|err| invalid(format!("fact '{}': {err}", fact.id)))?;
    if let Some(pattern) = &fact.pattern {
        for source in pattern.sources() {
            compile_pattern(source)
                .map_err(|err| invalid(format!("fact '{}' regex '{source}': {err}", fact.id)))?;
        }
    }
    if fact.end_state == EndState::Superseded {
        let session = fact.superseded_in.as_deref().ok_or_else(|| {
            invalid(format!(
                "fact '{}' is superseded and needs superseded_in",
                fact.id
            ))
        })?;
        require_session(
            script,
            session,
            &format!("fact '{}' superseded_in", fact.id),
        )?;
    }
    Ok(())
}

fn check_claim(script: &FactScript, turn: &ScriptTurn, claim: &Claim) -> Result<(), RecallError> {
    if turn.noise {
        return Err(invalid("a noise turn cannot carry claims"));
    }
    check_claim_shape(script, claim)
}

fn check_claim_shape(script: &FactScript, claim: &Claim) -> Result<(), RecallError> {
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
        check_stale(script, question)?;
    }
    Ok(())
}

fn check_stale(script: &FactScript, question: &DerivedQuestion) -> Result<(), RecallError> {
    let Some(listed) = &question.stale else {
        return Ok(());
    };
    if question.kind != QuestionKind::CurrentValue {
        return Err(invalid(format!(
            "question '{}' lists stale values but is not a current_value question",
            question.text
        )));
    }
    for item in listed {
        match item {
            StaleRef::Fact(id) => {
                script.fact(id)?;
            }
            StaleRef::Text(text) => nonempty_line("stale text", &text.text)?,
        }
    }
    Ok(())
}

fn check_pairs(script: &FactScript) -> Result<(), RecallError> {
    for pair in &script.superseded_pairs {
        script.fact(&pair.old)?;
        script.fact(&pair.new)?;
        if pair.old == pair.new {
            return Err(invalid(format!(
                "superseded pair '{}' names itself",
                pair.old
            )));
        }
        require_session(
            script,
            &pair.retire_in,
            &format!("superseded pair '{}'", pair.old),
        )?;
    }
    Ok(())
}

fn check_noise(script: &FactScript) -> Result<(), RecallError> {
    for item in &script.noise {
        nonempty_line("noise id", &item.id)?;
        require_session(script, &item.session, &format!("noise '{}'", item.id))?;
        for source in &item.match_any {
            compile_pattern(source)
                .map_err(|err| invalid(format!("noise '{}' regex '{source}': {err}", item.id)))?;
        }
    }
    Ok(())
}

fn require_session(script: &FactScript, id: &str, label: &str) -> Result<(), RecallError> {
    if script.sessions.iter().any(|session| session.id == id) {
        return Ok(());
    }
    Err(invalid(format!("{label} names unknown session '{id}'")))
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

fn check_turn_text(text: &str) -> Result<(), RecallError> {
    nonempty_line("turn text", text)?;
    if text.contains('|') {
        return Err(invalid("turn text cannot contain a bar"));
    }
    let trimmed = text.trim();
    if trimmed.starts_with('#') || trimmed.starts_with("---") {
        return Err(invalid("turn text cannot look like a heading or a break"));
    }
    Ok(())
}

fn frontmatter_token(label: &str, value: &str) -> Result<(), RecallError> {
    nonempty_line(label, value)?;
    if value.contains('"') || value.contains('|') {
        return Err(invalid(format!("{label} cannot contain a quote or a bar")));
    }
    Ok(())
}

fn nonempty_line(label: &str, value: &str) -> Result<(), RecallError> {
    if value.trim().is_empty() || value.contains('\n') || value.contains('\r') {
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

    #[test]
    fn a_bar_a_carriage_return_or_a_heading_is_rejected() {
        let date = script_with("date = \"2026-01-01|bad\"", "The deploy host is pine.");
        assert!(parse_script(&date).unwrap_err().to_string().contains("bar"));
        let cr = script_with("date = \"2026-01-01T00:00:00Z\"", "line\\rbreak");
        assert!(parse_script(&cr).unwrap_err().to_string().contains("line"));
        let heading = script_with("date = \"2026-01-01T00:00:00Z\"", "### Assistant");
        assert!(parse_script(&heading)
            .unwrap_err()
            .to_string()
            .contains("heading"));
        let huge = "x".repeat(MAX_SCRIPT_BYTES + 1);
        assert!(parse_script(&huge)
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
    }

    #[test]
    fn a_transcript_session_quote_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        fs::write(
            sessions.join("s01.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-09-28T09:10:37.000Z","message":{"role":"user","content":"Python 3.12, managed with uv"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let loaded = load_script(&write_transcript_script(
            dir.path(),
            "script.toml",
            "",
            "Python 3.12, managed with uv",
        ))
        .unwrap();
        assert_eq!(loaded.sessions[0].date, "2026-09-28T09:10:37.000Z");
        assert!(loaded.sessions[0].transcript_path.is_some());

        fs::write(
            sessions.join("s01.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-09-28T09:10:37.000Z","message":{"role":"user","content":[{"type":"tool_result","content":"only in the tool"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let err = load_script(&write_transcript_script(
            dir.path(),
            "tool.toml",
            "date = \"2026-09-28T09:10:37Z\"\n",
            "only in the tool",
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("claim 'F01'"),
            "the claim must be named: {err}"
        );
        assert!(err.to_string().contains("text message"), "{err}");
    }

    #[test]
    fn transcript_and_turns_are_exclusive() {
        let text = r#"
name = "both"

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"
transcript = "sessions/s01.jsonl"

[[sessions.turns]]
role = "user"
text = "The deploy host is pine."

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
        assert!(
            err.to_string().contains("both transcript and turns"),
            "{err}"
        );
    }

    #[test]
    fn bad_regex_is_rejected() {
        let text = r#"
name = "bad-regex"

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
end_state = "live"

[facts.match]
core_all = ["(unclosed"]
detail_all = []
wrong_any = []

[[expect]]
fact = "host"
live = true
"#;
        let err = parse_script(text).unwrap_err();
        assert!(err.to_string().contains("regex"), "{err}");
        assert!(err.to_string().contains("host"), "{err}");
    }

    #[test]
    fn every_linkshelf_pattern_compiles() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/linkshelf/script.toml");
        let script = load_script(&path).unwrap();
        assert_eq!(script.name, "linkshelf");
        assert_eq!(script.project.as_deref(), Some("linkshelf"));
        assert_eq!(script.cwd.as_deref(), Some("/work/linkshelf"));
        assert_eq!(script.sessions.len(), 14);
        assert_eq!(script.facts.len(), 46);
        assert_eq!(script.superseded_pairs.len(), 6);
        assert_eq!(script.noise.len(), 7);
        assert_eq!(script.expect.len(), 46);
        assert!(script
            .sessions
            .iter()
            .all(|session| session.turns.is_empty()));
        assert!(script
            .sessions
            .iter()
            .all(|session| session.transcript_path.is_some()));
        let mut patterns = 0;
        for fact in &script.facts {
            let pattern = fact.pattern.as_ref().expect("every fact has a match");
            for source in pattern.sources() {
                compile_pattern(source).expect(source);
                patterns += 1;
            }
        }
        for item in &script.noise {
            for source in &item.match_any {
                compile_pattern(source).expect(source);
                patterns += 1;
            }
        }
        assert_eq!(patterns, 97);
    }

    /// Two storage settings under different keys, the first replaced by the
    /// second, a rejected cache setting, and the questions in `questions`.
    fn question_script(questions: &str) -> String {
        format!(
            r#"
name = "questions"

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "Keep the shelf in shelf.json."

[[sessions.turns.claims]]
op = "create"
fact = "path"

[[sessions]]
id = "s2"
date = "2026-01-02T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "Move the store to shelf.db and drop the redis cache."

[[sessions.turns.claims]]
op = "supersede"
fact = "path"
replacement = "store"

[[sessions.turns.claims]]
op = "reject"
fact = "cache"

[[facts]]
id = "path"
subject = "shelf"
relation = "setting:storage_path"
object = "shelf.json"
subject_type = "project"
object_type = "concept"
end_state = "superseded"
superseded_in = "s2"

[[facts]]
id = "store"
subject = "shelf"
relation = "setting:storage"
object = "shelf.db"
subject_type = "project"
object_type = "concept"

[[facts]]
id = "cache"
subject = "shelf"
relation = "setting:cache"
object = "redis"
subject_type = "project"
object_type = "concept"
end_state = "rejected"

[[expect]]
fact = "path"
live = false

[[expect]]
fact = "store"
live = true

[[expect]]
fact = "cache"
live = false

[[superseded_pairs]]
old = "path"
new = "store"
retire_in = "s2"

{questions}
"#
        )
    }

    #[test]
    fn a_question_without_a_kind_is_exact_and_has_no_stale_values() {
        let script = parse_script(&history()).unwrap();
        for question in &script.questions {
            assert_eq!(question.kind, QuestionKind::Exact);
            assert!(question.stale.is_none());
            assert!(script.stale_values(question).unwrap().is_empty());
        }
    }

    #[test]
    fn omitted_stale_values_come_from_the_superseded_pairs() {
        let script = parse_script(&question_script(
            r#"
[[questions]]
text = "where is the shelf stored"
answer = "shelf.db"
kind = "current_value"
needs = ["store"]
absent = ["path"]

[[questions]]
text = "what is the store file"
answer = "Shelf.DB"
kind = "current_value"
"#,
        ))
        .unwrap();
        // The pair crosses keys: storage_path to storage. The old value text
        // is what is scanned for, not the current relation name.
        let needed = script.stale_values(&script.questions[0]).unwrap();
        assert_eq!(needed, vec!["shelf.json".to_string()]);
        let answered = script.stale_values(&script.questions[1]).unwrap();
        assert_eq!(answered, vec!["shelf.json".to_string()]);
    }

    #[test]
    fn listed_stale_values_take_fact_ids_and_literal_text() {
        let script = parse_script(&question_script(
            r#"
[[questions]]
text = "what backs the shelf"
answer = "shelf.db"
kind = "current_value"
needs = ["store"]
stale = ["cache", { text = "json file" }]

[[questions]]
text = "what backs the shelf now"
answer = "shelf.db"
kind = "current_value"
needs = ["store"]
stale = []
"#,
        ))
        .unwrap();
        assert_eq!(
            script.stale_values(&script.questions[0]).unwrap(),
            vec!["redis".to_string(), "json file".to_string()]
        );
        // An empty list is kept: it does not fall back to the pairs.
        assert!(script
            .stale_values(&script.questions[1])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_stale_reference_must_name_a_known_fact() {
        let err = parse_script(&question_script(
            r#"
[[questions]]
text = "what backs the shelf"
answer = "shelf.db"
kind = "current_value"
stale = ["missing"]
"#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("unknown fact 'missing'"), "{err}");
    }

    #[test]
    fn stale_values_need_a_current_value_question() {
        let err = parse_script(&question_script(
            r#"
[[questions]]
text = "what backs the shelf"
answer = "shelf.db"
stale = ["path"]
"#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("current_value"), "{err}");
    }

    #[test]
    fn a_malformed_stale_entry_or_kind_is_rejected() {
        for questions in [
            r#"
[[questions]]
text = "what backs the shelf"
answer = "shelf.db"
kind = "current_value"
stale = [{ text = "json", fact = "path" }]
"#,
            r#"
[[questions]]
text = "what backs the shelf"
answer = "shelf.db"
kind = "current_value"
stale = [{ text = "" }]
"#,
            r#"
[[questions]]
text = "what backs the shelf"
answer = "shelf.db"
kind = "latest"
"#,
        ] {
            assert!(parse_script(&question_script(questions)).is_err());
        }
    }

    #[test]
    fn linkshelf_questions_come_from_gold_facts() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/linkshelf/script.toml");
        let script = load_script(&path).unwrap();
        assert_eq!(script.questions.len(), 9);
        let object = |id: &str| script.fact(id).unwrap().object.clone();
        // One current-value question per superseded pair: the new object is
        // the answer, and the old object is filled in as the stale value.
        let current: Vec<_> = script
            .questions
            .iter()
            .filter(|question| question.kind == QuestionKind::CurrentValue)
            .collect();
        assert_eq!(current.len(), script.superseded_pairs.len());
        for pair in &script.superseded_pairs {
            let question = current
                .iter()
                .find(|question| question.needs == std::slice::from_ref(&pair.new))
                .unwrap_or_else(|| panic!("no question needs {}", pair.new));
            assert_eq!(question.absent, std::slice::from_ref(&pair.old));
            assert_eq!(question.answer, object(&pair.new));
            assert!(question.stale.is_none());
            assert_eq!(
                script.stale_values(question).unwrap(),
                vec![object(&pair.old)]
            );
        }
        // Every answer is the gold object of a fact the question names.
        for question in &script.questions {
            assert!(
                question
                    .needs
                    .iter()
                    .chain(&question.absent)
                    .any(|id| object(id) == question.answer),
                "{}",
                question.text
            );
        }
        // Two stable settings, never replaced, and one refused value.
        let exact: Vec<_> = script
            .questions
            .iter()
            .filter(|question| question.kind == QuestionKind::Exact)
            .collect();
        assert_eq!(exact.len(), 3);
        let stable: Vec<_> = exact
            .iter()
            .filter(|question| !question.needs.is_empty())
            .collect();
        assert_eq!(stable.len(), 2);
        for question in &stable {
            for id in &question.needs {
                assert_eq!(script.fact(id).unwrap().end_state, EndState::Live);
                assert!(script
                    .superseded_pairs
                    .iter()
                    .all(|pair| pair.old != *id && pair.new != *id));
            }
        }
        let refused: Vec<_> = exact
            .iter()
            .filter(|question| question.needs.is_empty())
            .collect();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].absent, ["F42"]);
        assert_eq!(script.fact("F42").unwrap().end_state, EndState::Rejected);
    }

    fn silent_change() -> FactScript {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/silent-change/script.toml");
        load_script(&path).unwrap()
    }

    /// Words and phrases that announce a replacement. s2 of the silent-change
    /// fixture must use none of them. `new` before the setting's key words is
    /// checked by [`departure_cues`] as well.
    const DEPARTURE_CUES: &[&str] = &[
        "now",
        "is now",
        "no longer",
        "anymore",
        "instead",
        "rather than",
        "drop",
        "dropped",
        "switch",
        "switched",
        "replace",
        "replaced",
        "becomes",
        "became",
        "change",
        "changed",
        "bump",
        "bumped",
        "rename",
        "renamed",
        "move to",
        "moved to",
        "migrate",
        "migrated",
        "update",
        "updated",
        "upgrade",
        "upgraded",
        "remove",
        "removed",
        "deprecate",
        "old",
        "previous",
        "previously",
        "used to",
        "stop using",
    ];

    /// The cues in `text`, as whole words or phrases, ignoring case. `new`
    /// directly before one of `key` is a cue too.
    fn departure_cues(text: &str, key: &[&str]) -> Vec<String> {
        let mut found: Vec<String> = DEPARTURE_CUES
            .iter()
            .filter(|cue| has_phrase(text, cue))
            .map(|cue| (*cue).to_string())
            .collect();
        for word in key {
            let cue = format!("new {word}");
            if has_phrase(text, &cue) {
                found.push(cue);
            }
        }
        found
    }

    /// Whether `phrase` stands in `text` as whole words, ignoring case and
    /// the spacing between words.
    fn has_phrase(text: &str, phrase: &str) -> bool {
        let words: Vec<String> = phrase.split_whitespace().map(regex::escape).collect();
        compile_pattern(&format!(r"\b{}\b", words.join(r"\s+")))
            .unwrap()
            .is_match(text)
    }

    /// The setting's key words: the subject, then the key after `setting:`.
    fn key_words(fact: &ScriptFact) -> Vec<&str> {
        let key = fact
            .relation
            .strip_prefix("setting:")
            .unwrap_or(&fact.relation);
        std::iter::once(fact.subject.as_str())
            .chain(key.split('_'))
            .collect()
    }

    #[test]
    fn silent_change_fixture_declares_its_gold() {
        let script = silent_change();
        assert_eq!(script.name, "silent-change");
        assert_eq!(script.sessions.len(), 2);
        assert!(script
            .sessions
            .iter()
            .all(|session| session.transcript.is_none() && !session.turns.is_empty()));
        assert_eq!(script.facts.len(), 3);
        assert_eq!(
            script.superseded_pairs,
            vec![SupersededPair {
                old: "F01".into(),
                new: "F02".into(),
                retire_in: "s2".into(),
            }]
        );
        let state = |id: &str| script.fact(id).unwrap().end_state;
        assert_eq!(state("F01"), EndState::Superseded);
        assert_eq!(
            script.fact("F01").unwrap().superseded_in.as_deref(),
            Some("s2")
        );
        assert_eq!(state("F02"), EndState::Live);
        assert_eq!(state("F03"), EndState::Live);
        let live: Vec<(&str, bool)> = script
            .expect
            .iter()
            .map(|expected| (expected.fact.as_str(), expected.live))
            .collect();
        assert_eq!(live, [("F01", false), ("F02", true), ("F03", true)]);

        // Old and new are two values of one setting. The sibling is another
        // setting and is in no pair.
        let old = script.fact("F01").unwrap();
        let new = script.fact("F02").unwrap();
        let sibling = script.fact("F03").unwrap();
        assert_eq!((&old.subject, &old.relation), (&new.subject, &new.relation));
        assert_ne!(old.object, new.object);
        assert_ne!(
            (&sibling.subject, &sibling.relation),
            (&old.subject, &old.relation)
        );
        assert!(script
            .superseded_pairs
            .iter()
            .all(|pair| pair.old != sibling.id && pair.new != sibling.id));

        assert_eq!(script.questions.len(), 1);
        let question = &script.questions[0];
        assert_eq!(question.needs, ["F02"]);
        assert_eq!(question.absent, ["F01"]);
        assert_eq!(question.answer, new.object);
        // A current-value question. `stale` is left out and filled from the
        // pair, so the old value is the one the recall window is scanned for.
        assert_eq!(question.kind, QuestionKind::CurrentValue);
        assert!(question.stale.is_none());
        assert_eq!(
            script.stale_values(question).unwrap(),
            [old.object.as_str()]
        );
    }

    #[test]
    fn silent_change_cores_tell_the_two_values_apart() {
        let script = silent_change();
        let lines: Vec<String> = script
            .facts
            .iter()
            .map(|fact| format!("{} | {} | {}", fact.subject, fact.relation, fact.object))
            .collect();
        for fact in &script.facts {
            let pattern = fact.pattern.as_ref().expect("every fact has a match");
            assert!(!pattern.core_all.is_empty(), "{}", fact.id);
            let core: Vec<regex::Regex> = pattern
                .core_all
                .iter()
                .map(|source| compile_pattern(source).unwrap())
                .collect();
            // Each core matches its own gold record and no other fact's.
            for (other, line) in script.facts.iter().zip(&lines) {
                let hit = core.iter().all(|regex| regex.is_match(line));
                assert_eq!(hit, other.id == fact.id, "{} core on '{line}'", fact.id);
            }
        }
    }

    #[test]
    fn silent_change_s2_has_no_departure_cue_and_no_old_value() {
        let script = silent_change();
        let old = script.fact("F01").unwrap();
        let new = script.fact("F02").unwrap();
        let key = key_words(old);
        assert_eq!(key, ["client", "timeout"]);
        // "12 seconds", and the bare number on its own.
        let number = old.object.split_whitespace().next().unwrap();
        assert_eq!(number, "12");
        let s2 = script
            .sessions
            .iter()
            .find(|session| session.id == "s2")
            .unwrap();
        assert!(!s2.turns.is_empty());
        for turn in &s2.turns {
            assert_eq!(
                departure_cues(&turn.text, &key),
                Vec::<String>::new(),
                "{}",
                turn.text
            );
            for value in [old.object.as_str(), number] {
                assert!(
                    !has_phrase(&turn.text, value),
                    "s2 names the old value '{value}': {}",
                    turn.text
                );
            }
        }
        // s2 still states the new value.
        assert!(s2
            .turns
            .iter()
            .any(|turn| has_phrase(&turn.text, &new.object)));
    }

    #[test]
    fn the_s2_checks_catch_every_cue_and_the_old_value() {
        let key = ["client", "timeout"];
        assert!(departure_cues("The client timeout is 48 seconds.", &key).is_empty());
        for cue in DEPARTURE_CUES {
            let text = format!("The client timeout {cue} 48 seconds.");
            assert!(
                departure_cues(&text, &key).iter().any(|hit| hit == cue),
                "{text}"
            );
            let shouted = text.to_uppercase();
            assert!(
                departure_cues(&shouted, &key).iter().any(|hit| hit == cue),
                "{shouted}"
            );
        }
        assert_eq!(
            departure_cues("The new client timeout is 48 seconds.", &key),
            ["new client"]
        );
        assert_eq!(
            departure_cues("The New  Timeout is 48 seconds.", &key),
            ["new timeout"]
        );
        // Whole words only, and bare `is` is allowed.
        assert!(departure_cues(
            "A known, bold, unchanged client timeout is renewed at 48 seconds.",
            &key
        )
        .is_empty());
        assert!(departure_cues("A new batch size is 50 items.", &key).is_empty());

        assert!(has_phrase("The client timeout is 12 seconds.", "12"));
        assert!(has_phrase(
            "The client timeout is 12  Seconds.",
            "12 seconds"
        ));
        assert!(!has_phrase("The client timeout is 48 seconds.", "12"));
        assert!(!has_phrase("The client timeout is 120 seconds.", "12"));
    }

    fn open_loops() -> FactScript {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/writes/open-loops/script.toml");
        load_script(&path).unwrap()
    }

    /// The words that may reopen a fixed item, as the writer reads them.
    const REOPEN_CUES: &[&str] = &[
        "reopen",
        "reopened",
        "open again",
        "back to open",
        "not fixed",
    ];

    fn reopen_cues(text: &str) -> Vec<&'static str> {
        REOPEN_CUES
            .iter()
            .copied()
            .filter(|cue| has_phrase(text, cue))
            .collect()
    }

    #[test]
    fn open_loops_fixture_declares_its_gold() {
        let script = open_loops();
        assert_eq!(script.name, "open-loops");
        assert_eq!(script.project.as_deref(), Some("timber"));
        let ids: Vec<&str> = script.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2", "s3", "s4"]);
        assert!(script
            .sessions
            .iter()
            .all(|session| session.transcript.is_none() && !session.turns.is_empty()));

        let live: Vec<(&str, bool)> = script
            .expect
            .iter()
            .map(|expected| (expected.fact.as_str(), expected.live))
            .collect();
        assert_eq!(
            live,
            [
                ("A-open", true),
                ("B-open", false),
                ("B-fixed", true),
                ("C-open", true),
                ("C-fixed", false),
                ("D-fixed", true),
                ("D-open", false),
                ("S", true),
            ]
        );
        let state = |id: &str| script.fact(id).unwrap().end_state;
        assert_eq!(state("A-open"), EndState::Live);
        assert_eq!(state("B-open"), EndState::Superseded);
        assert_eq!(state("B-fixed"), EndState::Live);
        assert_eq!(state("C-open"), EndState::Live);
        assert_eq!(state("C-fixed"), EndState::Superseded);
        assert_eq!(state("D-fixed"), EndState::Live);
        assert_eq!(state("D-open"), EndState::Rejected);
        assert_eq!(state("S"), EndState::Live);
        assert_eq!(
            script.fact("B-open").unwrap().superseded_in.as_deref(),
            Some("s3")
        );
        assert_eq!(
            script.fact("C-fixed").unwrap().superseded_in.as_deref(),
            Some("s4")
        );
        assert_eq!(
            script.superseded_pairs,
            vec![
                SupersededPair {
                    old: "B-open".into(),
                    new: "B-fixed".into(),
                    retire_in: "s3".into(),
                },
                SupersededPair {
                    old: "C-fixed".into(),
                    new: "C-open".into(),
                    retire_in: "s4".into(),
                },
            ]
        );

        // Every item is an issue status, `open` or `fixed`. The sibling is a setting.
        for fact in script.facts.iter().filter(|fact| fact.id != "S") {
            assert_eq!(fact.relation, "issue_status", "{}", fact.id);
            assert_eq!(fact.subject_type, "case", "{}", fact.id);
            let value = fact.id.rsplit('-').next().unwrap();
            assert_eq!(fact.object, value, "{}", fact.id);
        }
        let sibling = script.fact("S").unwrap();
        assert_eq!(
            (sibling.relation.as_str(), sibling.object.as_str()),
            ("setting:upload_limit", "25 MB")
        );

        // One current-value question per item and one for the setting.
        let asked: Vec<(&str, Vec<&str>, Vec<&str>)> = script
            .questions
            .iter()
            .map(|question| {
                assert_eq!(
                    question.kind,
                    QuestionKind::CurrentValue,
                    "{}",
                    question.text
                );
                (
                    question.answer.as_str(),
                    question.needs.iter().map(String::as_str).collect(),
                    question.absent.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            asked,
            [
                ("open", vec!["A-open"], vec![]),
                ("fixed", vec!["B-fixed"], vec!["B-open"]),
                ("open", vec!["C-open"], vec!["C-fixed"]),
                ("fixed", vec!["D-fixed"], vec!["D-open"]),
                ("25 MB", vec!["S"], vec![]),
            ]
        );
        let stale: Vec<Vec<String>> = script
            .questions
            .iter()
            .map(|question| script.stale_values(question).unwrap())
            .collect();
        assert_eq!(
            stale,
            [
                vec![],
                vec!["open".to_string()],
                vec!["fixed".to_string()],
                vec!["open".to_string()],
                vec![],
            ]
        );
    }

    #[test]
    fn open_loops_cores_tell_every_fact_apart() {
        let script = open_loops();
        let lines: Vec<String> = script
            .facts
            .iter()
            .map(|fact| format!("{} | {} | {}", fact.subject, fact.relation, fact.object))
            .collect();
        for fact in &script.facts {
            let pattern = fact.pattern.as_ref().expect("every fact has a match");
            let core: Vec<regex::Regex> = pattern
                .core_all
                .iter()
                .map(|source| compile_pattern(source).unwrap())
                .collect();
            for (other, line) in script.facts.iter().zip(&lines) {
                let hit = core.iter().all(|regex| regex.is_match(line));
                assert_eq!(hit, other.id == fact.id, "{} core on '{line}'", fact.id);
            }
        }
    }

    #[test]
    fn open_loops_reopens_only_where_the_gold_reopens() {
        let script = open_loops();
        let turns: Vec<(&str, &ScriptTurn)> = script
            .sessions
            .iter()
            .flat_map(|session| {
                session
                    .turns
                    .iter()
                    .map(move |turn| (session.id.as_str(), turn))
            })
            .collect();
        for (session, turn) in &turns {
            let reopens = turn
                .claims
                .iter()
                .any(|claim| claim.op == ClaimOp::Supersede && claim.fact == "C-fixed");
            assert_eq!(
                !reopen_cues(&turn.text).is_empty(),
                reopens,
                "{session}: {}",
                turn.text
            );
        }
        // The s3 restatement names the fixed item, claims nothing, and states
        // no fix.
        let (_, restated) = turns
            .iter()
            .find(|(session, turn)| *session == "s3" && turn.claims.is_empty())
            .expect("restatement");
        let item = &script.fact("D-fixed").unwrap().subject;
        assert!(has_phrase(&restated.text, item), "{}", restated.text);
        for word in ["fix", "fixed", "pinned", "done", "answered", "resolved"] {
            assert!(
                !has_phrase(&restated.text, word),
                "{word}: {}",
                restated.text
            );
        }
        assert!(reopen_cues("The login is NOT fixed.") == ["not fixed"]);
        assert!(reopen_cues("Move it back to  open.") == ["back to open"]);
        assert!(reopen_cues("Reopening the guide.").is_empty());
    }

    fn write_transcript_script(dir: &Path, name: &str, date_line: &str, quote: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(
            &path,
            format!(
                r#"
name = "quote"

[[sessions]]
id = "s1"
{date_line}transcript = "sessions/s01.jsonl"

[[sessions.claims]]
op = "create"
fact = "F01"
quote = "{quote}"

[[facts]]
id = "F01"
subject = "linkshelf"
relation = "setting:python_version"
object = "3.12"
subject_type = "project"
object_type = "concept"

[[expect]]
fact = "F01"
live = true
"#
            ),
        )
        .unwrap();
        path
    }

    fn script_with(date_line: &str, turn: &str) -> String {
        format!(
            r#"
name = "edge"

[[sessions]]
id = "s1"
{date_line}

[[sessions.turns]]
role = "user"
text = "{turn}"

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
live = true
"#
        )
    }
}
