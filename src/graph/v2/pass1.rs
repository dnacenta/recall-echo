// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One structured pass over a session window.
//!
//! The provider is asked for JSON matching the schema below. The reply is
//! still checked here, because a backend may ignore the schema. A window
//! that is not the root object is retried once. One bad element is a drop,
//! so it does not fail the window.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::super::error::GraphError;
use super::super::ingest::IngestContext;
use super::super::llm::{LlmProvider, TokenUsage};
use super::super::llm_json::{self, JsonFailure};
use super::super::types::IngestionReport;
use super::super::GraphMemory;
use super::ground::{FieldCredit, SessionGround, ValueShape};
use super::input::{self, SessionInput, Turn, TurnRole};
use super::schema::{
    affects_target, filter_fact, map_subject, over_long, release_name, remap, Fact, Kind, Mention,
    Predicate, Role, DROP_CONFIDENCE, DROP_KEY_BINDING, DROP_OVERFLOW, DROP_PARSE, DROP_TOO_LONG,
    DROP_UNRESOLVED, DROP_VALUE_SUPPORT, DROP_VERSION, MAX_ITEMS,
};
use super::write;
use crate::graph::Provenance;

/// Pass-1 system prompt. Verbatim, with no trailing newline.
pub(crate) const PASS1_SYSTEM: &str = r#"You extract durable project memory from a coding-session transcript between a USER and a coding ASSISTANT.
Extract EVERY fact worth remembering in a future session, one fact per distinct value: decisions, chosen technologies and tools, settings and their values (ports, versions, paths, commands, limits, thresholds, formats), project rules and conventions, plans, and people's durable preferences. Bugs and releases go in their own sections (below).

Do NOT extract: greetings, small talk, moods, weather, holidays or personal chatter; one-off actions (ran tests, created a file, committed); tool names, file listings, test counts or command output; vague ideas the user says are not a plan.

FACTS (section "facts")
Entity types: project (the software being built), component (a part of the project: module, command, subsystem, file format, script, API), technology (an external language, library, framework, tool, service or database), person.
Predicates (subject -> object):
- uses: project|component -> technology. role = what it is used for: language, runtime, package_manager (uv, pip, npm, cargo), framework, cli_framework, web_framework, database, test_framework, linter, formatter, type_checker, build_tool, ci, hosting, library, other.
- part_of: component -> project|component.
- depends_on: component -> component.
- works_on: person -> project.
- setting: project|component|technology -> literal. key = short snake_case name (port, version, license, storage_path, test_command, line_length, timeout_seconds, max_concurrency, coverage_min, release_process ...). One current value per key.
- rule: project|component -> literal. A convention or constraint that must be followed, as a short imperative sentence.
- prefers: person -> literal. A durable working preference.
Use role "none" and key "" when they do not apply.
Numbers, versions, ports, paths, commands and thresholds are literals (setting values), never technologies.
The subject of project-wide facts is the PROJECT NAME given above the transcript. Use full proper names, never pronouns or "project".
status: decided = adopted, agreed or currently true; considered = proposed or discussed but not adopted; rejected = turned down, removed or abandoned.
replaces: when a decided fact REPLACES an earlier choice (switch from X to Y, X removed in favour of Y), put the old value X here and ALSO output X as a separate rejected fact. Otherwise "".

BUGS (section "bugs"): one entry per bug or problem: symptom = what went wrong (short), cause = why, fix = what was changed, status = open, fixed or wont_fix, component = the affected part or the project name. Leave cause or fix "" if not stated.

RELEASES (section "releases"): one entry per version that was released or is planned: version (e.g. 1.4.0), status = released or planned, contents = what it contains or will contain (short, comma separated).

Evidence (every entry): turn = the N of the [TN] turn that states it; quote = an exact verbatim copy of 3-20 consecutive words from that turn that contains the value (copy characters exactly).
confidence: 0.0-1.0 that this is a durable, correctly stated fact.

EXAMPLE (a different project)
Project: tidepool
[T0] USER: Let's build tidepool, a Go 1.22 service. Use chi for routing.
[T1] ASSISTANT: Sure. We could also look at gorilla/mux. Default port 3000?
[T2] USER: Port 9000. And never log request bodies.
[T3] ASSISTANT: Found a bug: the cache returned stale entries because the TTL was ignored. Fixed by checking expires_at on read.
[T4] USER: Let's move from Postgres to SQLite, Postgres is overkill here.
[T5] USER: Tag v1.4.0 with the SQLite move and the cache fix. Metrics export is planned for v1.5.
Output:
{"facts":[
{"subject":"tidepool","subject_type":"project","predicate":"uses","role":"language","key":"","object":"Go","object_type":"technology","status":"decided","replaces":"","turn":0,"quote":"a Go 1.22 service","confidence":0.9},
{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"go_version","object":"1.22","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"a Go 1.22 service","confidence":0.9},
{"subject":"tidepool","subject_type":"project","predicate":"uses","role":"web_framework","key":"","object":"chi","object_type":"technology","status":"decided","replaces":"","turn":0,"quote":"Use chi for routing","confidence":0.9},
{"subject":"tidepool","subject_type":"project","predicate":"uses","role":"web_framework","key":"","object":"gorilla/mux","object_type":"technology","status":"considered","replaces":"","turn":1,"quote":"We could also look at gorilla/mux","confidence":0.7},
{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"port","object":"9000","object_type":"literal","status":"decided","replaces":"","turn":2,"quote":"Port 9000.","confidence":0.9},
{"subject":"tidepool","subject_type":"project","predicate":"rule","role":"none","key":"","object":"Never log request bodies.","object_type":"literal","status":"decided","replaces":"","turn":2,"quote":"never log request bodies","confidence":0.9},
{"subject":"tidepool","subject_type":"project","predicate":"uses","role":"database","key":"","object":"SQLite","object_type":"technology","status":"decided","replaces":"PostgreSQL","turn":4,"quote":"move from Postgres to SQLite","confidence":0.9},
{"subject":"tidepool","subject_type":"project","predicate":"uses","role":"database","key":"","object":"PostgreSQL","object_type":"technology","status":"rejected","replaces":"","turn":4,"quote":"move from Postgres to SQLite","confidence":0.9}],
"bugs":[
{"symptom":"cache returned stale entries","cause":"the TTL was ignored","fix":"check expires_at on read","status":"fixed","component":"cache","turn":3,"quote":"the cache returned stale entries because the TTL was ignored","confidence":0.8}],
"releases":[
{"version":"1.4.0","status":"released","contents":"SQLite move, cache fix","turn":5,"quote":"Tag v1.4.0 with the SQLite move and the cache fix","confidence":0.9},
{"version":"1.5","status":"planned","contents":"metrics export","turn":5,"quote":"Metrics export is planned for v1.5","confidence":0.8}]}"#;

/// Output cap for one window.
const MAX_TOKENS: u32 = 4096;
/// Characters one window may hold. The reader splits a longer session.
const WINDOW_CHARS: usize = 48_000;

/// Closed shape of a pass-1 reply. No length, minimum or maximum keywords:
/// those are applied by the schema table, not by this object.
const PASS1_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["facts", "bugs", "releases"],
  "properties": {
    "facts": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["subject", "subject_type", "predicate", "role", "key", "object", "object_type", "status", "replaces", "turn", "quote", "confidence"],
        "properties": {
          "subject": {"type": "string"},
          "subject_type": {"type": "string", "enum": ["project", "component", "technology", "person"]},
          "predicate": {"type": "string", "enum": ["uses", "part_of", "depends_on", "works_on", "setting", "rule", "prefers"]},
          "role": {"type": "string", "enum": ["language", "runtime", "package_manager", "framework", "cli_framework", "web_framework", "database", "test_framework", "linter", "formatter", "type_checker", "build_tool", "ci", "hosting", "library", "other", "none"]},
          "key": {"type": "string"},
          "object": {"type": "string"},
          "object_type": {"type": "string", "enum": ["project", "component", "technology", "literal"]},
          "status": {"type": "string", "enum": ["decided", "considered", "rejected"]},
          "replaces": {"type": "string"},
          "turn": {"type": "integer"},
          "quote": {"type": "string"},
          "confidence": {"type": "number"}
        }
      }
    },
    "bugs": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["symptom", "cause", "fix", "status", "component", "turn", "quote", "confidence"],
        "properties": {
          "symptom": {"type": "string"},
          "cause": {"type": "string"},
          "fix": {"type": "string"},
          "status": {"type": "string", "enum": ["open", "fixed", "wont_fix"]},
          "component": {"type": "string"},
          "turn": {"type": "integer"},
          "quote": {"type": "string"},
          "confidence": {"type": "number"}
        }
      }
    },
    "releases": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["version", "status", "contents", "turn", "quote", "confidence"],
        "properties": {
          "version": {"type": "string"},
          "status": {"type": "string", "enum": ["released", "planned"]},
          "contents": {"type": "string"},
          "turn": {"type": "integer"},
          "quote": {"type": "string"},
          "confidence": {"type": "number"}
        }
      }
    }
  }
}"#;

/// The user message for one window.
#[must_use]
pub(crate) fn user_message(project: &str, date: &str, window: &str) -> String {
    format!(
        "Project: {project}\nSession date: {date}\nTranscript:\n{window}\n\nExtract the durable facts, bugs and releases as JSON."
    )
}

/// Read one session and write what the schema table keeps.
///
/// The store is checked before any provider call. A window whose reply is
/// not the root object is sent once more; the second failure is named on
/// the report and that window writes nothing. The function returns `Ok` so
/// a caller that retries on `Err` does not spend a third call.
pub(crate) async fn extract_session(
    gm: &GraphMemory,
    memory_dir: &Path,
    log: u32,
    ctx: &IngestContext,
    llm: &dyn LlmProvider,
) -> Result<IngestionReport, GraphError> {
    write::ensure_v2_store(gm).await?;
    let session = input::load_session(memory_dir, log)?;
    let windows = input::window(&session, WINDOW_CHARS);
    let mut report = IngestionReport::default();
    report.windows = u32::try_from(windows.len()).unwrap_or(u32::MAX);
    report.chunks_total = report.windows;

    let ground = SessionGround::prepare(&session.turns);
    let mut facts = Vec::new();
    for window in &windows {
        match call_window(llm, &session, window, &mut report).await {
            Ok(reply) => facts.extend(collect_facts(&reply, &session, &ground, &mut report)),
            Err(reason) => {
                report.errors.push(reason);
                report.chunks_failed += 1;
            }
        }
    }
    // Overlap repeats the last two turns. The same fact, session, turn and
    // quote is one write: a second copy must not corroborate the fact in the
    // session that created it.
    let facts = dedupe_facts(facts);

    if !facts.is_empty() {
        let write_ctx = write::WriteCtx::new(
            llm,
            &session.project,
            ctx.session_id(),
            &session.date,
            &session.turns,
        );
        let stats = write::write_facts(gm, &write_ctx, &facts).await?;
        report.entities_created = stats.entities_created;
        report.relationships_created = stats.edges;
        report.slots_written = stats.slots;
        report.llm_calls += stats.resolver_calls;
        report.resolve = stats.resolve;
        report.retirements = stats.retirements;
        report.retirements_skipped = stats.retirements_skipped;
        if stats.unresolved > 0 {
            *report.drops.entry(DROP_UNRESOLVED.to_string()).or_insert(0) += stats.unresolved;
        }
    }
    Ok(report)
}

async fn call_window(
    llm: &dyn LlmProvider,
    session: &SessionInput,
    window: &str,
    report: &mut IngestionReport,
) -> Result<Map<String, Value>, String> {
    let message = user_message(&session.project.name, &session.date, window);
    let mut last = String::new();
    for _ in 0..2 {
        report.llm_calls += 1;
        let completion = match llm
            .complete_structured(PASS1_SYSTEM, &message, pass1_schema(), MAX_TOKENS)
            .await
        {
            Ok(completion) => completion,
            Err(err) => return Err(format!("parse failure: {err}")),
        };
        bill(report, completion.usage, &message, &completion.text);
        match parsed_root(&completion.text) {
            Ok(object) => return Ok(object),
            Err(reason) => last = reason,
        }
    }
    Err(last)
}

fn bill(report: &mut IngestionReport, usage: Option<TokenUsage>, message: &str, reply: &str) {
    match usage {
        Some(usage) => report.measured_tokens += usage.total(),
        None => {
            let chars = PASS1_SYSTEM.len() + message.len() + reply.len();
            report.estimated_tokens += u64::try_from(chars / 4).unwrap_or(0);
        }
    }
}

/// The first JSON object, salvaged when the reply was cut off, then checked
/// against the root of [`PASS1_JSON_SCHEMA`].
fn parsed_root(text: &str) -> Result<Map<String, Value>, String> {
    let object = match llm_json::first_json_object(text) {
        Ok(object) => object,
        Err(JsonFailure::Truncated { .. }) => llm_json::salvage_truncated(text)
            .ok_or_else(|| "parse failure: truncated JSON".to_string())?,
        Err(JsonFailure::NoJson) => return Err("parse failure: no JSON object".to_string()),
        Err(JsonFailure::InvalidJson { .. } | JsonFailure::Shape { .. }) => {
            return Err("parse failure: invalid JSON".to_string());
        }
    };
    if root_shape(&object) {
        Ok(object)
    } else {
        Err("parse failure: reply is not the pass-1 object".to_string())
    }
}

#[cfg(test)]
fn facts_from_reply(
    reply: &Map<String, Value>,
    session: &SessionInput,
    report: &mut IngestionReport,
) -> Vec<Fact> {
    let ground = SessionGround::prepare(&session.turns);
    collect_facts(reply, session, &ground, report)
}

fn collect_facts(
    reply: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
    report: &mut IngestionReport,
) -> Vec<Fact> {
    let mut facts = Vec::new();
    for section in ["facts", "bugs", "releases"] {
        let Some(items) = reply.get(section).and_then(Value::as_array) else {
            continue;
        };
        for (index, item) in items.iter().enumerate() {
            report.candidates += 1;
            if index >= MAX_ITEMS {
                count_drop(report, DROP_OVERFLOW);
                continue;
            }
            match accept_item(section, item, session, ground, report) {
                Ok(batch) => {
                    report.kept += 1;
                    facts.extend(batch);
                }
                Err(reason) => count_drop(report, reason),
            }
        }
    }
    facts
}

fn accept_item(
    section: &str,
    item: &Value,
    session: &SessionInput,
    ground: &SessionGround,
    report: &mut IngestionReport,
) -> Result<Vec<Fact>, &'static str> {
    if !element_matches(section, item) {
        return Err(DROP_PARSE);
    }
    let object = item.as_object().ok_or(DROP_PARSE)?;
    match section {
        "facts" => accept_fact(object, session, ground).map(|fact| vec![fact]),
        "bugs" => expand_bug(object, session, ground, report),
        "releases" => expand_release(object, session, ground, report),
        _ => Err(DROP_PARSE),
    }
}

fn accept_fact(
    object: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
) -> Result<Fact, &'static str> {
    let cited = cite_object(object, ground)?;
    let mut mention = mention_from(object, &session.turns)?;
    mention.turn = cited.turn;
    mention.provenance = cited.role;
    let mut fact = remap(mention)?;
    fact.replaces = object
        .get("replaces")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    map_subject(&mut fact, &session.project);
    let shape = if fact.object_kind.is_some() {
        ValueShape::Entity
    } else {
        ValueShape::Literal
    };
    if !ground.supports_value(&fact.object_name, fact.turn, &fact.quote, shape) {
        return Err(DROP_VALUE_SUPPORT);
    }
    if fact.predicate == Predicate::Setting
        && !ground.key_bound(&fact.key, &fact.object_name, fact.turn, &fact.quote)
    {
        return Err(DROP_KEY_BINDING);
    }
    if !ground.replaces_grounded(&fact.replaces, &fact.status, fact.turn) {
        fact.replaces.clear();
    }
    filter_fact(&fact)?;
    Ok(fact)
}

fn mention_from(object: &Map<String, Value>, turns: &[Turn]) -> Result<Mention, &'static str> {
    let subject_kind = Kind::parse(text(object, "subject_type")?).ok_or(DROP_PARSE)?;
    let predicate = Predicate::parse(text(object, "predicate")?).ok_or(DROP_PARSE)?;
    let role = Role::parse(text(object, "role")?).ok_or(DROP_PARSE)?;
    let object_kind = match text(object, "object_type")? {
        "literal" => None,
        other => Some(Kind::parse(other).ok_or(DROP_PARSE)?),
    };
    let subject = text(object, "subject")?;
    let key = text(object, "key")?;
    if over_long(subject) || over_long(key) {
        return Err(DROP_TOO_LONG);
    }
    let turn = turn_of(object)?;
    Ok(Mention {
        subject: subject.to_string(),
        subject_kind,
        predicate,
        role,
        key: key.to_string(),
        object: text(object, "object")?.to_string(),
        object_kind,
        status: text(object, "status")?.to_string(),
        turn,
        quote: text(object, "quote")?.to_string(),
        confidence: confidence_of(object)?,
        provenance: role_at(turn, turns),
    })
}

fn expand_bug(
    object: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
    report: &mut IngestionReport,
) -> Result<Vec<Fact>, &'static str> {
    let cited = cite_object(object, ground)?;
    let confidence = confidence_of(object)?;
    if confidence < 0.6 {
        return Err(DROP_CONFIDENCE);
    }
    let symptom = text(object, "symptom")?.trim();
    if symptom.is_empty() {
        return Err(DROP_PARSE);
    }
    let cause = text(object, "cause")?.trim();
    let fix = text(object, "fix")?.trim();
    if over_long(symptom) || over_long(cause) || over_long(fix) {
        return Err(DROP_TOO_LONG);
    }
    let mut carrier = carrier(object, confidence, &cited)?;
    // Cause and fix are slots of the symptom, the same subject as the symptom slot.
    carrier.subject_name = symptom.to_string();
    carrier.subject_kind = Kind::Issue;
    let (target_name, target_kind) = affects_target(text(object, "component")?, &session.project);
    let mut facts = vec![slot_fact(
        symptom,
        Kind::Issue,
        Predicate::IssueSymptom,
        symptom,
        &carrier,
    )];
    push_field(
        &mut facts,
        report,
        ground,
        cause,
        &carrier,
        Predicate::IssueCause,
    );
    push_field(
        &mut facts,
        report,
        ground,
        fix,
        &carrier,
        Predicate::IssueFix,
    );
    // The model's open/fixed/wont_fix is not stored. A fix that stayed on the
    // record is `fixed`; a fix that was never stated, or that failed support,
    // is not on the record, so the status is `open`.
    let kept_fix = facts
        .iter()
        .any(|fact| fact.predicate == Predicate::IssueFix);
    let status_value = if kept_fix { "fixed" } else { "open" };
    facts.push(inferred(slot_fact(
        symptom,
        Kind::Issue,
        Predicate::IssueStatus,
        status_value,
        &carrier,
    )));
    if !target_name.is_empty() {
        facts.push(edge_fact(
            symptom,
            Kind::Issue,
            Predicate::Affects,
            &target_name,
            Some(target_kind),
            &carrier,
        ));
    }
    Ok(facts)
}

fn expand_release(
    object: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
    report: &mut IngestionReport,
) -> Result<Vec<Fact>, &'static str> {
    let cited = cite_object(object, ground)?;
    let confidence = confidence_of(object)?;
    if confidence < 0.6 {
        return Err(DROP_CONFIDENCE);
    }
    let mut carrier = carrier(object, confidence, &cited)?;
    let turn_text = ground.turn_text(carrier.turn).unwrap_or("");
    let name = release_name(&session.project.name, text(object, "version")?, turn_text)
        .ok_or(DROP_VERSION)?;
    let contents = text(object, "contents")?.trim();
    if over_long(contents) {
        return Err(DROP_TOO_LONG);
    }
    // Status, contents and part_of all belong to the release, not the carrier default.
    carrier.subject_name = name.clone();
    carrier.subject_kind = Kind::Release;
    let mut facts = vec![inferred(slot_fact(
        &name,
        Kind::Release,
        Predicate::ReleaseStatus,
        text(object, "status")?,
        &carrier,
    ))];
    push_field(
        &mut facts,
        report,
        ground,
        contents,
        &carrier,
        Predicate::ReleaseContents,
    );
    if !session.project.name.is_empty() {
        facts.push(inferred(edge_fact(
            &name,
            Kind::Release,
            Predicate::PartOf,
            &session.project.name,
            Some(Kind::Project),
            &carrier,
        )));
    }
    Ok(facts)
}

fn cite_object(
    object: &Map<String, Value>,
    ground: &SessionGround,
) -> Result<super::ground::Cited, &'static str> {
    ground.cite(turn_of(object)?, text(object, "quote")?)
}

fn push_field(
    facts: &mut Vec<Fact>,
    report: &mut IngestionReport,
    ground: &SessionGround,
    value: &str,
    carrier: &Fact,
    predicate: Predicate,
) {
    if value.is_empty() {
        return;
    }
    let mut fact = slot_fact(
        &carrier.subject_name,
        carrier.subject_kind,
        predicate,
        value,
        carrier,
    );
    match ground.credit_field(value, carrier.turn, &carrier.quote) {
        FieldCredit::Turn => facts.push(fact),
        FieldCredit::Session => {
            fact.provenance = Provenance::SelfGenerated;
            facts.push(fact);
        }
        FieldCredit::Drop => count_drop(report, DROP_VALUE_SUPPORT),
    }
}

fn inferred(mut fact: Fact) -> Fact {
    fact.provenance = Provenance::SelfGenerated;
    fact
}

fn carrier(
    object: &Map<String, Value>,
    confidence: f64,
    cited: &super::ground::Cited,
) -> Result<Fact, &'static str> {
    Ok(Fact {
        subject_name: String::new(),
        subject_kind: Kind::Issue,
        predicate: Predicate::IssueSymptom,
        role: None,
        key: String::new(),
        object_name: String::new(),
        object_kind: None,
        status: "decided".to_string(),
        turn: cited.turn,
        quote: text(object, "quote")?.to_string(),
        confidence,
        provenance: from_role(cited.role),
        replaces: String::new(),
    })
}

fn from_role(role: TurnRole) -> Provenance {
    match role {
        TurnRole::User => Provenance::User,
        TurnRole::Assistant => Provenance::Assistant,
    }
}

fn slot_fact(subject: &str, kind: Kind, predicate: Predicate, value: &str, carrier: &Fact) -> Fact {
    Fact {
        subject_name: subject.to_string(),
        subject_kind: kind,
        predicate,
        role: None,
        key: String::new(),
        object_name: value.to_string(),
        object_kind: None,
        status: "decided".to_string(),
        turn: carrier.turn,
        quote: carrier.quote.clone(),
        confidence: carrier.confidence,
        provenance: carrier.provenance,
        replaces: String::new(),
    }
}

fn edge_fact(
    subject: &str,
    kind: Kind,
    predicate: Predicate,
    object_name: &str,
    object_kind: Option<Kind>,
    carrier: &Fact,
) -> Fact {
    Fact {
        subject_name: subject.to_string(),
        subject_kind: kind,
        predicate,
        role: None,
        key: String::new(),
        object_name: object_name.to_string(),
        object_kind,
        status: "decided".to_string(),
        turn: carrier.turn,
        quote: carrier.quote.clone(),
        confidence: carrier.confidence,
        provenance: carrier.provenance,
        replaces: String::new(),
    }
}

fn role_at(index: usize, turns: &[Turn]) -> TurnRole {
    turns
        .iter()
        .find(|turn| turn.index == index)
        .map(|turn| turn.role)
        .unwrap_or(TurnRole::User)
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, &'static str> {
    object.get(key).and_then(Value::as_str).ok_or(DROP_PARSE)
}

fn confidence_of(object: &Map<String, Value>) -> Result<f64, &'static str> {
    object
        .get("confidence")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or(DROP_PARSE)
}

fn turn_of(object: &Map<String, Value>) -> Result<usize, &'static str> {
    let value = object.get("turn").ok_or(DROP_PARSE)?;
    if let Some(number) = value.as_u64() {
        return usize::try_from(number).map_err(|_| DROP_PARSE);
    }
    if let Some(number) = value.as_i64() {
        return usize::try_from(number).map_err(|_| DROP_PARSE);
    }
    Err(DROP_PARSE)
}

/// One write per fact identity, session, turn and quote. The session is the
/// whole call, so it is not part of the key.
fn dedupe_facts(facts: Vec<Fact>) -> Vec<Fact> {
    let mut seen = HashSet::new();
    let mut kept = Vec::with_capacity(facts.len());
    for fact in facts {
        if seen.insert(fact_identity(&fact)) {
            kept.push(fact);
        }
    }
    kept
}

fn fact_identity(
    fact: &Fact,
) -> (
    String,
    &'static str,
    &'static str,
    &'static str,
    String,
    String,
    &'static str,
    String,
    usize,
    String,
) {
    (
        fact.subject_name.clone(),
        fact.subject_kind.as_str(),
        fact.predicate.as_str(),
        fact.role.map(Role::as_str).unwrap_or(""),
        fact.key.clone(),
        fact.object_name.clone(),
        fact.object_kind.map(Kind::as_str).unwrap_or(""),
        fact.status.clone(),
        fact.turn,
        fact.quote.clone(),
    )
}

fn count_drop(report: &mut IngestionReport, reason: &str) {
    *report.drops.entry(reason.to_string()).or_insert(0) += 1;
}

fn pass1_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(PASS1_JSON_SCHEMA)
            .unwrap_or_else(|err| panic!("pass-1 schema is not JSON: {err}"))
    })
}

fn root_shape(object: &Map<String, Value>) -> bool {
    let schema = pass1_schema();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };
    if closed(schema) && object.keys().any(|key| !properties.contains_key(key)) {
        return false;
    }
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return false;
    };
    required.iter().all(|name| {
        let Some(name) = name.as_str() else {
            return false;
        };
        properties
            .get(name)
            .and_then(|property| property.get("type"))
            .and_then(Value::as_str)
            == Some("array")
            && object.get(name).is_some_and(Value::is_array)
    })
}

fn element_matches(section: &str, item: &Value) -> bool {
    let Some(schema) = pass1_schema()
        .get("properties")
        .and_then(|properties| properties.get(section))
        .and_then(|property| property.get("items"))
    else {
        return false;
    };
    let Some(object) = item.as_object() else {
        return false;
    };
    object_matches(object, schema)
}

fn object_matches(object: &Map<String, Value>, schema: &Value) -> bool {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return false;
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };
    if closed(schema) && object.keys().any(|key| !properties.contains_key(key)) {
        return false;
    }
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[]);
    if required.iter().any(|name| {
        // `replaces` stays in the schema the provider is asked for. A reply
        // that omits it is still the fact: the writer treats absence as "".
        name.as_str()
            .is_none_or(|name| name != "replaces" && !object.contains_key(name))
    }) {
        return false;
    }
    object.iter().all(|(key, value)| {
        properties
            .get(key)
            .is_some_and(|property| value_matches(value, property))
    })
}

fn closed(schema: &Value) -> bool {
    schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
}

fn value_matches(value: &Value, schema: &Value) -> bool {
    let type_ok = match schema.get("type").and_then(Value::as_str) {
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("object") => value
            .as_object()
            .is_some_and(|object| object_matches(object, schema)),
        Some("array") => value.is_array(),
        _ => false,
    };
    if !type_ok {
        return false;
    }
    match schema.get("enum").and_then(Value::as_array) {
        Some(variants) => variants.iter().any(|variant| variant == value),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::v2::input::{ProjectIdentity, Turn, TurnRole};
    use crate::graph::v2::schema::{
        Kind, Predicate, DROP_OVERFLOW, DROP_QUOTE, DROP_TOO_LONG, DROP_VALUE_SUPPORT,
        DROP_VERSION, MAX_ITEMS,
    };
    use crate::graph::Provenance;
    use serde_json::Value;

    fn session(turns: Vec<Turn>) -> SessionInput {
        SessionInput {
            session_id: "s".into(),
            date: "2026-01-02".into(),
            project: ProjectIdentity {
                name: "tidepool".into(),
                aliases: vec!["tidepool".into()],
            },
            turns,
            tool_lines: Vec::new(),
        }
    }

    fn turns() -> Vec<Turn> {
        vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "Ship the shelf.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "Port 9000 is chosen. Tag v1.4.0 today.".into(),
            },
        ]
    }

    #[tokio::test]
    async fn pass1_calls_complete_structured() {
        struct Seen(std::sync::Mutex<Option<Value>>);

        #[async_trait::async_trait]
        impl LlmProvider for Seen {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Err(GraphError::Llm("plain complete was called".into()))
            }

            async fn complete_structured(
                &self,
                system: &str,
                user: &str,
                schema: &Value,
                max_tokens: u32,
            ) -> Result<crate::graph::llm::Completion, GraphError> {
                assert_eq!(system, PASS1_SYSTEM);
                assert!(user.contains("tidepool"), "{user}");
                assert_eq!(max_tokens, MAX_TOKENS);
                *self.0.lock().expect("lock") = Some(schema.clone());
                Ok(crate::graph::llm::Completion::unmeasured(
                    r#"{"facts":[],"bugs":[],"releases":[]}"#,
                ))
            }
        }

        let seen = Seen(std::sync::Mutex::new(None));
        let mut report = IngestionReport::default();
        let object = call_window(&seen, &session(turns()), "[T0] USER: hi", &mut report)
            .await
            .expect("structured reply");
        assert!(object.get("facts").is_some());
        assert_eq!(seen.0.lock().expect("lock").as_ref(), Some(pass1_schema()));
        assert_eq!(report.llm_calls, 1);
    }

    fn fact_json(turn: usize, quote: &str) -> Value {
        serde_json::json!({
            "subject": "tidepool",
            "subject_type": "project",
            "predicate": "setting",
            "role": "none",
            "key": "port",
            "object": "9000",
            "object_type": "literal",
            "status": "decided",
            "replaces": "",
            "turn": turn,
            "quote": quote,
            "confidence": 0.9
        })
    }

    #[test]
    fn parses_reply_and_recites_quote() {
        let example = PASS1_SYSTEM
            .split_once("Output:\n")
            .expect("prompt example")
            .1;
        let example = parsed_root(example).expect("prompt example matches the schema");
        assert!(element_matches("facts", &example["facts"][0]));
        assert!(element_matches("bugs", &example["bugs"][0]));
        assert!(element_matches("releases", &example["releases"][0]));
        let mut extra = example["facts"][0].clone();
        extra
            .as_object_mut()
            .expect("object")
            .insert("note".into(), Value::String("no".into()));
        assert!(!element_matches("facts", &extra));

        let reply = serde_json::json!({
            "facts": [
                fact_json(0, "Ship the shelf."),
                fact_json(0, "Port 9000 is chosen.")
            ],
            "bugs": [],
            "releases": []
        });
        let reply = reply.as_object().expect("object").clone();
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(&reply, &session(turns()), &mut report);
        assert_eq!(report.candidates, 2);
        assert_eq!(report.kept, 1);
        // 9000 is not in "Ship the shelf.", so that fact fails value support.
        // The other quote lives in the assistant turn, so it is re-cited.
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_eq!(facts[0].turn, 1);
        assert_eq!(facts[0].provenance, Provenance::Assistant);
        assert_eq!(facts[0].predicate, Predicate::Setting);

        let mut bare = fact_json(1, "Port 9000 is chosen.");
        bare.as_object_mut().expect("object").remove("replaces");
        let reply = serde_json::json!({
            "facts": [bare],
            "bugs": [],
            "releases": []
        });
        let reply = reply.as_object().expect("object").clone();
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(&reply, &session(turns()), &mut report);
        assert_eq!(facts.len(), 1, "a reply without replaces is kept");
        assert!(facts[0].replaces.is_empty());
        assert_eq!(facts[0].key, "port");
        assert_eq!(facts[0].object_name, "9000");
        assert_eq!(facts[0].provenance, Provenance::Assistant);

        let mut same_speaker = turns();
        same_speaker.push(Turn {
            index: 2,
            role: TurnRole::User,
            text: "The chosen port is nine thousand.".into(),
        });
        let mut moved_fact = fact_json(0, "The chosen port is nine thousand.");
        moved_fact["object"] = Value::String("thousand".into());
        let moved = serde_json::json!({
            "facts": [moved_fact],
            "bugs": [],
            "releases": []
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            moved.as_object().expect("object"),
            &session(same_speaker),
            &mut report,
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].turn, 2);
        assert_eq!(facts[0].provenance, Provenance::User);
        assert_eq!(facts[0].object_name, "thousand");

        let empty = serde_json::json!({
            "facts": [fact_json(0, "")],
            "bugs": [],
            "releases": []
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            empty.as_object().expect("object"),
            &session(turns()),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1));
    }

    #[test]
    fn long_fields_and_extra_items_are_counted_drops() {
        let long = "x".repeat(301);
        let mut subject = fact_json(0, "Ship the shelf.");
        subject["subject"] = Value::String(long.clone());
        let mut key = fact_json(0, "Ship the shelf.");
        key["key"] = Value::String(long.clone());
        let mut bug = serde_json::json!({
            "symptom": long,
            "cause": "",
            "fix": "",
            "status": "open",
            "component": "cache",
            "turn": 0,
            "quote": "Ship the shelf.",
            "confidence": 0.9
        });
        bug["cause"] = Value::String("y".repeat(301));
        let mut release = serde_json::json!({
            "version": "1.4.0",
            "status": "released",
            "contents": "z".repeat(301),
            "turn": 1,
            "quote": "Tag v1.4.0 today.",
            "confidence": 0.9
        });
        let _ = &mut release;
        let mut facts = Vec::new();
        facts.push(fact_json(1, "Port 9000 is chosen."));
        for _ in 0..MAX_ITEMS {
            facts.push(fact_json(1, "Port 9000 is chosen."));
        }
        let reply = serde_json::json!({
            "facts": [subject, key],
            "bugs": [bug],
            "releases": [release]
        });
        let mut report = IngestionReport::default();
        let kept = facts_from_reply(
            reply.as_object().expect("object"),
            &session(turns()),
            &mut report,
        );
        assert!(kept.is_empty());
        assert_eq!(report.drops.get(DROP_TOO_LONG).copied(), Some(4));

        let reply = serde_json::json!({
            "facts": facts,
            "bugs": [],
            "releases": []
        });
        let mut report = IngestionReport::default();
        let kept = facts_from_reply(
            reply.as_object().expect("object"),
            &session(turns()),
            &mut report,
        );
        assert_eq!(kept.len(), MAX_ITEMS);
        assert_eq!(report.candidates, MAX_ITEMS as u32 + 1);
        assert_eq!(report.drops.get(DROP_OVERFLOW).copied(), Some(1));
    }

    #[test]
    fn release_version_must_be_in_the_turn() {
        let reply = serde_json::json!({
            "facts": [],
            "bugs": [],
            "releases": [
                {
                    "version": "9.9.9",
                    "status": "released",
                    "contents": "nothing",
                    "turn": 1,
                    "quote": "Tag v1.4.0 today.",
                    "confidence": 0.9
                },
                {
                    "version": "1.4.0",
                    "status": "planned",
                    "contents": "the shelf",
                    "turn": 1,
                    "quote": "Tag v1.4.0 today.",
                    "confidence": 0.9
                }
            ]
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("object"),
            &session(turns()),
            &mut report,
        );
        assert_eq!(report.drops.get(DROP_VERSION).copied(), Some(1));
        assert_eq!(report.kept, 1);
        assert!(facts
            .iter()
            .any(|fact| fact.subject_name == "tidepool v1.4.0"));
        let status = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::ReleaseStatus)
            .expect("status slot");
        assert_eq!(status.object_name, "planned");
        assert_eq!(status.status, "decided");
        assert!(facts
            .iter()
            .any(|fact| { fact.predicate == Predicate::PartOf && fact.object_name == "tidepool" }));
        let contents = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::ReleaseContents)
            .expect("contents slot");
        assert_eq!(contents.subject_name, "tidepool v1.4.0");
        assert_eq!(contents.subject_kind, Kind::Release);
        // "shelf" is not in the cited turn. It is in another turn, so the
        // field is kept and the provenance is inferred.
        assert_eq!(contents.provenance, Provenance::SelfGenerated);
        assert_eq!(status.provenance, Provenance::SelfGenerated);
    }

    #[test]
    fn unsupported_cause_is_dropped_and_the_bug_kept() {
        let turns = vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "The cache returned stale entries.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "The TTL was ignored in the reader.".into(),
            },
        ];
        let reply = serde_json::json!({
            "facts": [],
            "bugs": [{
                "symptom": "stale entries",
                "cause": "the TTL was ignored",
                "fix": "invented remedy nowhere",
                "status": "fixed",
                "component": "cache",
                "turn": 0,
                "quote": "The cache returned stale entries.",
                "confidence": 0.9
            }],
            "releases": []
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("object"),
            &session(turns),
            &mut report,
        );
        assert_eq!(report.kept, 1);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        let symptom = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .expect("symptom");
        assert_eq!(symptom.provenance, Provenance::User);
        assert_eq!(symptom.subject_name, "stale entries");
        let cause = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueCause)
            .expect("cause");
        assert_eq!(cause.object_name, "the TTL was ignored");
        assert_eq!(cause.subject_name, "stale entries");
        assert_eq!(cause.provenance, Provenance::SelfGenerated);
        assert!(facts
            .iter()
            .all(|fact| fact.predicate != Predicate::IssueFix));
        let status = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueStatus)
            .expect("status");
        assert_eq!(status.object_name, "open");
        assert_eq!(status.provenance, Provenance::SelfGenerated);
        let affects = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::Affects)
            .expect("affects");
        assert_eq!(affects.object_name, "cache");
        assert_eq!(affects.provenance, Provenance::User);
        assert_eq!(affects.subject_kind, Kind::Issue);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn overlap_does_not_corroborate_the_creating_session() {
        use std::sync::atomic::{AtomicU32, Ordering};

        use crate::conversation::ConversationEntry;
        use crate::graph::confidence::{Evidence, ExtractionContext};

        let quote = "the port is 9000";
        let padding = "x".repeat(15_000);
        let quoted = format!("{quote}. {padding}");
        let filler = "y".repeat(15_000);
        let entries = vec![
            ConversationEntry::UserMessage(filler.clone()),
            ConversationEntry::UserMessage(filler.clone()),
            ConversationEntry::UserMessage(quoted),
            ConversationEntry::UserMessage(filler),
        ];
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        input::write_turns_sidecar(memory, 1, "s", "2026-01-02T00:00:00Z", None, &entries).unwrap();
        let session = input::load_session(memory, 1).unwrap();
        let windows = input::window(&session, WINDOW_CHARS);
        assert!(
            windows.len() > 1,
            "session must exceed the window budget, got {} chars",
            windows
                .iter()
                .map(|window| window.chars().count())
                .sum::<usize>()
        );
        assert!(
            windows
                .iter()
                .filter(|window| window.contains(quote))
                .count()
                > 1,
            "the quoted turn must sit in the overlap"
        );

        struct HitProvider {
            calls: AtomicU32,
            hits: AtomicU32,
        }
        #[async_trait::async_trait]
        impl LlmProvider for HitProvider {
            async fn complete(
                &self,
                _: &str,
                user_message: &str,
                _: u32,
            ) -> Result<String, GraphError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if !user_message.contains("the port is 9000") {
                    return Ok(r#"{"facts":[],"bugs":[],"releases":[]}"#.to_string());
                }
                self.hits.fetch_add(1, Ordering::SeqCst);
                let turn = turn_index_of(user_message, "the port is 9000");
                Ok(format!(
                    r#"{{"facts":[{{"subject":"shelf","subject_type":"project","predicate":"setting","role":"none","key":"port","object":"9000","object_type":"literal","status":"decided","replaces":"","turn":{turn},"quote":"the port is 9000","confidence":0.9}}],"bugs":[],"releases":[]}}"#
                ))
            }
        }
        fn turn_index_of(window: &str, needle: &str) -> usize {
            let mut current = 0;
            for line in window.lines() {
                if let Some(rest) = line.strip_prefix("[T") {
                    if let Some((num, _)) = rest.split_once(']') {
                        if let Ok(index) = num.parse() {
                            current = index;
                        }
                    }
                }
                if line.contains(needle) {
                    return current;
                }
            }
            0
        }

        let provider = HitProvider {
            calls: AtomicU32::new(0),
            hits: AtomicU32::new(0),
        };
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        let ctx = IngestContext::new("s", Some(1));
        let report = extract_session(&gm, memory, 1, &ctx, &provider)
            .await
            .unwrap();
        assert!(report.windows > 1);
        assert_eq!(report.llm_calls, report.windows);
        assert!(provider.hits.load(Ordering::SeqCst) > 1);
        assert_eq!(report.slots_written, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let shelf = entities
            .iter()
            .find(|entity| entity.name == "shelf")
            .expect("shelf");
        let slots = shelf
            .attributes
            .as_ref()
            .and_then(|attrs| attrs.get("v2"))
            .and_then(|v2| v2.get("slots"))
            .and_then(|slots| slots.as_array())
            .expect("slots");
        assert_eq!(slots.len(), 1);
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let alpha = slots[0]["alpha"].as_f64().unwrap();
        let beta = slots[0]["beta"].as_f64().unwrap();
        assert!((alpha - prior.alpha()).abs() < 1e-9, "alpha {alpha}");
        assert!((beta - prior.beta()).abs() < 1e-9, "beta {beta}");
        assert_eq!(slots[0]["evidence"].as_array().unwrap().len(), 1);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn malformed_reply_retries_once_then_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::create_dir_all(memory.join("conversations")).unwrap();
        std::fs::write(
            memory.join("conversations/conversation-001.md"),
            "---\nlog: 1\ndate: \"2026-01-02T00:00:00Z\"\nsession_id: \"s\"\n---\n\n\
             ### User\n\nShip the shelf.\n",
        )
        .unwrap();
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        let ctx = IngestContext::new("s", Some(1));
        let report = extract_session(&gm, memory, 1, &ctx, &Twice("not json"))
            .await
            .unwrap();
        assert_eq!(report.llm_calls, 2);
        assert!(report.is_total_failure());
        assert!(
            report
                .errors
                .iter()
                .any(|err| err.contains("parse failure")),
            "{:?}",
            report.errors
        );
        assert!(crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap()
            .is_empty());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn v1_entities_refuse_before_the_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::create_dir_all(memory.join("conversations")).unwrap();
        std::fs::write(
            memory.join("conversations/conversation-001.md"),
            "---\nlog: 1\ndate: \"2026-01-02T00:00:00Z\"\nsession_id: \"s\"\n---\n\n\
             ### User\n\nShip the shelf.\n",
        )
        .unwrap();
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        gm.db()
            .query(
                "CREATE entity SET name = 'pine', entity_type = 'project', abstract = 'host', overview = ''",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        let ctx = IngestContext::new("s", Some(1));
        let err = extract_session(&gm, memory, 1, &ctx, &PanicProvider)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(write::V2_REFUSAL), "{err}");
    }

    struct Twice(&'static str);

    #[async_trait::async_trait]
    impl LlmProvider for Twice {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Ok(self.0.to_string())
        }
    }

    struct PanicProvider;

    #[async_trait::async_trait]
    impl LlmProvider for PanicProvider {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            panic!("provider called");
        }
    }
}
