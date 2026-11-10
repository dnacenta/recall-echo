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
use super::super::llm::{LlmProvider, Message, TokenUsage};
use super::super::llm_json::{self, JsonFailure};
use super::super::types::{IngestionReport, MatchReport, StageTiming};
use super::super::GraphMemory;
use super::ground::{FieldCredit, SessionGround, ValueShape};
use super::input::{self, ProjectIdentity, SessionInput, Turn, TurnRole};
use super::schema::{
    affects_target, component_of_project, component_project_candidate, component_subject_blocked,
    filter_fact, key_is_empty, map_subject, over_long, project_object, release_name, remap,
    restates_setting, Fact, Kind, Mention, Predicate, Role, DROP_CONFIDENCE, DROP_KEY_BINDING,
    DROP_OVERFLOW, DROP_PARSE, DROP_QUOTE, DROP_REJECTED, DROP_RESTATEMENT, DROP_TOO_LONG,
    DROP_UNRESOLVED, DROP_VALUE_SUPPORT, DROP_VALUE_SUPPORT_PARTIAL, DROP_VERSION, MAX_ITEMS,
};
use super::write;
use crate::graph::Provenance;

/// Pass-1 instructions. The worked example is sent as its own turns.
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

Known facts lists what memory already holds. When a decided fact replaces one of them, copy that known value exactly into `replaces`, and reuse its key for the same setting."#;

const KNOWN_SENTENCES: &str = "\n\nKnown facts lists what memory already holds. When a decided fact replaces one of them, copy that known value exactly into `replaces`, and reuse its key for the same setting.";

const fn bare_end() -> usize {
    let full = PASS1_SYSTEM.as_bytes();
    let suffix = KNOWN_SENTENCES.as_bytes();
    let start = full.len() - suffix.len();
    let mut index = 0;
    while index < suffix.len() {
        assert!(full[start + index] == suffix[index]);
        index += 1;
    }
    start
}

/// Pass-1 instructions with those sentences removed.
pub(crate) const PASS1_SYSTEM_BARE: &str = PASS1_SYSTEM.split_at(bare_end()).0;

/// Worked example, in the same shape as [`user_message`].
pub(crate) const PASS1_EXAMPLE_USER: &str = r#"Project: tidepool
Session date: 2026-01-02
Transcript:
[T0] USER: Let's build tidepool, a Go 1.22 service. Use chi for routing.
[T1] ASSISTANT: Sure. We could also look at gorilla/mux. Default port 3000?
[T2] USER: Port 9000. And never log request bodies.
[T3] ASSISTANT: Found a bug: the cache returned stale entries because the TTL was ignored. Fixed by checking expires_at on read.
[T4] USER: Let's move from Postgres to SQLite, Postgres is overkill here.
[T5] USER: Tag v1.4.0 with the SQLite move and the cache fix. Metrics export is planned for v1.5.

Extract the durable facts, bugs and releases as JSON."#;

/// Compact reply for [`PASS1_EXAMPLE_USER`]. Keys follow the order the schema
/// has once it is serialised for the provider.
pub(crate) const PASS1_EXAMPLE_REPLY: &str = r#"{"bugs":[{"cause":"the TTL was ignored","component":"cache","confidence":0.8,"fix":"check expires_at on read","quote":"the cache returned stale entries because the TTL was ignored","status":"fixed","symptom":"cache returned stale entries","turn":3}],"facts":[{"confidence":0.9,"key":"","object":"Go","object_type":"technology","predicate":"uses","quote":"a Go 1.22 service","replaces":"","role":"language","status":"decided","subject":"tidepool","subject_type":"project","turn":0},{"confidence":0.9,"key":"go_version","object":"1.22","object_type":"literal","predicate":"setting","quote":"a Go 1.22 service","replaces":"","role":"none","status":"decided","subject":"tidepool","subject_type":"project","turn":0},{"confidence":0.9,"key":"","object":"chi","object_type":"technology","predicate":"uses","quote":"Use chi for routing","replaces":"","role":"web_framework","status":"decided","subject":"tidepool","subject_type":"project","turn":0},{"confidence":0.7,"key":"","object":"gorilla/mux","object_type":"technology","predicate":"uses","quote":"We could also look at gorilla/mux","replaces":"","role":"web_framework","status":"considered","subject":"tidepool","subject_type":"project","turn":1},{"confidence":0.9,"key":"port","object":"9000","object_type":"literal","predicate":"setting","quote":"Port 9000.","replaces":"","role":"none","status":"decided","subject":"tidepool","subject_type":"project","turn":2},{"confidence":0.9,"key":"","object":"Never log request bodies.","object_type":"literal","predicate":"rule","quote":"never log request bodies","replaces":"","role":"none","status":"decided","subject":"tidepool","subject_type":"project","turn":2},{"confidence":0.9,"key":"","object":"SQLite","object_type":"technology","predicate":"uses","quote":"move from Postgres to SQLite","replaces":"PostgreSQL","role":"database","status":"decided","subject":"tidepool","subject_type":"project","turn":4},{"confidence":0.9,"key":"","object":"PostgreSQL","object_type":"technology","predicate":"uses","quote":"move from Postgres to SQLite","replaces":"","role":"database","status":"rejected","subject":"tidepool","subject_type":"project","turn":4}],"releases":[{"confidence":0.9,"contents":"SQLite move, cache fix","quote":"Tag v1.4.0 with the SQLite move and the cache fix","status":"released","turn":5,"version":"1.4.0"},{"confidence":0.8,"contents":"metrics export","quote":"Metrics export is planned for v1.5","status":"planned","turn":5,"version":"1.5"}]}"#;

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
///
/// `known` is the rendered known-facts block, or empty when there is nothing
/// to show. An empty block leaves the message byte-for-byte unchanged.
#[must_use]
pub(crate) fn user_message(project: &str, date: &str, window: &str, known: &str) -> String {
    let known_line = if known.is_empty() {
        String::new()
    } else {
        format!("{known}\n")
    };
    format!(
        "Project: {project}\nSession date: {date}\n{known_line}Transcript:\n{window}\n\nExtract the durable facts, bugs and releases as JSON."
    )
}

/// Windows [`extract_session`] would send for this session.
#[must_use]
pub(crate) fn window_count(session: &SessionInput) -> usize {
    input::window(session, WINDOW_CHARS).len()
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
    let total_started = std::time::Instant::now();
    write::ensure_v2_store(gm).await?;
    let session = input::load_session(memory_dir, log)?;
    let windows = input::window(&session, WINDOW_CHARS);
    let mut report = IngestionReport::default();
    report.windows = u32::try_from(windows.len()).unwrap_or(u32::MAX);
    report.chunks_total = report.windows;

    let pass1_started = std::time::Instant::now();
    // The known block is opt-in model input, not a turn. Off does not read
    // the store, leaves `WriteCtx.known` empty, and omits the block's sentences.
    let ground = SessionGround::prepare(&session.turns);
    let known_on = known_facts_context_enabled(memory_dir);
    let ground_edges = ground_component_edges_enabled(memory_dir);
    let match_on = known_match_enabled(memory_dir);
    let system = if known_on {
        PASS1_SYSTEM
    } else {
        PASS1_SYSTEM_BARE
    };
    // The block is the only pre-pass lookup. Matching, when the block is off,
    // waits until the kept facts are known so a session with nothing to match
    // does not read the store.
    let session_text = super::known::turn_text(&session.turns);
    // One load when either switch is on. The block ranks that load. Matching
    // reuses the unranked rows, so a second read is not needed.
    let (stored, known_block) = if known_on && match_on {
        let rows = super::known::read_live(gm, &session.project, &session_text).await;
        let block =
            super::known::render_known(&super::known::rank_known(rows.clone(), &session_text));
        (Some(rows), block)
    } else if known_on {
        let rows = super::known::known_rows(gm, &session.project, &session_text).await?;
        (None, super::known::render_known(&rows))
    } else {
        (None, String::new())
    };
    let mut facts = Vec::new();
    for window in &windows {
        match call_window(llm, &session, window, &known_block, system, &mut report).await {
            Ok(reply) => facts.extend(collect_facts(
                &reply,
                &session,
                &ground,
                &mut report,
                ground_edges,
            )),
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
    // Decide against this list, then drop. Reconcile below indexes targets by
    // the facts it receives, so the list must not shrink after that.
    let facts = if drop_restatements_enabled(memory_dir) {
        drop_restating_rules(facts, &mut report)
    } else {
        facts
    };
    let pass1_ms = elapsed_ms(pass1_started);

    let match_started = std::time::Instant::now();
    let (facts, targets, mut match_report, match_ms) = if match_on {
        let rows = if let Some(rows) = stored {
            rows
        } else if facts.iter().any(wants_match) {
            super::known::read_live(gm, &session.project, &session_text).await
        } else {
            Vec::new()
        };
        let reconciled = super::known_match::reconcile(&facts, &rows, &ground);
        (
            reconciled.facts,
            reconciled.targets,
            reconciled.report,
            Some(elapsed_ms(match_started)),
        )
    } else {
        let _ = stored;
        (facts, Vec::new(), MatchReport::default(), None)
    };

    let mut resolve_ms = 0;
    let mut write_ms = 0;
    let consolidate_on = consolidate_enabled(memory_dir);
    let mut consolidate_ms = None;
    if !facts.is_empty() {
        let mut write_ctx = write::WriteCtx::new(
            llm,
            &session.project,
            ctx.session_id(),
            &session.date,
            &session.turns,
        );
        write_ctx.match_on = match_on;
        write_ctx.hold_on = hold_enabled(memory_dir);
        write_ctx.links_on = links_enabled(memory_dir);
        write_ctx.ground_on = ground_edges;
        write_ctx.targets = targets;
        let stats = write::write_facts(gm, &write_ctx, &facts).await?;
        if match_on {
            for (reason, count) in &stats.match_skips {
                *match_report.skipped.entry(reason.clone()).or_insert(0) += *count;
            }
        }
        resolve_ms = stats.resolve_ms;
        write_ms = stats.write_ms;
        report.entities_created = stats.entities_created;
        report.relationships_created = stats.edges;
        report.slots_written = stats.slots;
        report.llm_calls += stats.resolver_calls;
        report.resolve = stats.resolve.clone();
        report.retirements = stats.retirements.clone();
        report.retirements_skipped = stats.retirements_skipped;
        report.retirement_skips = stats.retirement_skips.clone();
        if stats.drops_enumeration > 0 {
            *report.drops.entry("enumeration".to_string()).or_insert(0) += stats.drops_enumeration;
        }
        if let Some(usage) = stats.usage {
            record_sides(&mut report, usage);
        }
        if stats.unresolved > 0 {
            *report.drops.entry(DROP_UNRESOLVED.to_string()).or_insert(0) += stats.unresolved;
        }
        // Consolidation may add derived-anchor counts on top of the write totals.
        report.links = stats.links;
        report.component_edges.add(stats.component_edges);
        if consolidate_on {
            let started = std::time::Instant::now();
            super::consolidate::run(gm, &write_ctx, &facts, &stats, llm, &mut report).await;
            consolidate_ms = Some(elapsed_ms(started));
        }
        report.hold.holds_opened = stats.holds_opened;
        report.hold.holds_confirmed = report
            .hold
            .holds_confirmed
            .saturating_add(stats.holds_confirmed);
    } else if hold_enabled(memory_dir) {
        // No facts to write. A text-only session can still confirm or judge a hold.
        let mut write_ctx = write::WriteCtx::new(
            llm,
            &session.project,
            ctx.session_id(),
            &session.date,
            &session.turns,
        );
        write_ctx.hold_on = true;
        write_ctx.ground_on = ground_edges;
        let started = std::time::Instant::now();
        let stats = write::WriteStats::default();
        super::consolidate::run(gm, &write_ctx, &facts, &stats, llm, &mut report).await;
        consolidate_ms = Some(elapsed_ms(started));
    } else if consolidate_on {
        consolidate_ms = Some(0);
    }
    report.known_match = match_report;
    report.timing = Some(StageTiming {
        pass1_ms,
        resolve_ms,
        write_ms,
        total_ms: elapsed_ms(total_started),
        match_ms,
        consolidate_ms,
    });
    Ok(report)
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn known_facts_context_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .known_facts_context
}

fn known_match_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .known_match
}

fn consolidate_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .consolidate
}

fn hold_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .hold_applies()
}

fn links_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .links_apply()
}

fn drop_restatements_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .drop_restatements
}

fn ground_component_edges_enabled(memory_dir: &Path) -> bool {
    crate::config::load_from_dir(memory_dir)
        .graph
        .unwrap_or_default()
        .ground_component_edges
}

/// Drop a decided rule only when it and one setting are a component by themselves.
///
/// Matches are edges in a bipartite graph. A larger component is ambiguous, so
/// every fact in it stays. `kept` is left as the candidate count.
fn drop_restating_rules(facts: Vec<Fact>, report: &mut IngestionReport) -> Vec<Fact> {
    let drop = isolated_restatement_rules(&facts);
    let mut kept = Vec::with_capacity(facts.len());
    for (index, fact) in facts.into_iter().enumerate() {
        if drop[index] {
            count_drop(report, DROP_RESTATEMENT);
            continue;
        }
        kept.push(fact);
    }
    kept
}

fn isolated_restatement_rules(facts: &[Fact]) -> Vec<bool> {
    let mut parent: Vec<usize> = (0..facts.len()).collect();
    link_restatement_edges(facts, &mut parent);
    let (rules, settings) = component_counts(facts, &parent);
    facts
        .iter()
        .enumerate()
        .map(|(index, fact)| {
            let root = find(&parent, index);
            eligible_rule(fact) && rules[root] == 1 && settings[root] == 1
        })
        .collect()
}

fn link_restatement_edges(facts: &[Fact], parent: &mut [usize]) {
    for (rule_index, rule) in facts.iter().enumerate() {
        if eligible_rule(rule) {
            link_rule(facts, parent, rule_index, rule);
        }
    }
}

fn link_rule(facts: &[Fact], parent: &mut [usize], rule_index: usize, rule: &Fact) {
    for (setting_index, setting) in facts.iter().enumerate() {
        if restatement_edge(setting, rule) {
            unite(parent, rule_index, setting_index);
        }
    }
}

fn restatement_edge(setting: &Fact, rule: &Fact) -> bool {
    restatement_anchor(setting, rule)
        && restates_setting(&rule.object_name, &setting.key, &setting.object_name)
}

fn component_counts(facts: &[Fact], parent: &[usize]) -> (Vec<u32>, Vec<u32>) {
    let mut rules = vec![0u32; facts.len()];
    let mut settings = vec![0u32; facts.len()];
    for (index, fact) in facts.iter().enumerate() {
        let root = find(parent, index);
        if eligible_rule(fact) {
            rules[root] = rules[root].saturating_add(1);
        } else if anchor_setting(fact) {
            settings[root] = settings[root].saturating_add(1);
        }
    }
    (rules, settings)
}

fn eligible_rule(fact: &Fact) -> bool {
    fact.predicate == Predicate::Rule && fact.status == "decided" && fact.replaces.trim().is_empty()
}

fn anchor_setting(fact: &Fact) -> bool {
    fact.predicate == Predicate::Setting
        && fact.status == "decided"
        && !key_is_empty(&fact.key)
        && !fact.object_name.trim().is_empty()
}

fn restatement_anchor(setting: &Fact, rule: &Fact) -> bool {
    anchor_setting(setting)
        && setting.turn == rule.turn
        && setting.subject_kind == rule.subject_kind
        && setting.subject_name == rule.subject_name
}

fn find(parent: &[usize], mut index: usize) -> usize {
    while parent[index] != index {
        index = parent[index];
    }
    index
}

fn unite(parent: &mut [usize], left: usize, right: usize) {
    let left = find(parent, left);
    let right = find(parent, right);
    if left != right {
        parent[right] = left;
    }
}

/// A decided setting, or a decided `uses` whose role can retire a stored edge.
fn wants_match(fact: &Fact) -> bool {
    if fact.status != "decided" {
        return false;
    }
    match fact.predicate {
        Predicate::Setting => true,
        Predicate::Uses => fact
            .role
            .is_some_and(|role| !matches!(role, Role::Other | Role::None | Role::Library)),
        _ => false,
    }
}

async fn call_window(
    llm: &dyn LlmProvider,
    session: &SessionInput,
    window: &str,
    known: &str,
    system: &str,
    report: &mut IngestionReport,
) -> Result<Map<String, Value>, String> {
    let message = user_message(&session.project.name, &session.date, window, known);
    let messages = [
        Message::user(PASS1_EXAMPLE_USER),
        Message::assistant(PASS1_EXAMPLE_REPLY),
        Message::user(message.clone()),
    ];
    let mut last = String::new();
    for _ in 0..2 {
        report.llm_calls += 1;
        let completion = match llm
            .complete_structured_chat(system, &messages, pass1_schema(), MAX_TOKENS)
            .await
        {
            Ok(completion) => completion,
            Err(err) => return Err(format!("parse failure: {err}")),
        };
        bill(report, completion.usage, system, &message, &completion.text);
        match parsed_root(&completion.text) {
            Ok(object) => return Ok(object),
            Err(reason) => last = reason,
        }
    }
    Err(last)
}

fn bill(
    report: &mut IngestionReport,
    usage: Option<TokenUsage>,
    system: &str,
    message: &str,
    reply: &str,
) {
    match usage {
        Some(usage) => {
            report.measured_tokens += usage.total();
            record_sides(report, usage);
        }
        None => {
            let chars = system.len()
                + PASS1_EXAMPLE_USER.len()
                + PASS1_EXAMPLE_REPLY.len()
                + message.len()
                + reply.len();
            report.estimated_tokens += u64::try_from(chars / 4).unwrap_or(0);
        }
    }
}

/// Add one reported call. An unreported side arrives as zero, and that zero is summed.
fn record_sides(report: &mut IngestionReport, usage: TokenUsage) {
    report.input_tokens = Some(
        report
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.input_tokens),
    );
    report.output_tokens = Some(
        report
            .output_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens),
    );
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
    facts_with_ground(reply, session, report, false)
}

#[cfg(test)]
fn facts_with_ground(
    reply: &Map<String, Value>,
    session: &SessionInput,
    report: &mut IngestionReport,
    ground_on: bool,
) -> Vec<Fact> {
    let ground = SessionGround::prepare(&session.turns);
    collect_facts(reply, session, &ground, report, ground_on)
}

fn collect_facts(
    reply: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
    report: &mut IngestionReport,
    ground_on: bool,
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
            match accept_item(section, item, session, ground, report, ground_on) {
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
    ground_on: bool,
) -> Result<Vec<Fact>, &'static str> {
    if !element_matches(section, item) {
        return Err(DROP_PARSE);
    }
    let object = item.as_object().ok_or(DROP_PARSE)?;
    match section {
        "facts" => accept_fact(object, session, ground, ground_on, report).map(|fact| vec![fact]),
        "bugs" => expand_bug(object, session, ground, report),
        "releases" => expand_release(object, session, ground, report),
        _ => Err(DROP_PARSE),
    }
}

struct ComponentPrior<'a> {
    name: &'a str,
    kind: Kind,
    project: &'a ProjectIdentity,
    ground_on: bool,
    swapped: bool,
}

impl ComponentPrior<'_> {
    fn applies(&self) -> bool {
        self.ground_on && !self.swapped
    }
}

fn accept_fact(
    object: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
    ground_on: bool,
    report: &mut IngestionReport,
) -> Result<Fact, &'static str> {
    let (mut fact, swapped) = open_fact(object, session, ground)?;
    let prior_name = fact.subject_name.clone();
    let prior = ComponentPrior {
        name: &prior_name,
        kind: fact.subject_kind,
        project: &session.project,
        ground_on,
        swapped,
    };
    bind_subject(&mut fact, swapped, &session.project)?;
    let value_ok = object_supported(swapped, &fact, ground);
    let grounded = recover_unsupported(&mut fact, &prior, value_ok, ground)?;
    let kept = kept_component_object(&fact, &prior, value_ok);
    finish_fact(fact, ground, grounded, kept, report)
}

fn open_fact(
    object: &Map<String, Value>,
    session: &SessionInput,
    ground: &SessionGround,
) -> Result<(Fact, bool), &'static str> {
    let cited = cite_object(object, ground)?;
    let mut mention = mention_from(object, &session.turns)?;
    mention.turn = cited.turn;
    mention.provenance = cited.role;
    mention.quote = cited.quote;
    let swapped = component_of_project(&mention);
    let mut fact = remap(mention)?;
    fact.replaces = replaces_text(object);
    Ok((fact, swapped))
}

fn replaces_text(object: &Map<String, Value>) -> String {
    object
        .get("replaces")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn bind_subject(
    fact: &mut Fact,
    swapped: bool,
    project: &ProjectIdentity,
) -> Result<(), &'static str> {
    if swapped {
        project_object(fact, project)?;
        return Ok(());
    }
    map_subject(fact, project);
    Ok(())
}

fn recover_unsupported(
    fact: &mut Fact,
    prior: &ComponentPrior<'_>,
    value_ok: bool,
    ground: &SessionGround,
) -> Result<bool, &'static str> {
    if value_ok {
        return Ok(false);
    }
    if !prior.applies() {
        return Err(DROP_VALUE_SUPPORT);
    }
    recover_component_edge(fact, prior, ground)?;
    Ok(true)
}

fn object_supported(swapped: bool, fact: &Fact, ground: &SessionGround) -> bool {
    if swapped {
        return ground.supports_value(
            &fact.subject_name,
            fact.turn,
            &fact.quote,
            ValueShape::Entity,
        );
    }
    let shape = if fact.object_kind.is_some() {
        ValueShape::Entity
    } else {
        ValueShape::Literal
    };
    // A punctuation literal has no content token. Only a keyed setting whose
    // quote wraps it can pass gate 2 that way. Every other predicate, including
    // a swapped component edge, stays on the ordinary check.
    let punctuation_setting = fact.predicate == Predicate::Setting
        && ground.punctuation_setting_supported(
            &fact.key,
            &fact.object_name,
            fact.turn,
            &fact.quote,
        );
    punctuation_setting || ground.supports_value(&fact.object_name, fact.turn, &fact.quote, shape)
}

fn recover_component_edge(
    fact: &mut Fact,
    prior: &ComponentPrior<'_>,
    ground: &SessionGround,
) -> Result<(), &'static str> {
    if !component_project_candidate(prior.kind, fact, prior.project) {
        return Err(DROP_VALUE_SUPPORT);
    }
    if component_subject_blocked(prior.name, &fact.object_name, prior.project) {
        return Err(DROP_REJECTED);
    }
    if !ground.supports_value(prior.name, fact.turn, &fact.quote, ValueShape::Entity) {
        return Err(DROP_VALUE_SUPPORT);
    }
    fact.subject_name = prior.name.to_string();
    fact.subject_kind = Kind::Component;
    fact.object_name = prior.project.name.clone();
    fact.object_kind = Some(Kind::Project);
    fact.replaces.clear();
    fact.provenance = Provenance::SelfGenerated;
    Ok(())
}

fn kept_component_object(fact: &Fact, prior: &ComponentPrior<'_>, value_ok: bool) -> bool {
    prior.applies()
        && value_ok
        && fact.subject_kind == Kind::Component
        && component_project_candidate(prior.kind, fact, prior.project)
}

fn finish_fact(
    mut fact: Fact,
    ground: &SessionGround,
    grounded: bool,
    kept_object: bool,
    report: &mut IngestionReport,
) -> Result<Fact, &'static str> {
    if fact.predicate == Predicate::Setting
        && !ground.key_bound(&fact.key, &fact.object_name, fact.turn, &fact.quote)
    {
        return Err(DROP_KEY_BINDING);
    }
    if !ground.replaces_grounded(&fact.replaces, &fact.status, fact.turn) {
        fact.replaces.clear();
    }
    filter_fact(&fact)?;
    note_component_edge(grounded, kept_object, report);
    Ok(fact)
}

fn note_component_edge(grounded: bool, kept_object: bool, report: &mut IngestionReport) {
    if grounded {
        report.component_edges.component_edges_grounded += 1;
    } else if kept_object {
        report.component_edges.component_edges_kept_object += 1;
    }
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
        quote: text(object, "quote")?.trim().to_string(),
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
    // A merged quote drops the cause and the fix with it. A verbatim field of
    // this bug can stand in, and only after the quote has already missed.
    let cited = match cite_object(object, ground) {
        Err(DROP_QUOTE) => {
            let symptom = text(object, "symptom")?.trim();
            let cause = text(object, "cause")?.trim();
            let fix = text(object, "fix")?.trim();
            ground
                .cite_field(turn_of(object)?, &[symptom, cause, fix])
                .ok_or(DROP_QUOTE)?
        }
        other => other?,
    };
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
    let mut carrier = carrier(confidence, &cited);
    // The cited turn is checked first. A later turn may use that turn and
    // every earlier turn, so a fix turn can restate a symptom said earlier.
    // A turn after the citation is not support. Nowhere in that text drops
    // the whole bug. The quote was already required to be in the cited turn.
    let symptom_credit = ground.credit_symptom(symptom, carrier.turn, &carrier.quote);
    if symptom_credit == FieldCredit::Drop {
        return Err(DROP_VALUE_SUPPORT);
    }
    // Cause and fix are slots of the symptom, the same subject as the symptom slot.
    carrier.subject_name = symptom.to_string();
    carrier.subject_kind = Kind::Issue;
    let component = text(object, "component")?.trim();
    let (target_name, target_kind) = affects_target(component, &session.project);
    let mut symptom_fact = slot_fact(
        symptom,
        Kind::Issue,
        Predicate::IssueSymptom,
        symptom,
        &carrier,
    );
    // Not supported by the cited turn: the slot is inferred. The quote stays.
    if symptom_credit == FieldCredit::Session {
        symptom_fact.provenance = Provenance::SelfGenerated;
    }
    let mut facts = vec![symptom_fact];
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
        // A named component is an entity of the cited turn. An empty, generic
        // or project-name component is the project fallback: that edge is
        // synthetic, so it is inferred and it is not a drop.
        if target_kind == Kind::Component {
            if ground.supports_value(component, carrier.turn, &carrier.quote, ValueShape::Entity) {
                facts.push(edge_fact(
                    symptom,
                    Kind::Issue,
                    Predicate::Affects,
                    &target_name,
                    Some(target_kind),
                    &carrier,
                ));
            } else {
                count_drop(report, DROP_VALUE_SUPPORT_PARTIAL);
            }
        } else {
            facts.push(inferred(edge_fact(
                symptom,
                Kind::Issue,
                Predicate::Affects,
                &target_name,
                Some(target_kind),
                &carrier,
            )));
        }
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
    let mut carrier = carrier(confidence, &cited);
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
        FieldCredit::Drop => count_drop(report, DROP_VALUE_SUPPORT_PARTIAL),
    }
}

fn inferred(mut fact: Fact) -> Fact {
    fact.provenance = Provenance::SelfGenerated;
    fact
}

fn carrier(confidence: f64, cited: &super::ground::Cited) -> Fact {
    Fact {
        subject_name: String::new(),
        subject_kind: Kind::Issue,
        predicate: Predicate::IssueSymptom,
        role: None,
        key: String::new(),
        object_name: String::new(),
        object_kind: None,
        status: "decided".to_string(),
        turn: cited.turn,
        quote: cited.quote.clone(),
        confidence,
        provenance: from_role(cited.role),
        replaces: String::new(),
    }
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
    use crate::graph::types::IngestionReport;
    use crate::graph::v2::input::{ProjectIdentity, Turn, TurnRole};
    use crate::graph::v2::schema::{
        Fact, Kind, Predicate, DROP_CONFIDENCE, DROP_KEY_BINDING, DROP_OVERFLOW, DROP_PARSE,
        DROP_QUOTE, DROP_REJECTED, DROP_TOO_LONG, DROP_VALUE_SUPPORT, DROP_VALUE_SUPPORT_PARTIAL,
        DROP_VERSION, MAX_ITEMS,
    };
    use crate::graph::Provenance;
    use serde_json::Value;

    /// The pass-1 system prompt from before the known-facts block (631c5dc).
    const SYSTEM_BEFORE_KNOWN_BLOCK: &str =
        include_str!("../../../tests/fixtures/pass1/system-pre-known-block.txt");

    #[test]
    fn off_system_prompt_matches_the_golden_pre_block_prompt() {
        assert_eq!(PASS1_SYSTEM_BARE, SYSTEM_BEFORE_KNOWN_BLOCK);
        assert_eq!(
            PASS1_SYSTEM,
            format!("{SYSTEM_BEFORE_KNOWN_BLOCK}{KNOWN_SENTENCES}")
        );
    }

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

            async fn complete_structured_chat(
                &self,
                system: &str,
                messages: &[Message],
                schema: &Value,
                max_tokens: u32,
            ) -> Result<crate::graph::llm::Completion, GraphError> {
                assert_eq!(system, PASS1_SYSTEM);
                assert_eq!(max_tokens, MAX_TOKENS);
                assert_eq!(messages.len(), 3);
                assert_eq!(messages[0], Message::user(PASS1_EXAMPLE_USER));
                assert_eq!(messages[1], Message::assistant(PASS1_EXAMPLE_REPLY));
                assert_eq!(
                    messages[2],
                    Message::user(user_message("tidepool", "2026-01-02", "[T0] USER: hi", ""))
                );
                *self.0.lock().expect("lock") = Some(schema.clone());
                Ok(crate::graph::llm::Completion::unmeasured(
                    r#"{"facts":[],"bugs":[],"releases":[]}"#,
                ))
            }
        }

        let seen = Seen(std::sync::Mutex::new(None));
        let mut report = IngestionReport::default();
        let object = call_window(
            &seen,
            &session(turns()),
            "[T0] USER: hi",
            "",
            PASS1_SYSTEM,
            &mut report,
        )
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
        let example = parsed_root(PASS1_EXAMPLE_REPLY).expect("prompt example matches the schema");
        assert_eq!(
            PASS1_EXAMPLE_REPLY,
            serde_json::to_string(&example).expect("canonical"),
            "example reply is compact JSON in serialised key order"
        );
        for section in ["facts", "bugs", "releases"] {
            let items = example[section].as_array().expect(section);
            assert!(!items.is_empty(), "{section}");
            for item in items {
                assert!(element_matches(section, item), "{section}: {item}");
            }
        }
        let transcript = PASS1_EXAMPLE_USER
            .split_once("Transcript:\n")
            .expect("transcript")
            .1
            .strip_suffix("\n\nExtract the durable facts, bugs and releases as JSON.")
            .expect("closing line");
        assert_eq!(
            PASS1_EXAMPLE_USER,
            user_message("tidepool", "2026-01-02", transcript, "")
        );
        assert!(!PASS1_SYSTEM.contains("EXAMPLE"));
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
    fn project_uses_component_is_part_of_the_session() {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "Add an export command with CSV output.".into(),
        }];
        let cases = [
            (
                "tidepool",
                "project",
                "uses",
                "export command",
                "component",
                "decided",
                true,
            ),
            (
                "the project",
                "project",
                "uses",
                "export command",
                "component",
                "decided",
                true,
            ),
            (
                "tidepool",
                "project",
                "uses",
                "export command",
                "component",
                "considered",
                true,
            ),
            (
                "tidepool",
                "project",
                "uses",
                "export command",
                "component",
                "rejected",
                true,
            ),
            (
                "tidepool",
                "project",
                "uses",
                "import wizard",
                "component",
                "decided",
                false,
            ),
            (
                "export command",
                "component",
                "part_of",
                "tidepool",
                "project",
                "decided",
                false,
            ),
        ];
        for (subject, subject_type, predicate, object, object_type, status, kept) in cases {
            let mut item = fact_json(0, "Add an export command");
            let fields = item.as_object_mut().expect("fact");
            for (key, value) in [
                ("subject", subject),
                ("subject_type", subject_type),
                ("predicate", predicate),
                ("object", object),
                ("object_type", object_type),
                ("status", status),
                ("key", ""),
            ] {
                fields.insert(key.into(), Value::String(value.into()));
            }
            let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
            let mut report = IngestionReport::default();
            let facts = facts_from_reply(
                reply.as_object().expect("reply"),
                &session(turns.clone()),
                &mut report,
            );
            assert_candidate_balance(&report);
            if kept {
                assert_eq!(facts.len(), 1, "{subject} {status}");
                assert!(report.drops.is_empty(), "{:?}", report.drops);
                let fact = &facts[0];
                assert_eq!(fact.subject_name, "export command");
                assert_eq!(fact.subject_kind, Kind::Component);
                assert_eq!(fact.predicate, Predicate::PartOf);
                assert_eq!(fact.object_name, "tidepool");
                assert_eq!(fact.object_kind, Some(Kind::Project));
                assert_eq!(fact.status, status);
                assert_eq!(fact.provenance, Provenance::User);
                assert_eq!(fact.role, None);
                assert_eq!(fact.key, "");
                assert!(fact.replaces.is_empty());
                assert_eq!(fact.quote, "Add an export command");
            } else {
                assert!(facts.is_empty(), "{predicate} {object}");
                assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
            }
        }
    }

    struct ComponentCase<'a> {
        turn: &'a str,
        subject: &'a str,
        object: &'a str,
        status: &'a str,
        quote: &'a str,
        replaces: &'a str,
        aliases: &'a [&'a str],
        ground_on: bool,
    }

    fn part_of<'a>(
        subject: &'a str,
        object: &'a str,
        status: &'a str,
        quote: &'a str,
        ground_on: bool,
    ) -> ComponentCase<'a> {
        ComponentCase {
            turn: "Add an export command with CSV output.",
            subject,
            object,
            status,
            quote,
            replaces: "",
            aliases: &[],
            ground_on,
        }
    }

    fn component_edge_case(case: ComponentCase<'_>) -> (Vec<Fact>, IngestionReport) {
        let mut item = fact_json(0, case.quote);
        let fields = item.as_object_mut().expect("fact");
        for (key, value) in [
            ("subject", case.subject),
            ("subject_type", "component"),
            ("predicate", "part_of"),
            ("object", case.object),
            ("object_type", "project"),
            ("status", case.status),
            ("key", ""),
            ("replaces", case.replaces),
        ] {
            fields.insert(key.into(), Value::String(value.into()));
        }
        let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
        let session = SessionInput {
            session_id: "s".into(),
            date: "2026-01-02".into(),
            project: ProjectIdentity {
                name: "tidepool".into(),
                aliases: case
                    .aliases
                    .iter()
                    .map(|alias| (*alias).to_string())
                    .collect(),
            },
            turns: vec![Turn {
                index: 0,
                role: TurnRole::User,
                text: case.turn.into(),
            }],
            tool_lines: Vec::new(),
        };
        let mut report = IngestionReport::default();
        let facts = facts_with_ground(
            reply.as_object().expect("reply"),
            &session,
            &mut report,
            case.ground_on,
        );
        assert_candidate_balance(&report);
        (facts, report)
    }

    #[test]
    fn decided_component_part_of_grounds_on_the_subject() {
        let (facts, report) = component_edge_case(ComponentCase {
            replaces: "csv",
            ..part_of(
                "export command",
                "tidepool",
                "decided",
                "Add an export command",
                true,
            )
        });
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        let fact = &facts[0];
        assert_eq!(fact.subject_name, "export command");
        assert_eq!(fact.subject_kind, Kind::Component);
        assert_eq!(fact.predicate, Predicate::PartOf);
        assert_eq!(fact.object_name, "tidepool");
        assert_eq!(fact.object_kind, Some(Kind::Project));
        assert_eq!(fact.status, "decided");
        assert_eq!(fact.provenance, Provenance::SelfGenerated);
        assert!(fact.replaces.is_empty());
        assert_eq!(fact.quote, "Add an export command");
        assert_eq!(report.component_edges.component_edges_grounded, 1);
        assert_eq!(report.component_edges.component_edges_kept_object, 0);
        assert!(report.drops.is_empty());

        let (off, off_report) = component_edge_case(ComponentCase {
            replaces: "csv",
            ..part_of(
                "export command",
                "tidepool",
                "decided",
                "Add an export command",
                false,
            )
        });
        assert!(off.is_empty());
        assert_eq!(off_report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert!(off_report.component_edges.is_blank());
    }

    #[test]
    fn component_part_of_keeps_cited_provenance_when_the_project_is_named() {
        let (facts, report) = component_edge_case(ComponentCase {
            turn: "Add an export command to tidepool.",
            ..part_of(
                "export command",
                "tidepool",
                "decided",
                "export command to tidepool",
                true,
            )
        });
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        let fact = &facts[0];
        assert_eq!(fact.subject_name, "export command");
        assert_eq!(fact.object_name, "tidepool");
        assert_eq!(fact.provenance, Provenance::User);
        assert_eq!(fact.quote, "export command to tidepool");
        assert_eq!(report.component_edges.component_edges_kept_object, 1);
        assert_eq!(report.component_edges.component_edges_grounded, 0);
    }

    #[test]
    fn component_part_of_an_alias_is_canonicalised() {
        let (facts, report) = component_edge_case(ComponentCase {
            replaces: "old",
            aliases: &["tp"],
            ..part_of(
                "export command",
                "tp",
                "decided",
                "Add an export command",
                true,
            )
        });
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        assert_eq!(facts[0].object_name, "tidepool");
        assert_eq!(facts[0].provenance, Provenance::SelfGenerated);
        assert!(facts[0].replaces.is_empty());
        assert_eq!(report.component_edges.component_edges_grounded, 1);
    }

    #[test]
    fn undecided_component_part_of_stays_dropped_when_the_flag_is_on() {
        for status in ["considered", "rejected"] {
            let (facts, report) = component_edge_case(part_of(
                "export command",
                "tidepool",
                status,
                "Add an export command",
                true,
            ));
            assert!(facts.is_empty(), "{status}");
            assert_eq!(
                report.drops.get(DROP_VALUE_SUPPORT).copied(),
                Some(1),
                "{status}"
            );
            assert!(report.component_edges.is_blank(), "{status}");
        }
    }

    #[test]
    fn self_edge_and_project_subject_are_not_repointed() {
        let (self_edge, report) = component_edge_case(part_of(
            "tidepool",
            "tidepool",
            "decided",
            "Add an export command",
            true,
        ));
        assert!(self_edge.is_empty());
        assert_eq!(report.drops.get(DROP_REJECTED).copied(), Some(1));
        assert!(report.component_edges.is_blank());

        let (alias, alias_report) = component_edge_case(ComponentCase {
            aliases: &["tp"],
            ..part_of("tp", "tidepool", "decided", "Add an export command", true)
        });
        assert!(alias.is_empty());
        assert_eq!(alias_report.drops.get(DROP_REJECTED).copied(), Some(1));

        let (store, store_report) = component_edge_case(part_of(
            "store",
            "store",
            "decided",
            "Add an export command",
            true,
        ));
        assert!(store.is_empty());
        assert_eq!(store_report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert!(store_report.component_edges.is_blank());
    }

    #[test]
    fn a_different_project_stays_on_the_object_check() {
        let (dropped, report) = component_edge_case(part_of(
            "export command",
            "harbor",
            "decided",
            "Add an export command",
            true,
        ));
        assert!(dropped.is_empty());
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert!(report.component_edges.is_blank());

        let (kept, kept_report) = component_edge_case(ComponentCase {
            turn: "Add an export command for harbor.",
            ..part_of(
                "export command",
                "harbor",
                "decided",
                "export command for harbor",
                true,
            )
        });
        assert_eq!(kept.len(), 1, "{:?}", kept_report.drops);
        assert_eq!(kept[0].object_name, "harbor");
        assert_eq!(kept[0].provenance, Provenance::User);
        assert!(kept_report.component_edges.is_blank());
    }

    #[test]
    fn generic_component_subject_is_rejected_on_the_new_path() {
        for subject in ["it", "code", "app"] {
            let (facts, report) = component_edge_case(part_of(
                subject,
                "tidepool",
                "decided",
                "Add an export command",
                true,
            ));
            assert!(facts.is_empty(), "{subject}");
            assert_eq!(
                report.drops.get(DROP_REJECTED).copied(),
                Some(1),
                "{subject} {:?}",
                report.drops
            );
            assert!(report.component_edges.is_blank(), "{subject}");
        }
    }

    #[test]
    fn unsupported_component_name_stays_dropped() {
        let (facts, report) = component_edge_case(part_of(
            "export command",
            "tidepool",
            "decided",
            "CSV output",
            true,
        ));
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert!(report.component_edges.is_blank());
    }

    #[test]
    fn empty_and_over_long_component_subjects_are_rejected() {
        let (empty, empty_report) = component_edge_case(ComponentCase {
            subject: "",
            ..part_of(
                "export command",
                "tidepool",
                "decided",
                "Add an export command",
                true,
            )
        });
        assert!(empty.is_empty());
        assert_eq!(empty_report.drops.get(DROP_REJECTED).copied(), Some(1));
        assert!(empty_report.component_edges.is_blank());

        let long = "x".repeat(301);
        let (over, over_report) = component_edge_case(ComponentCase {
            subject: &long,
            ..part_of(
                "export command",
                "tidepool",
                "decided",
                "Add an export command",
                true,
            )
        });
        assert!(over.is_empty());
        assert_eq!(over_report.drops.get(DROP_TOO_LONG).copied(), Some(1));
        assert!(over_report.component_edges.is_blank());
    }

    #[test]
    fn inverted_project_part_of_component_is_part_of_the_session() {
        let turn = "Add a cache layer module to widget inside the platform suite \
            beside the queue broker using the buffer pool instead of the buffer";
        let session = SessionInput {
            session_id: "s".into(),
            date: "2026-01-02".into(),
            project: ProjectIdentity {
                name: "widget".into(),
                aliases: vec!["wdg".into()],
            },
            turns: vec![Turn {
                index: 0,
                role: TurnRole::User,
                text: turn.into(),
            }],
            tool_lines: Vec::new(),
        };
        let accept = |subject: &str,
                      predicate: &str,
                      object: &str,
                      object_type: &str,
                      status: &str,
                      quote: &str,
                      replaces: &str| {
            let mut item = fact_json(0, quote);
            let fields = item.as_object_mut().expect("fact");
            for (key, value) in [
                ("subject", subject),
                ("subject_type", "project"),
                ("predicate", predicate),
                ("object", object),
                ("object_type", object_type),
                ("status", status),
                ("key", ""),
                ("role", "none"),
                ("replaces", replaces),
            ] {
                fields.insert(key.into(), Value::String(value.into()));
            }
            let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
            let mut report = IngestionReport::default();
            let facts = facts_from_reply(reply.as_object().expect("reply"), &session, &mut report);
            assert_candidate_balance(&report);
            (facts, report)
        };

        let (missing, report) = accept(
            "widget",
            "part_of",
            "queue worker",
            "component",
            "decided",
            "widget",
            "",
        );
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));

        let (facts, report) = accept(
            "widget",
            "part_of",
            "cache layer",
            "component",
            "decided",
            "cache layer",
            "",
        );
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        let fact = &facts[0];
        assert_eq!(fact.subject_name, "cache layer");
        assert_eq!(fact.subject_kind, Kind::Component);
        assert_eq!(fact.predicate, Predicate::PartOf);
        assert_eq!(fact.object_name, "widget");
        assert_eq!(fact.object_kind, Some(Kind::Project));
        assert_eq!(fact.status, "decided");
        assert_eq!(fact.provenance, Provenance::User);
        assert_eq!(fact.quote, "cache layer");
        assert_eq!(fact.turn, 0);
        assert_eq!(fact.confidence, 0.9);
        assert!(fact.replaces.is_empty());
        let (uses, uses_report) = accept(
            "widget",
            "uses",
            "cache layer",
            "component",
            "decided",
            "cache layer",
            "",
        );
        assert!(uses_report.drops.is_empty(), "{:?}", uses_report.drops);
        assert_eq!(facts, uses);

        for (subject, object, object_type, status, quote) in [
            (
                "widget",
                "platform suite",
                "project",
                "decided",
                "platform suite",
            ),
            (
                "widget",
                "queue broker",
                "technology",
                "decided",
                "queue broker",
            ),
            ("widget", "buffer pool", "literal", "decided", "buffer pool"),
            (
                "widget",
                "cache layer",
                "component",
                "considered",
                "cache layer",
            ),
            (
                "widget",
                "cache layer",
                "component",
                "rejected",
                "cache layer",
            ),
            (
                "gizmo",
                "cache layer",
                "component",
                "decided",
                "cache layer",
            ),
            ("widget", "wdg", "component", "decided", "widget"),
            ("widget", "widget", "component", "decided", "widget"),
        ] {
            let (facts, report) =
                accept(subject, "part_of", object, object_type, status, quote, "");
            assert!(
                facts.is_empty(),
                "{subject} {object} {object_type} {status}"
            );
            assert_eq!(
                report.drops.get(DROP_REJECTED).copied(),
                Some(1),
                "{subject} {object} {object_type} {status} {:?}",
                report.drops
            );
        }

        for subject in ["wdg", "the project"] {
            let (kept, report) = accept(
                subject,
                "part_of",
                "cache layer",
                "component",
                "decided",
                "cache layer",
                "",
            );
            assert_eq!(kept.len(), 1, "{subject} {:?}", report.drops);
            assert_eq!(
                (kept[0].subject_name.as_str(), kept[0].object_name.as_str()),
                ("cache layer", "widget")
            );
        }

        let (cleared, report) = accept(
            "widget",
            "part_of",
            "cache layer",
            "component",
            "decided",
            "cache layer",
            "buffer",
        );
        assert_eq!(cleared.len(), 1, "{:?}", report.drops);
        assert!(cleared[0].replaces.is_empty(), "{}", cleared[0].replaces);
    }

    #[test]
    fn swapped_component_rejects_generic_empty_and_over_long_names() {
        let long = "x".repeat(301);
        let turn = "Add an export command with CSV output.";
        let quote = "Add an export command";
        let cases = [
            ("", turn, quote),
            ("---", turn, quote),
            ("file", "Keep the file export.", "Keep the file export."),
            (long.as_str(), long.as_str(), long.as_str()),
        ];
        for (object, turn, quote) in cases {
            let turns = vec![Turn {
                index: 0,
                role: TurnRole::User,
                text: turn.into(),
            }];
            let mut item = fact_json(0, quote);
            let fields = item.as_object_mut().expect("fact");
            for (key, value) in [
                ("subject", "tidepool"),
                ("subject_type", "project"),
                ("predicate", "uses"),
                ("object", object),
                ("object_type", "component"),
                ("status", "decided"),
                ("key", ""),
            ] {
                fields.insert(key.into(), Value::String(value.into()));
            }
            let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
            let mut report = IngestionReport::default();
            let facts = facts_from_reply(
                reply.as_object().expect("reply"),
                &session(turns),
                &mut report,
            );
            assert!(facts.is_empty(), "{object}");
            assert_eq!(
                report.drops.get(DROP_REJECTED).copied(),
                Some(1),
                "{object} {:?}",
                report.drops
            );
            assert_candidate_balance(&report);
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn swapped_replaces_is_cleared_and_retires_nothing() {
        let turn = "We use the export command instead of csv";
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: turn.into(),
        }];
        let mut item = fact_json(0, turn);
        let fields = item.as_object_mut().expect("fact");
        for (key, value) in [
            ("subject", "tidepool"),
            ("subject_type", "project"),
            ("predicate", "uses"),
            ("object", "export command"),
            ("object_type", "component"),
            ("status", "decided"),
            ("key", ""),
            ("replaces", "csv"),
        ] {
            fields.insert(key.into(), Value::String(value.into()));
        }
        let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
        let input = session(turns.clone());
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(reply.as_object().expect("reply"), &input, &mut report);
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        let fact = &facts[0];
        assert!(fact.replaces.is_empty(), "{}", fact.replaces);
        assert_eq!(fact.predicate, Predicate::PartOf);
        assert_eq!(fact.subject_name, "export command");
        assert_eq!(fact.subject_kind, Kind::Component);
        assert_eq!(fact.object_name, "tidepool");
        assert_eq!(fact.status, "decided");

        let prior = [
            Fact {
                subject_name: "export command".into(),
                subject_kind: Kind::Component,
                predicate: Predicate::Setting,
                role: None,
                key: "format".into(),
                object_name: "csv".into(),
                object_kind: None,
                status: "decided".into(),
                turn: 0,
                quote: "format is csv".into(),
                confidence: 0.9,
                provenance: Provenance::User,
                replaces: String::new(),
            },
            Fact {
                subject_name: "export command".into(),
                subject_kind: Kind::Component,
                predicate: Predicate::PartOf,
                role: None,
                key: String::new(),
                object_name: "cli".into(),
                object_kind: Some(Kind::Component),
                status: "decided".into(),
                turn: 0,
                quote: "export command is part of cli".into(),
                confidence: 0.9,
                provenance: Provenance::User,
                replaces: String::new(),
            },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        let llm = PanicProvider;
        let ctx = write::WriteCtx::new(&llm, &input.project, "s1", "2026-04-03", &[]);
        write::write_facts(&gm, &ctx, &prior).await.unwrap();
        let ctx = write::WriteCtx::new(&llm, &input.project, "s2", "2026-04-11", &turns);
        let stats = write::write_facts(&gm, &ctx, &facts).await.unwrap();
        assert!(stats.retirements.is_empty(), "{:?}", stats.retirements);

        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let component = entities
            .iter()
            .find(|entity| entity.name == "export command")
            .expect("component");
        let slots = component.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap();
        let slot = slots
            .iter()
            .find(|slot| slot["key"] == "format" && slot["value"] == "csv")
            .expect("format=csv");
        assert!(slot["valid_until"].is_null(), "{slot}");
        let cli = entities
            .iter()
            .find(|entity| entity.name == "cli")
            .expect("cli");
        let edges = crate::graph::crud::list_all_relationships(gm.db())
            .await
            .unwrap();
        let live = edges
            .iter()
            .filter(|edge| {
                edge.rel_type == "part_of"
                    && edge.valid_until.is_none()
                    && edge.from_id == component.id
                    && edge.to_id == cli.id
            })
            .count();
        assert_eq!(live, 1, "{edges:?}");
    }

    #[tokio::test]
    async fn unmeasured_bill_includes_the_example() {
        struct Bare;

        #[async_trait::async_trait]
        impl LlmProvider for Bare {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Ok(r#"{"facts":[],"bugs":[],"releases":[]}"#.to_string())
            }
        }

        let window = "[T0] USER: hi";
        let mut report = IngestionReport::default();
        call_window(
            &Bare,
            &session(turns()),
            window,
            "",
            PASS1_SYSTEM,
            &mut report,
        )
        .await
        .expect("reply");
        let message = user_message("tidepool", "2026-01-02", window, "");
        let reply = r#"{"facts":[],"bugs":[],"releases":[]}"#;
        let chars = PASS1_SYSTEM.len()
            + PASS1_EXAMPLE_USER.len()
            + PASS1_EXAMPLE_REPLY.len()
            + message.len()
            + reply.len();
        assert_eq!(report.measured_tokens, 0);
        assert_eq!(
            report.estimated_tokens,
            u64::try_from(chars / 4).unwrap_or(0)
        );
    }

    #[test]
    fn bill_adds_input_and_output_separately() {
        let mut both = IngestionReport::default();
        bill(
            &mut both,
            TokenUsage::from_counts(Some(3), Some(5)),
            PASS1_SYSTEM,
            "m",
            "r",
        );
        bill(
            &mut both,
            TokenUsage::from_counts(Some(2), Some(1)),
            PASS1_SYSTEM,
            "m",
            "r",
        );
        assert_eq!(
            (
                both.input_tokens,
                both.output_tokens,
                both.measured_tokens,
                both.estimated_tokens
            ),
            (Some(5), Some(6), 11, 0)
        );

        let mut one = IngestionReport::default();
        bill(
            &mut one,
            TokenUsage::from_counts(None, Some(4)),
            PASS1_SYSTEM,
            "m",
            "r",
        );
        assert_eq!(
            (one.input_tokens, one.output_tokens, one.measured_tokens),
            (Some(0), Some(4), 4)
        );

        let mut none = IngestionReport::default();
        bill(&mut none, None, PASS1_SYSTEM, "m", "r");
        assert_eq!(
            (none.input_tokens, none.output_tokens, none.measured_tokens),
            (None, None, 0)
        );
        assert!(none.estimated_tokens > 0);
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
        assert_candidate_balance(&report);
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
    fn release_version_inside_a_longer_number_is_bad_version() {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "Ship 1.0.0 with the exporters.".into(),
        }];
        let reply = serde_json::json!({
            "facts": [],
            "bugs": [],
            "releases": [{
                "version": "1.0",
                "status": "released",
                "contents": "the exporters",
                "turn": 0,
                "quote": "Ship 1.0.0 with the exporters.",
                "confidence": 0.9
            }]
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("object"),
            &session(turns),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.kept, 0);
        assert_eq!(report.candidates, 1);
        assert_eq!(report.drops.get(DROP_VERSION).copied(), Some(1));
        assert_candidate_balance(&report);
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
        assert_eq!(
            report.drops.get(DROP_VALUE_SUPPORT_PARTIAL).copied(),
            Some(1)
        );
        assert!(!report.drops.contains_key(DROP_VALUE_SUPPORT));
        assert_candidate_balance(&report);
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

    #[test]
    fn ungrounded_symptom_number_drops_the_bug() {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "The cache returned stale entries.".into(),
        }];
        let reply = serde_json::json!({
            "facts": [],
            "bugs": [{
                "symptom": "server crashes on port 9090 under load",
                "cause": "",
                "fix": "",
                "status": "open",
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
        assert_eq!(report.kept, 0);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert!(facts.is_empty());
    }

    #[test]
    fn verbatim_symptom_is_kept() {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "The cache returned stale entries.".into(),
        }];
        let reply = serde_json::json!({
            "facts": [],
            "bugs": [{
                "symptom": "stale entries",
                "cause": "",
                "fix": "",
                "status": "open",
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
        assert!(report.drops.is_empty(), "{:?}", report.drops);
        let symptom = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .expect("symptom");
        assert_eq!(symptom.object_name, "stale entries");
        assert_eq!(symptom.provenance, Provenance::User);
        let affects = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::Affects)
            .expect("affects");
        assert_eq!(affects.object_name, "cache");
        assert_eq!(affects.object_kind, Some(Kind::Component));
        assert_eq!(affects.provenance, Provenance::User);
    }

    #[test]
    fn bug_without_a_component_keeps_the_project_edge() {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "The cache returned stale entries.".into(),
        }];
        for component in ["", "project", "tidepool"] {
            let reply = serde_json::json!({
                "facts": [],
                "bugs": [{
                    "symptom": "stale entries",
                    "cause": "",
                    "fix": "",
                    "status": "open",
                    "component": component,
                    "turn": 0,
                    "quote": "The cache returned stale entries.",
                    "confidence": 0.9
                }],
                "releases": []
            });
            let mut report = IngestionReport::default();
            let facts = facts_from_reply(
                reply.as_object().expect("object"),
                &session(turns.clone()),
                &mut report,
            );
            assert_eq!(report.kept, 1, "{component}");
            assert!(report.drops.is_empty(), "{component} {:?}", report.drops);
            let affects = facts
                .iter()
                .find(|fact| fact.predicate == Predicate::Affects)
                .unwrap_or_else(|| panic!("affects for {component}"));
            assert_eq!(affects.object_name, "tidepool");
            assert_eq!(affects.object_kind, Some(Kind::Project));
            assert_eq!(affects.provenance, Provenance::SelfGenerated);
        }
    }

    #[test]
    fn ungrounded_component_drops_only_the_edge() {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "The cache returned stale entries.".into(),
        }];
        let reply = serde_json::json!({
            "facts": [],
            "bugs": [{
                "symptom": "stale entries",
                "cause": "",
                "fix": "",
                "status": "open",
                "component": "payments gateway",
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
        assert_eq!(
            report.drops.get(DROP_VALUE_SUPPORT_PARTIAL).copied(),
            Some(1)
        );
        assert!(!report.drops.contains_key(DROP_VALUE_SUPPORT));
        assert_candidate_balance(&report);
        assert!(facts
            .iter()
            .any(|fact| fact.predicate == Predicate::IssueSymptom));
        assert!(facts
            .iter()
            .all(|fact| fact.predicate != Predicate::Affects));
    }

    fn assert_candidate_balance(report: &IngestionReport) {
        let partial = report
            .drops
            .get(DROP_VALUE_SUPPORT_PARTIAL)
            .copied()
            .unwrap_or(0);
        let listed: u32 = report.drops.values().copied().sum();
        assert_eq!(
            report.kept + listed - partial,
            report.candidates,
            "kept {} drops {:?}",
            report.kept,
            report.drops
        );
    }

    fn bug_reply(
        symptom: &str,
        cause: &str,
        fix: &str,
        component: &str,
        turn: usize,
        quote: &str,
    ) -> Map<String, Value> {
        serde_json::json!({
            "facts": [],
            "bugs": [{
                "symptom": symptom,
                "cause": cause,
                "fix": fix,
                "status": "fixed",
                "component": component,
                "turn": turn,
                "quote": quote,
                "confidence": 0.9
            }],
            "releases": []
        })
        .as_object()
        .expect("object")
        .clone()
    }

    fn reader_turns() -> Vec<Turn> {
        vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "The reader dropped rows near 4242 before the backup.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "Patched the reader and shipped it.".into(),
            },
        ]
    }

    #[test]
    fn later_turn_keeps_a_symptom_said_earlier_in_the_session() {
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply(
                "dropped rows",
                "",
                "dropped rows patched",
                "",
                1,
                "Patched the reader",
            ),
            &session(reader_turns()),
            &mut report,
        );
        assert_eq!(report.kept, 1);
        assert!(report.drops.is_empty(), "{:?}", report.drops);
        assert_candidate_balance(&report);
        let symptom = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .expect("symptom");
        assert_eq!(symptom.object_name, "dropped rows");
        assert_eq!(symptom.provenance, Provenance::SelfGenerated);
        assert_eq!(symptom.turn, 1);
        let fix = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueFix)
            .expect("fix");
        assert_eq!(fix.object_name, "dropped rows patched");
        assert_eq!(fix.provenance, Provenance::SelfGenerated);
        let status = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueStatus)
            .expect("status");
        assert_eq!(status.object_name, "fixed");
    }

    #[test]
    fn later_turn_that_states_the_symptom_keeps_its_role() {
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply("patched the reader", "", "", "", 1, "Patched the reader"),
            &session(reader_turns()),
            &mut report,
        );
        assert!(report.drops.is_empty(), "{:?}", report.drops);
        let symptom = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .expect("symptom");
        assert_eq!(symptom.provenance, Provenance::Assistant);
        assert_eq!(symptom.object_name, "patched the reader");
    }

    #[test]
    fn symptom_support_stops_at_the_cited_turn() {
        let turns = vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "The reader dropped rows.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "Patched the reader.".into(),
            },
            Turn {
                index: 2,
                role: TurnRole::User,
                text: "Retry count is 17.".into(),
            },
        ];
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply("dropped rows 17", "", "", "", 1, "Patched the reader"),
            &session(turns.clone()),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.kept, 0);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);

        let mut earlier = turns;
        earlier[0].text = "The reader dropped rows near 17.".into();
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply("dropped rows 17", "", "", "", 1, "Patched the reader"),
            &session(earlier),
            &mut report,
        );
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        let symptom = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .expect("symptom");
        assert_eq!(symptom.object_name, "dropped rows 17");
        assert_eq!(symptom.provenance, Provenance::SelfGenerated);
        assert_candidate_balance(&report);
    }

    #[test]
    fn symptom_only_stated_after_the_citation_is_dropped() {
        let turns = vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "Morning.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::User,
                text: "Thanks".into(),
            },
            Turn {
                index: 2,
                role: TurnRole::Assistant,
                text: "The reader dropped rows.".into(),
            },
        ];
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply("dropped rows", "", "", "", 1, "Thanks"),
            &session(turns),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.kept, 0);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn first_turn_does_not_take_a_symptom_from_a_later_turn() {
        let turns = vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "Patched the reader and shipped it.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "The reader dropped rows near 4242.".into(),
            },
        ];
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply("dropped rows", "", "", "", 0, "Patched the reader"),
            &session(turns),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.kept, 0);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn symptom_absent_from_the_session_still_drops_the_bug() {
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply(
                "invented outage",
                "",
                "dropped rows patched",
                "",
                1,
                "Patched the reader",
            ),
            &session(reader_turns()),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert!(!report.drops.contains_key(DROP_VALUE_SUPPORT_PARTIAL));
        assert_candidate_balance(&report);
    }

    #[test]
    fn quote_must_still_be_in_a_turn() {
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply(
                "dropped rows",
                "",
                "",
                "",
                1,
                "this sentence was never said",
            ),
            &session(reader_turns()),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn partial_drops_stay_out_of_the_candidate_total() {
        let reply = serde_json::json!({
            "facts": [fact_json(1, "Patched the reader")],
            "bugs": [
                {
                    "symptom": "dropped rows",
                    "cause": "",
                    "fix": "invented remedy nowhere",
                    "status": "open",
                    "component": "billing ledger",
                    "turn": 1,
                    "quote": "Patched the reader",
                    "confidence": 0.9
                },
                {
                    "symptom": "dropped rows on 9991",
                    "cause": "",
                    "fix": "",
                    "status": "open",
                    "component": "",
                    "turn": 1,
                    "quote": "Patched the reader",
                    "confidence": 0.9
                }
            ],
            "releases": [{
                "version": "4242",
                "status": "released",
                "contents": "unrelated payload",
                "turn": 0,
                "quote": "near 4242 before",
                "confidence": 0.9
            }]
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("object"),
            &session(reader_turns()),
            &mut report,
        );
        // The setting names 9000, which is not in the session. The second bug's
        // number is not in the session. The release is kept; its contents, the
        // first bug's fix, and that bug's component are partial drops.
        assert_eq!(report.candidates, 4);
        assert_eq!(report.kept, 2);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(2));
        assert!(!report.drops.contains_key(DROP_VERSION));
        assert_eq!(
            report.drops.get(DROP_VALUE_SUPPORT_PARTIAL).copied(),
            Some(3)
        );
        assert_candidate_balance(&report);
        assert!(facts
            .iter()
            .any(|fact| fact.predicate == Predicate::IssueSymptom));
        assert!(facts
            .iter()
            .all(|fact| fact.predicate != Predicate::IssueFix));
        assert!(facts
            .iter()
            .all(|fact| fact.predicate != Predicate::ReleaseContents));
        assert!(facts.iter().all(|fact| fact.predicate != Predicate::Affects
            || fact.object_kind != Some(Kind::Component)));
    }

    #[test]
    fn kept_values_never_carry_an_unstated_number() {
        let turns = reader_turns();
        let raw = format!("{} {}", turns[0].text, turns[1].text);
        let session = session(turns);
        for n in 0..200 {
            for number in [n.to_string(), format!("{n}.5"), format!("-{n}")] {
                if raw.contains(&number) {
                    continue;
                }
                let mut report = IngestionReport::default();
                let facts = facts_from_reply(
                    &bug_reply(
                        &format!("dropped rows {number}"),
                        &format!("cause rows {number}"),
                        &format!("patched rows {number}"),
                        "",
                        1,
                        "Patched the reader",
                    ),
                    &session,
                    &mut report,
                );
                assert!(
                    facts.is_empty(),
                    "{number} was admitted: {:?}",
                    facts
                        .iter()
                        .map(|fact| fact.object_name.as_str())
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    report.drops.get(DROP_VALUE_SUPPORT).copied(),
                    Some(1),
                    "{number}"
                );
                assert_candidate_balance(&report);
            }
        }
        // A digit run inside a longer number is not that number.
        for symptom in [
            "dropped rows 42",
            "dropped rows 2",
            "dropped rows 4",
            "dropped rows 24",
            "dropped rows 242",
            "dropped rows 424",
            "dropped rows 42420",
            "dropped rows -1",
            "dropped rows 0.2",
        ] {
            let mut report = IngestionReport::default();
            let facts = facts_from_reply(
                &bug_reply(symptom, "", "", "", 1, "Patched the reader"),
                &session,
                &mut report,
            );
            assert!(facts.is_empty(), "{symptom}");
            assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        }
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply(
                "dropped rows 4242",
                "",
                "patched rows 9991",
                "",
                1,
                "Patched the reader",
            ),
            &session,
            &mut report,
        );
        assert!(facts
            .iter()
            .any(|fact| fact.object_name == "dropped rows 4242"));
        assert!(facts.iter().all(|fact| !fact.object_name.contains("9991")));
        assert_eq!(
            report.drops.get(DROP_VALUE_SUPPORT_PARTIAL).copied(),
            Some(1)
        );
        assert_candidate_balance(&report);

        let quote = "Patched the reader; the mark is `|`.";
        for value in ["|", ";", ",", "-->", "----", "|9", "a|"] {
            let (facts, report) = one_fact("mark", value, "setting", quote);
            assert_candidate_balance(&report);
            assert!(facts.iter().all(|fact| {
                super::super::ground::numbers_in(&fact.object_name)
                    .iter()
                    .all(|number| {
                        super::super::ground::numbers_in(quote)
                            .iter()
                            .any(|have| have == number)
                    })
            }));
            assert_eq!(facts.len() == 1, value == "|", "{value} {:?}", report.drops);
            if value == "|" {
                assert_eq!(facts[0].object_name, "|");
            }
        }
    }

    #[test]
    fn punctuation_setting_passes_gate_two_only_when_wrapped() {
        let keep = "The column separator is `|` in the export.";
        let (facts, report) = one_fact("column_separator", "|", "setting", keep);
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        assert_eq!(facts[0].object_name, "|");
        assert_eq!(facts[0].key, "column_separator");
        assert_eq!(facts[0].predicate, Predicate::Setting);
        assert_candidate_balance(&report);

        let bare = "The column separator is | in the export.";
        let (facts, report) = one_fact("column_separator", "|", "setting", bare);
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);

        for predicate in ["rule", "uses"] {
            let (facts, report) = one_fact("column_separator", "|", predicate, keep);
            assert!(facts.is_empty(), "{predicate}: {:?}", report.drops);
            assert_eq!(
                report.drops.get(DROP_VALUE_SUPPORT).copied(),
                Some(1),
                "{predicate}"
            );
            assert_candidate_balance(&report);
        }
    }

    fn one_fact(
        key: &str,
        object: &str,
        predicate: &str,
        quote: &str,
    ) -> (Vec<Fact>, IngestionReport) {
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: quote.into(),
        }];
        let mut item = fact_json(0, quote);
        let fields = item.as_object_mut().expect("fact");
        fields.insert("key".into(), Value::String(key.into()));
        fields.insert("object".into(), Value::String(object.into()));
        fields.insert("predicate".into(), Value::String(predicate.into()));
        let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("object"),
            &session(turns),
            &mut report,
        );
        (facts, report)
    }

    #[test]
    fn separator_quote_stores_the_turn_span_for_a_fact_and_a_bug() {
        let text = "if the deploy fails we keep rolling back to the last release";
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: text.into(),
        }];
        let quote = "rolling_back to the last release";
        let stored = "rolling back to the last release";
        let mut item = fact_json(0, quote);
        item["object"] = Value::String("last release".into());
        let reply = serde_json::json!({
            "facts": [item],
            "bugs": [{
                "symptom": "last release",
                "cause": "",
                "fix": "",
                "status": "open",
                "component": "",
                "turn": 0,
                "quote": quote,
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
        assert_eq!(report.kept, 2, "{:?}", report.drops);
        assert_candidate_balance(&report);
        assert!(facts.iter().all(|fact| fact.quote == stored));
        assert!(facts.iter().any(|fact| {
            fact.predicate == Predicate::Setting && fact.object_name == "last release"
        }));
        assert!(facts.iter().any(|fact| {
            fact.predicate == Predicate::IssueSymptom && fact.object_name == "last release"
        }));
    }

    #[test]
    fn underscore_quote_never_keeps_an_unstated_number() {
        let turns = reader_turns();
        let raw = format!("{} {}", turns[0].text, turns[1].text);
        let session = session(turns);
        let quote = "Patched_the_reader";
        for n in 0..200 {
            for number in [n.to_string(), format!("{n}.5"), format!("-{n}")] {
                if raw.contains(&number) {
                    continue;
                }
                let mut report = IngestionReport::default();
                let facts = facts_from_reply(
                    &bug_reply(&format!("dropped rows {number}"), "", "", "", 1, quote),
                    &session,
                    &mut report,
                );
                assert!(facts.is_empty(), "{number}");
                assert_eq!(
                    report.drops.get(DROP_VALUE_SUPPORT).copied(),
                    Some(1),
                    "{number}"
                );
            }
        }
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            &bug_reply("dropped rows 4242", "", "", "", 1, quote),
            &session,
            &mut report,
        );
        assert!(facts
            .iter()
            .any(|fact| fact.object_name == "dropped rows 4242"));
        assert!(facts.iter().all(|fact| {
            fact.quote == "Patched the reader"
                && super::super::ground::numbers_in(&fact.object_name)
                    .iter()
                    .all(|number| {
                        super::super::ground::numbers_in(&raw)
                            .iter()
                            .any(|have| have == number)
                    })
        }));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn separator_and_space_quotes_are_one_evidence_item() {
        let text = "if the deploy fails we keep rolling back to the last release";
        let stored = "rolling back to the last release";
        let turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: text.into(),
        }];
        let input = session(turns.clone());
        let extract = |quote: &str| {
            let mut item = fact_json(0, quote);
            item["object"] = Value::String("last release".into());
            let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
            let mut report = IngestionReport::default();
            let facts = facts_from_reply(reply.as_object().expect("object"), &input, &mut report);
            assert_eq!(facts.len(), 1, "{:?}", report.drops);
            assert_eq!(facts[0].quote, stored);
            facts
        };
        let first = extract("rolling_back to the last release");
        let second = extract("rolling back to the last release");
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        let llm = PanicProvider;
        let ctx = write::WriteCtx::new(&llm, &input.project, "s1", "2026-01-02", &turns);
        write::write_facts(&gm, &ctx, &first).await.unwrap();
        let again = write::write_facts(&gm, &ctx, &second).await.unwrap();
        assert_eq!(again.already_seen, 1);
        assert_eq!(again.reinforced, 0);
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let evidence = entities
            .iter()
            .find(|entity| entity.name == "tidepool")
            .expect("project")
            .attributes
            .as_ref()
            .unwrap()["v2"]["slots"][0]["evidence"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["quote"], stored);
    }

    #[test]
    fn agreement_citation_survives_the_gates() {
        let quote = "9123 is good.";
        let kept_turns = vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: quote.into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "Default port is now 9123.".into(),
            },
        ];
        let (facts, report) = grade_port(&kept_turns, 0, quote, "9123");
        assert_eq!(facts.len(), 1, "{:?}", report.drops);
        assert_eq!(facts[0].key, "port");
        assert_eq!(facts[0].object_name, "9123");
        assert_eq!(facts[0].turn, 0);
        assert_eq!(facts[0].quote, quote);
        assert_eq!(facts[0].provenance, Provenance::User);
        assert_eq!(report.kept, 1);
        assert!(report.drops.is_empty(), "{:?}", report.drops);

        let dropped_turns = vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: quote.into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "Port 9123 is taken, so I used 9124.".into(),
            },
        ];
        let (facts, report) = grade_port(&dropped_turns, 0, quote, "9123");
        assert!(facts.is_empty());
        assert_eq!(report.kept, 0);
        assert_eq!(report.drops.get(DROP_KEY_BINDING).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    fn grade_port(
        turns: &[Turn],
        turn: usize,
        quote: &str,
        value: &str,
    ) -> (Vec<Fact>, IngestionReport) {
        let mut item = fact_json(turn, quote);
        let object = item.as_object_mut().expect("fact");
        object.insert("key".into(), Value::String("port".into()));
        object.insert("object".into(), Value::String(value.into()));
        let reply = serde_json::json!({
            "facts": [item],
            "bugs": [],
            "releases": []
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("reply"),
            &session(turns.to_vec()),
            &mut report,
        );
        (facts, report)
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
        let timing = report.timing.expect("v2 timing");
        assert!(
            timing.total_ms >= timing.pass1_ms,
            "pass1 {} resolve {} write {} total {}",
            timing.pass1_ms,
            timing.resolve_ms,
            timing.write_ms,
            timing.total_ms
        );
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

    fn run_bug(
        turns: &[Turn],
        symptom: &str,
        cause: &str,
        fix: &str,
        turn: usize,
        quote: &str,
    ) -> (Vec<Fact>, IngestionReport) {
        let reply = bug_reply(symptom, cause, fix, "", turn, quote);
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(&reply, &session(turns.to_vec()), &mut report);
        (facts, report)
    }

    fn cache_turns() -> Vec<Turn> {
        vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "The cache returned stale entries after deploy.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "Confirmed: the TTL was ignored by the reader, so expired rows were served. I changed the reader to check expires_at on read.".into(),
            },
        ]
    }

    #[test]
    fn merged_bug_quote_keeps_the_cause_from_the_cited_turn() {
        let cause = "the TTL was ignored by the reader, so expired rows were served";
        let fix = "I changed the reader to check expires_at on read";
        let (facts, report) = run_bug(
            &cache_turns(),
            "cache returned stale entries",
            cause,
            fix,
            1,
            "the cache returned stale rows after the reader",
        );
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        assert_candidate_balance(&report);
        assert!(facts
            .iter()
            .all(|fact| fact.quote == cause && fact.turn == 1));
        let symptom = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .unwrap();
        assert_eq!(symptom.provenance, Provenance::SelfGenerated);
        let cause_fact = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueCause)
            .unwrap();
        assert_eq!(
            (cause_fact.object_name.as_str(), cause_fact.provenance),
            (cause, Provenance::Assistant)
        );
        let fix_fact = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueFix)
            .unwrap();
        assert_eq!(
            (fix_fact.object_name.as_str(), fix_fact.provenance),
            (fix, Provenance::Assistant)
        );
        let status = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueStatus)
            .unwrap();
        assert_eq!(
            (status.object_name.as_str(), status.provenance),
            ("fixed", Provenance::SelfGenerated)
        );
    }

    #[test]
    fn a_verbatim_symptom_is_the_stored_quote() {
        let symptom = "expired rows were served";
        let (facts, report) = run_bug(
            &cache_turns()[1..],
            symptom,
            "the TTL was ignored by the reader",
            "I changed the reader to check expires_at on read",
            1,
            "the cache returned stale rows after the reader",
        );
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        assert!(facts.iter().all(|fact| fact.quote == symptom));
    }

    #[test]
    fn a_field_only_in_another_turn_still_drops_the_bug() {
        let mut turns = cache_turns();
        turns[1].text = "Noted and left the rest unchanged for now.".into();
        let (facts, report) = run_bug(
            &turns,
            "cache returned stale entries",
            "stale entries after deploy",
            "returned stale entries after deploy",
            1,
            "the cache returned stale rows after the reader",
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn a_near_match_or_a_short_field_drops_the_bug() {
        let turns = cache_turns();
        for field in [
            "Cache Returned Stale Entries",
            "cache-returned stale entries",
            "cache_returned stale entries",
        ] {
            let (facts, report) =
                run_bug(&turns[..1], field, field, field, 0, "missing quote here");
            assert!(facts.is_empty(), "{field}");
            assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1), "{field}");
        }
        let (facts, report) = run_bug(
            &[Turn {
                index: 0,
                role: TurnRole::Assistant,
                text: "I changed the reader to check it on read.".into(),
            }],
            "not in this turn",
            "also absent here",
            "check it",
            0,
            "missing quote here",
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1));
    }

    #[test]
    fn an_accepted_quote_is_left_unchanged() {
        let user = "The cache returned stale entries after deploy.";
        let assistant =
            "Confirmed: the TTL was ignored by the reader, so expired rows were served.";
        let cases = [
            (
                user,
                assistant,
                "cache returned stale entries",
                "the TTL was ignored by the reader",
                "after deploy",
                "after deploy",
                0usize,
            ),
            (
                user,
                assistant,
                "cache returned stale entries",
                "the TTL was ignored by the reader",
                "the TTL was ignored by the reader",
                "the TTL was ignored by the reader",
                1,
            ),
            (
                "we keep rolling back to the last release",
                assistant,
                "keep rolling back",
                "the TTL was ignored by the reader",
                "rolling_back to the last release",
                "rolling back to the last release",
                0,
            ),
        ];
        for (left, right, symptom, cause, quote, stored, turn) in cases {
            let (facts, report) = run_bug(
                &[
                    Turn {
                        index: 0,
                        role: TurnRole::User,
                        text: left.into(),
                    },
                    Turn {
                        index: 1,
                        role: TurnRole::Assistant,
                        text: right.into(),
                    },
                ],
                symptom,
                cause,
                "",
                0,
                quote,
            );
            assert_eq!(report.kept, 1, "{quote} {:?}", report.drops);
            assert!(
                facts
                    .iter()
                    .all(|fact| fact.quote == stored && fact.turn == turn),
                "{quote}"
            );
        }
    }

    #[test]
    fn a_verbatim_fact_object_does_not_save_a_missed_quote() {
        let mut item = fact_json(0, "the cache returned stale rows after the reader");
        item["object"] = Value::String("stale entries after".into());
        let reply = serde_json::json!({"facts": [item], "bugs": [], "releases": []});
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().unwrap(),
            &session(cache_turns()),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    fn grade_fallback(
        turns: &[Turn],
        symptom: &str,
        cause: &str,
        fix: &str,
        cited: usize,
    ) -> (Vec<Fact>, IngestionReport) {
        let miss = "the cache returned stale rows after the reader";
        let chosen = SessionGround::prepare(turns).cite_field(cited, &[symptom, cause, fix]);
        let (facts, report) = run_bug(turns, symptom, cause, fix, cited, miss);
        let raw = turns
            .iter()
            .map(|turn| turn.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for fact in &facts {
            for number in super::super::ground::numbers_in(&fact.object_name) {
                assert!(
                    super::super::ground::numbers_in(&raw)
                        .iter()
                        .any(|have| have == &number),
                    "{number}"
                );
            }
        }
        let Some(chosen) = chosen else {
            assert!(facts.is_empty(), "{symptom}/{cause}/{fix}");
            assert_eq!(report.drops.get(DROP_QUOTE).copied(), Some(1));
            assert_candidate_balance(&report);
            return (facts, report);
        };
        if report.kept > 0 {
            assert!(facts.iter().all(|fact| fact.quote == chosen.quote));
        }
        let direct = bug_reply(symptom, cause, fix, "", cited, &chosen.quote);
        let mut other = IngestionReport::default();
        let direct_facts = facts_from_reply(&direct, &session(turns.to_vec()), &mut other);
        assert_eq!(facts, direct_facts, "{}", chosen.quote);
        assert_eq!(report.kept, other.kept);
        assert_eq!(report.candidates, other.candidates);
        assert_eq!(report.drops, other.drops);
        (facts, report)
    }

    fn shaped(span: &str, mode: usize) -> String {
        match mode {
            0 => span.to_string(),
            1 => span.replacen(' ', "-", 1),
            _ => String::new(),
        }
    }

    #[test]
    fn fallback_kept_bug_matches_quoting_the_chosen_field() {
        let rows = [
            (
                "The cache returned stale entries after deploy.",
                [
                    "cache returned stale entries",
                    "stale entries after deploy",
                    "returned stale entries after deploy",
                ],
            ),
            (
                "Confirmed: the TTL was ignored by the reader, so expired rows were served. I changed the reader to check expires_at on read.",
                [
                    "the TTL was ignored by the reader",
                    "expired rows were served",
                    "I changed the reader to check expires_at on read",
                ],
            ),
            (
                "The reader missed port 4242 after the deploy.",
                [
                    "missed port 4242 after the deploy",
                    "reader missed port 4242",
                    "port 4242 after the deploy",
                ],
            ),
            (
                "The timeout was raised and the reader now checks port 4242.",
                [
                    "timeout was raised and the reader",
                    "reader now checks port 4242",
                    "checks port 4242",
                ],
            ),
        ];
        for pair in [(0, 1), (2, 3)] {
            for swap in [false, true] {
                let (first, second) = if swap { (pair.1, pair.0) } else { pair };
                for cited in [0usize, 1] {
                    for mode in 0..27 {
                        let spans = rows[if cited == 0 { first } else { second }].1;
                        let symptom = shaped(spans[0], mode % 3);
                        let cause = shaped(spans[1], (mode / 3) % 3);
                        let fix = shaped(spans[2], mode / 9);
                        let turns = [
                            Turn {
                                index: 0,
                                role: if swap {
                                    TurnRole::Assistant
                                } else {
                                    TurnRole::User
                                },
                                text: rows[first].0.into(),
                            },
                            Turn {
                                index: 1,
                                role: if swap {
                                    TurnRole::User
                                } else {
                                    TurnRole::Assistant
                                },
                                text: rows[second].0.into(),
                            },
                        ];
                        grade_fallback(&turns, &symptom, &cause, &fix, cited);
                    }
                }
            }
        }
        let turns = [
            Turn {
                index: 0,
                role: TurnRole::User,
                text: rows[2].0.into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: rows[3].0.into(),
            },
        ];
        let (facts, report) =
            grade_fallback(&turns, "reader missed port 4242", "timeout hit 9090", "", 0);
        assert_eq!(
            report.drops.get(DROP_VALUE_SUPPORT_PARTIAL).copied(),
            Some(1)
        );
        assert!(facts
            .iter()
            .all(|fact| fact.quote == "reader missed port 4242"
                && !fact.object_name.contains("9090")));
    }

    #[test]
    fn fallback_still_runs_the_later_bug_checks() {
        let fix = "changed the reader to check expires_at on read";
        let turns = [Turn {
            index: 0,
            role: TurnRole::Assistant,
            text: "I changed the reader to check expires_at on read.".into(),
        }];
        let mut low = bug_reply("not in this turn", "", fix, "", 0, "missing quote here");
        low["bugs"][0]["confidence"] = serde_json::json!(0.4);
        let mut report = IngestionReport::default();
        assert!(facts_from_reply(&low, &session(turns.to_vec()), &mut report).is_empty());
        assert_eq!(report.drops.get(DROP_CONFIDENCE).copied(), Some(1));
        let (facts, report) = run_bug(&turns, "", "", fix, 0, "missing quote here");
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_PARSE).copied(), Some(1));
        let (facts, report) = run_bug(
            &turns,
            "not in this turn",
            &"y".repeat(301),
            fix,
            0,
            "missing quote here",
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_TOO_LONG).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn fallback_bug_write_is_idempotent() {
        let cause = "the TTL was ignored by the reader, so expired rows were served";
        let fix = "I changed the reader to check expires_at on read";
        let turns = cache_turns();
        let (facts, report) = run_bug(
            &turns,
            "cache returned stale entries",
            cause,
            fix,
            1,
            "the cache returned stale rows after the reader",
        );
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        let llm = PanicProvider;
        let input = session(turns.clone());
        let ctx = write::WriteCtx::new(&llm, &input.project, "s1", "2026-01-02", &turns);
        write::write_facts(&gm, &ctx, &facts).await.unwrap();
        let before = stored_belief(&gm).await;
        let again = write::write_facts(&gm, &ctx, &facts).await.unwrap();
        assert_eq!(again.already_seen, u32::try_from(facts.len()).unwrap());
        assert_eq!(again.reinforced, 0);
        assert_eq!(again.slots, 0);
        assert_eq!(again.edges, 0);
        assert_eq!(stored_belief(&gm).await, before);
    }

    #[cfg(feature = "embedded")]
    async fn stored_belief(gm: &GraphMemory) -> Vec<(String, f64, f64)> {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let mut rows = Vec::new();
        for entity in &entities {
            let Some(slots) = entity
                .attributes
                .as_ref()
                .and_then(|attrs| attrs["v2"]["slots"].as_array())
            else {
                continue;
            };
            for slot in slots {
                rows.push((
                    format!("{}:{}:{}", entity.name, slot["predicate"], slot["value"]),
                    slot["alpha"].as_f64().unwrap(),
                    slot["beta"].as_f64().unwrap(),
                ));
            }
        }
        for edge in crate::graph::crud::list_all_relationships(gm.db())
            .await
            .unwrap()
        {
            rows.push((
                format!("edge:{}:{}", edge.rel_type, edge.id_string()),
                edge.alpha.unwrap(),
                edge.beta.unwrap(),
            ));
        }
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
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

    fn widget_turns() -> Vec<Turn> {
        vec![
            Turn {
                index: 0,
                role: TurnRole::User,
                text: "widget import froze on a large archive: TimeoutError.".into(),
            },
            Turn {
                index: 1,
                role: TurnRole::Assistant,
                text: "The importer stopped because the socket closed. I raised the wait.".into(),
            },
        ]
    }

    fn widget_session(turns: Vec<Turn>) -> SessionInput {
        let mut input = session(turns);
        input.project.name = "widget".into();
        input.project.aliases = vec!["widget".into()];
        input
    }

    fn run_widget(
        turns: &[Turn],
        symptom: &str,
        cause: &str,
        fix: &str,
        turn: usize,
        quote: &str,
    ) -> (Vec<Fact>, IngestionReport) {
        let reply = bug_reply(symptom, cause, fix, "", turn, quote);
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(&reply, &widget_session(turns.to_vec()), &mut report);
        (facts, report)
    }

    fn symptom_of(facts: &[Fact]) -> &Fact {
        facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueSymptom)
            .expect("symptom")
    }

    #[test]
    fn span_symptom_is_kept_with_the_cited_role() {
        let (facts, report) = run_widget(
            &widget_turns(),
            "froze on a large archive",
            "the socket closed",
            "raised the wait",
            0,
            "TimeoutError",
        );
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        assert!(!report.drops.contains_key(DROP_VALUE_SUPPORT));
        assert_candidate_balance(&report);
        let symptom = symptom_of(&facts);
        assert_eq!(symptom.object_name, "froze on a large archive");
        assert_eq!(symptom.provenance, Provenance::User);
        assert_eq!(symptom.turn, 0);
        assert_eq!(symptom.quote, "TimeoutError");
        let cause = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueCause)
            .expect("cause");
        assert_eq!(
            (cause.object_name.as_str(), cause.provenance),
            ("the socket closed", Provenance::SelfGenerated)
        );
        let fix = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueFix)
            .expect("fix");
        assert_eq!(
            (fix.object_name.as_str(), fix.provenance),
            ("raised the wait", Provenance::SelfGenerated)
        );
        let status = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueStatus)
            .expect("status");
        assert_eq!(status.object_name, "fixed");
        assert_eq!(status.provenance, Provenance::SelfGenerated);
    }

    #[test]
    fn scattered_span_drops_the_bug() {
        let (facts, report) = run_widget(
            &widget_turns(),
            "import froze archive",
            "the socket closed",
            "raised the wait",
            0,
            "TimeoutError",
        );
        assert!(facts.is_empty());
        assert_eq!(report.kept, 0);
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn symptom_only_in_a_later_turn_drops() {
        let mut turns = widget_turns();
        turns[1].text = "the socket closed during upload".into();
        let (facts, report) = run_widget(
            &turns,
            "socket closed during upload",
            "",
            "",
            0,
            "TimeoutError",
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn cause_span_and_setting_stay_on_their_own_rules() {
        let (facts, report) = run_widget(
            &widget_turns(),
            "TimeoutError",
            "froze on a large archive",
            "",
            0,
            "TimeoutError",
        );
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        assert_eq!(symptom_of(&facts).provenance, Provenance::User);
        let cause = facts
            .iter()
            .find(|fact| fact.predicate == Predicate::IssueCause)
            .expect("cause");
        assert_eq!(cause.object_name, "froze on a large archive");
        assert_eq!(cause.provenance, Provenance::SelfGenerated);
        assert!(facts
            .iter()
            .all(|fact| fact.predicate != Predicate::IssueFix));

        let reply = serde_json::json!({
            "facts": [{
                "subject": "widget",
                "subject_type": "project",
                "predicate": "setting",
                "role": "none",
                "key": "archive_limit",
                "object": "froze on a large archive",
                "object_type": "literal",
                "status": "decided",
                "replaces": "",
                "turn": 0,
                "quote": "TimeoutError",
                "confidence": 0.9
            }],
            "bugs": [],
            "releases": []
        });
        let mut report = IngestionReport::default();
        let facts = facts_from_reply(
            reply.as_object().expect("reply"),
            &widget_session(widget_turns()),
            &mut report,
        );
        assert!(facts.is_empty());
        assert_eq!(report.drops.get(DROP_VALUE_SUPPORT).copied(), Some(1));
        assert_candidate_balance(&report);
    }

    #[test]
    fn span_symptom_matches_quoting_the_symptom() {
        let cause = "the socket closed";
        let fix = "raised the wait";
        let (span, span_report) = run_widget(
            &widget_turns(),
            "froze on a large archive",
            cause,
            fix,
            0,
            "TimeoutError",
        );
        let (quoted, quoted_report) = run_widget(
            &widget_turns(),
            "froze on a large archive",
            cause,
            fix,
            0,
            "froze on a large archive",
        );
        assert_eq!(span_report.kept, 1);
        assert_eq!(quoted_report.kept, 1);
        assert!(span_report.drops.is_empty(), "{:?}", span_report.drops);
        assert!(quoted_report.drops.is_empty(), "{:?}", quoted_report.drops);
        assert!(span.iter().all(|fact| fact.quote == "TimeoutError"));
        assert!(quoted
            .iter()
            .all(|fact| fact.quote == "froze on a large archive"));
        let blank = |facts: Vec<Fact>| {
            facts
                .into_iter()
                .map(|mut fact| {
                    fact.quote.clear();
                    fact
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(blank(span), blank(quoted));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn known_match_does_not_change_the_request_or_read_before_the_provider() {
        use std::sync::Mutex;

        use crate::graph::llm::Completion;

        const PRE_BLOCK_USER: &str = "Project: tidepool\nSession date: 2026-01-02\nTranscript:\n[T0] USER: Ship the shelf today.\n\nExtract the durable facts, bugs and releases as JSON.";
        let reply = r#"{"facts":[{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"","object":"shelf","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Ship the shelf today.","confidence":0.9}],"bugs":[],"releases":[]}"#;

        struct Seen {
            during: Mutex<u32>,
            reply: &'static str,
        }

        #[async_trait::async_trait]
        impl LlmProvider for Seen {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Err(GraphError::Llm("plain complete was called".into()))
            }

            async fn complete_structured_chat(
                &self,
                system: &str,
                messages: &[Message],
                _: &Value,
                _: u32,
            ) -> Result<Completion, GraphError> {
                assert_eq!(
                    super::super::known::lookup_count(),
                    0,
                    "the store was read before the provider returned"
                );
                assert_eq!(system, PASS1_SYSTEM_BARE);
                assert_eq!(messages[2], Message::user(PRE_BLOCK_USER));
                assert!(!messages[2].content.contains("Known facts"));
                *self.during.lock().expect("lock") = super::super::known::lookup_count();
                Ok(Completion::unmeasured(self.reply))
            }
        }

        fn plant(memory: &std::path::Path) {
            let cwd = memory.join("tidepool");
            std::fs::create_dir_all(&cwd).unwrap();
            input::write_turns_sidecar(
                memory,
                1,
                "s",
                "2026-01-02",
                Some(cwd.to_str().unwrap()),
                &[crate::conversation::ConversationEntry::UserMessage(
                    "Ship the shelf today.".into(),
                )],
            )
            .unwrap();
        }

        let matched = tempfile::tempdir().unwrap();
        plant(matched.path());
        std::fs::write(
            matched.path().join(".recall-echo.toml"),
            "[graph]\nknown_match = true\n",
        )
        .unwrap();
        let gm = GraphMemory::open_embedded(&matched.path().join("graph"))
            .await
            .unwrap();
        super::super::known::reset_lookups();
        let provider = Seen {
            during: Mutex::new(1),
            reply,
        };
        let report = extract_session(
            &gm,
            matched.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert_eq!(*provider.during.lock().expect("lock"), 0);
        assert_eq!(super::super::known::lookup_count(), 1);
        assert_eq!(report.kept, 1, "{:?}", report.drops);
        assert!(report.timing.unwrap().match_ms.is_some());

        let off = tempfile::tempdir().unwrap();
        plant(off.path());
        let gm = GraphMemory::open_embedded(&off.path().join("graph"))
            .await
            .unwrap();
        super::super::known::reset_lookups();
        let provider = Seen {
            during: Mutex::new(1),
            reply,
        };
        let report = extract_session(
            &gm,
            off.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert_eq!(super::super::known::lookup_count(), 0);
        assert!(report.timing.unwrap().match_ms.is_none());
        assert!(report.known_match.is_blank());
    }

    #[cfg(feature = "embedded")]
    fn hold_config(drop_restatements: bool) -> String {
        let mut body = String::from("[graph]\nconsolidate = true\nconsolidate_hold = true\n");
        if drop_restatements {
            body.push_str("drop_restatements = true\n");
        }
        body
    }

    #[cfg(feature = "embedded")]
    fn write_archive(memory: &std::path::Path, text: &str) {
        std::fs::create_dir_all(memory.join("conversations")).unwrap();
        std::fs::write(
            memory.join("conversations/conversation-001.md"),
            format!(
                "---\nlog: 1\ndate: \"2026-04-03T00:00:00Z\"\nsession_id: \"s3\"\n---\n\n\
                 ### User\n\n{text}\n"
            ),
        )
        .unwrap();
    }

    #[cfg(feature = "embedded")]
    async fn seed_pending_binder(gm: &GraphMemory) {
        struct Quiet;
        #[async_trait::async_trait]
        impl LlmProvider for Quiet {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                panic!("resolver called");
            }
        }
        let quiet = Quiet;
        let project = ProjectIdentity {
            name: "acme".into(),
            aliases: Vec::new(),
        };
        let canvas_turns = [Turn {
            index: 0,
            role: TurnRole::User,
            text: "The binder is canvas.".into(),
        }];
        let canvas = Fact {
            subject_name: "acme".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: "binder".into(),
            object_name: "canvas".into(),
            object_kind: None,
            status: "decided".into(),
            turn: 0,
            quote: "The binder is canvas.".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        };
        let ctx = super::write::WriteCtx::new(&quiet, &project, "s1", "2026-04-01", &canvas_turns);
        super::write::write_facts(gm, &ctx, &[canvas])
            .await
            .unwrap();
        let linen_turns = [Turn {
            index: 0,
            role: TurnRole::User,
            text: "Store the pages in linen.".into(),
        }];
        let linen = Fact {
            subject_name: "acme".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: "binder".into(),
            object_name: "linen".into(),
            object_kind: None,
            status: "decided".into(),
            turn: 0,
            quote: "Store the pages in linen.".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        };
        let mut ctx =
            super::write::WriteCtx::new(&quiet, &project, "s2", "2026-04-02", &linen_turns);
        ctx.match_on = true;
        ctx.hold_on = true;
        let opened = super::write::write_facts(gm, &ctx, &[linen]).await.unwrap();
        assert_eq!(opened.holds_opened, 1);
    }

    #[cfg(feature = "embedded")]
    async fn acme_slots(gm: &GraphMemory) -> Vec<Value> {
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let acme = entities
            .iter()
            .find(|entity| entity.name == "acme")
            .expect("acme");
        acme.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(feature = "embedded")]
    async fn empty_extraction_confirms_pending(config: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(memory.join(".recall-echo.toml"), config).unwrap();
        write_archive(memory, "Switched canvas for linen.");
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        super::write::ensure_v2_store(&gm).await.unwrap();
        seed_pending_binder(&gm).await;

        struct Empty;
        #[async_trait::async_trait]
        impl LlmProvider for Empty {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Ok(r#"{"facts":[],"bugs":[],"releases":[]}"#.to_string())
            }
        }
        let report = extract_session(&gm, memory, 1, &IngestContext::new("s3", Some(1)), &Empty)
            .await
            .unwrap();
        assert_eq!(report.slots_written, 0);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
        assert!(
            report.hold.holds_confirmed >= 1,
            "empty facts left the hold: {:?}",
            report.hold
        );
        assert_eq!(report.hold.holds_opened, 0);
        let slots = acme_slots(&gm).await;
        let canvas = slots.iter().find(|slot| slot["value"] == "canvas").unwrap();
        let linen = slots.iter().find(|slot| slot["value"] == "linen").unwrap();
        assert_eq!(canvas["ended_by"], "s3:superseded");
        assert!(linen.get("pending").is_none());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn an_empty_extraction_still_settles_a_pending_hold() {
        empty_extraction_confirms_pending(&hold_config(false)).await;
        empty_extraction_confirms_pending(&hold_config(true)).await;
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn empty_extraction_with_the_flag_and_no_pending_hold_opens_none() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(memory.join(".recall-echo.toml"), hold_config(true)).unwrap();
        write_archive(memory, "Noted the shelf.");
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        struct Empty;
        #[async_trait::async_trait]
        impl LlmProvider for Empty {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Ok(r#"{"facts":[],"bugs":[],"releases":[]}"#.to_string())
            }
        }
        let report = extract_session(&gm, memory, 1, &IngestContext::new("s3", Some(1)), &Empty)
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.hold.is_blank(), "{:?}", report.hold);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
        assert_eq!(report.slots_written, 0);
    }

    #[cfg(feature = "embedded")]
    struct PairReply {
        reply: &'static str,
        judge: &'static str,
    }

    #[cfg(feature = "embedded")]
    #[async_trait::async_trait]
    impl LlmProvider for PairReply {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Ok(self.judge.to_string())
        }

        async fn complete_structured_chat(
            &self,
            system: &str,
            _: &[Message],
            _: &Value,
            _: u32,
        ) -> Result<crate::graph::llm::Completion, GraphError> {
            assert_eq!(system, PASS1_SYSTEM_BARE);
            Ok(crate::graph::llm::Completion::unmeasured(self.reply))
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_filtered_restatement_still_confirms_a_pending_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(memory.join(".recall-echo.toml"), hold_config(true)).unwrap();
        write_archive(
            memory,
            "Switched canvas for linen. The timeout is 10s. Use 10s.",
        );
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        super::write::ensure_v2_store(&gm).await.unwrap();
        seed_pending_binder(&gm).await;
        let provider = PairReply {
            reply: r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"timeout","object":"10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The timeout is 10s.","confidence":0.9},{"subject":"acme","subject_type":"project","predicate":"rule","role":"none","key":"","object":"use 10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Use 10s.","confidence":0.9}],"bugs":[],"releases":[]}"#,
            judge: r#"{"pairs":[]}"#,
        };
        let report = extract_session(
            &gm,
            memory,
            1,
            &IngestContext::new("s3", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(report.hold.holds_opened, 0, "{:?}", report.hold);
        assert!(
            report.hold.holds_confirmed >= 1,
            "filtered pair changed confirmation: {:?}",
            report.hold
        );
        let slots = acme_slots(&gm).await;
        let canvas = slots.iter().find(|slot| slot["value"] == "canvas").unwrap();
        let linen = slots.iter().find(|slot| slot["value"] == "linen").unwrap();
        assert_eq!(canvas["ended_by"], "s3:superseded");
        assert!(linen.get("pending").is_none());
        assert!(slots.iter().any(|slot| {
            slot["predicate"] == "setting" && slot["key"] == "timeout" && slot["value"] == "10s"
        }));
        assert!(slots.iter().all(|slot| slot["predicate"] != "rule"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_filtered_restatement_still_drops_a_reasserted_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(memory.join(".recall-echo.toml"), hold_config(true)).unwrap();
        write_archive(memory, "The binder stays canvas. Use canvas.");
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        super::write::ensure_v2_store(&gm).await.unwrap();
        seed_pending_binder(&gm).await;
        let provider = PairReply {
            reply: r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"binder","object":"canvas","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The binder stays canvas.","confidence":0.9},{"subject":"acme","subject_type":"project","predicate":"rule","role":"none","key":"","object":"use canvas","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Use canvas.","confidence":0.9}],"bugs":[],"releases":[]}"#,
            judge: r#"{"pairs":[]}"#,
        };
        let report = extract_session(
            &gm,
            memory,
            1,
            &IngestContext::new("s3", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(report.hold.holds_opened, 0, "{:?}", report.hold);
        assert!(
            report.hold.holds_dropped >= 1,
            "filtered pair changed reassertion: {:?}",
            report.hold
        );
        let slots = acme_slots(&gm).await;
        let canvas = slots
            .iter()
            .find(|slot| {
                slot["key"] == "binder" && slot["value"] == "canvas" && slot["pending"].is_null()
            })
            .expect("live canvas");
        assert!(canvas["ended_by"].is_null(), "{canvas}");
        let linen = slots.iter().find(|slot| slot["value"] == "linen").unwrap();
        assert_eq!(linen["ended_by"], "s3:hold_dropped");
        assert!(slots.iter().all(|slot| slot["predicate"] != "rule"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_filtered_restatement_leaves_an_ambiguous_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(memory.join(".recall-echo.toml"), hold_config(true)).unwrap();
        write_archive(memory, "The harbor bell is brass.");
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        super::write::ensure_v2_store(&gm).await.unwrap();
        seed_pending_binder(&gm).await;
        let provider = PairReply {
            reply: r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"bell","object":"brass","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The harbor bell is brass.","confidence":0.9},{"subject":"acme","subject_type":"project","predicate":"rule","role":"none","key":"","object":"bell is brass","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The harbor bell is brass.","confidence":0.9}],"bugs":[],"releases":[]}"#,
            judge: r#"{"pairs":[{"i":1,"verdict":"unrelated","quote":""}]}"#,
        };
        let report = extract_session(
            &gm,
            memory,
            1,
            &IngestContext::new("s3", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(report.hold.holds_opened, 0, "{:?}", report.hold);
        assert_eq!(report.hold.holds_confirmed, 0, "{:?}", report.hold);
        assert_eq!(report.hold.holds_dropped, 0, "{:?}", report.hold);
        assert!(
            report.hold.holds_ambiguous >= 1,
            "filtered pair settled an ambiguous hold: {:?}",
            report.hold
        );
        let slots = acme_slots(&gm).await;
        let canvas = slots.iter().find(|slot| slot["value"] == "canvas").unwrap();
        let linen = slots.iter().find(|slot| slot["value"] == "linen").unwrap();
        assert!(canvas["ended_by"].is_null(), "{canvas}");
        assert!(
            linen.get("pending").is_some_and(Value::is_object),
            "{linen}"
        );
        assert!(slots
            .iter()
            .any(|slot| slot["key"] == "bell" && slot["value"] == "brass"));
        assert!(slots.iter().all(|slot| slot["predicate"] != "rule"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_filtered_restatement_does_not_open_a_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(memory.join(".recall-echo.toml"), hold_config(true)).unwrap();
        write_archive(memory, "The timeout is 10s. Use 10s.");
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        let provider = PairReply {
            reply: r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"timeout","object":"10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The timeout is 10s.","confidence":0.9},{"subject":"acme","subject_type":"project","predicate":"rule","role":"none","key":"","object":"use 10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Use 10s.","confidence":0.9}],"bugs":[],"releases":[]}"#,
            judge: r#"{"pairs":[]}"#,
        };
        let report = extract_session(
            &gm,
            memory,
            1,
            &IngestContext::new("s3", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(report.kept, 2);
        assert!(report.hold.is_blank(), "{:?}", report.hold);
        let slots = acme_slots(&gm).await;
        assert!(slots.iter().any(|slot| {
            slot["key"] == "timeout" && slot["value"] == "10s" && slot.get("pending").is_none()
        }));
        assert!(slots.iter().all(|slot| slot["predicate"] != "rule"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn a_restatement_is_dropped_instead_of_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        std::fs::write(
            memory.join(".recall-echo.toml"),
            "[graph]\nconsolidate = true\nconsolidate_links = true\ndrop_restatements = true\n",
        )
        .unwrap();
        write_archive(memory, "The timeout is 10s. Use 10s.");
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        let provider = PairReply {
            reply: r#"{"facts":[{"subject":"acme","subject_type":"project","predicate":"setting","role":"none","key":"timeout","object":"10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The timeout is 10s.","confidence":0.9},{"subject":"acme","subject_type":"project","predicate":"rule","role":"none","key":"","object":"use 10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Use 10s.","confidence":0.9}],"bugs":[],"releases":[]}"#,
            judge: r#"{"pairs":[]}"#,
        };
        let report = extract_session(
            &gm,
            memory,
            1,
            &IngestContext::new("s3", Some(1)),
            &provider,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert!(report.links.is_blank(), "{:?}", report.links);
        let slots = acme_slots(&gm).await;
        assert!(slots.iter().any(|slot| {
            slot["key"] == "timeout" && slot["value"] == "10s" && slot.get("derived_from").is_none()
        }));
        assert!(slots.iter().all(|slot| slot["predicate"] != "rule"));
        assert!(slots.iter().all(|slot| slot.get("derived_from").is_none()));
    }

    fn harbor_setting(key: &str, value: &str) -> Fact {
        Fact {
            subject_name: "harbor".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: key.into(),
            object_name: value.into(),
            object_kind: None,
            status: "decided".into(),
            turn: 0,
            quote: "quoted".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }

    fn harbor_rule(object: &str) -> Fact {
        let mut fact = harbor_setting("", object);
        fact.predicate = Predicate::Rule;
        fact.key.clear();
        fact
    }

    fn apply_restatements(facts: Vec<Fact>) -> (Vec<Fact>, IngestionReport) {
        let mut report = IngestionReport::default();
        let kept = drop_restating_rules(facts, &mut report);
        (kept, report)
    }

    fn fact_keys(facts: &[Fact]) -> Vec<&str> {
        facts.iter().map(|fact| fact.key.as_str()).collect()
    }

    #[test]
    fn a_restating_rule_is_dropped_and_the_setting_stays() {
        let mut report = IngestionReport {
            candidates: 2,
            kept: 2,
            ..IngestionReport::default()
        };
        let kept = drop_restating_rules(
            vec![
                harbor_setting("timeout", "10s"),
                harbor_rule("use 10s"),
                harbor_rule("timeout is 10s"),
            ],
            &mut report,
        );
        // Two rules restate one setting, so the group stays.
        assert_eq!(kept.len(), 3);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
        assert_eq!(report.kept, 2);

        let mut report = IngestionReport {
            candidates: 2,
            kept: 2,
            ..IngestionReport::default()
        };
        let kept = drop_restating_rules(
            vec![harbor_setting("timeout", "10s"), harbor_rule("use 10s")],
            &mut report,
        );
        assert_eq!(fact_keys(&kept), vec!["timeout"]);
        assert_eq!(kept[0].object_name, "10s");
        assert_eq!(kept[0].predicate, Predicate::Setting);
        assert_eq!(kept[0].status, "decided");
        assert!(kept.iter().all(|fact| fact.predicate != Predicate::Rule));
        assert!(kept.iter().all(|fact| fact.status != "considered"));
        assert_eq!(report.candidates, 2);
        assert_eq!(report.kept, 2);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));

        let (kept, report) = apply_restatements(vec![
            harbor_rule("timeout is 10s"),
            harbor_setting("timeout", "10s"),
        ]);
        assert_eq!(fact_keys(&kept), vec!["timeout"]);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
    }

    #[test]
    fn projection_folds_prose_case_and_interior_whitespace() {
        for object in ["Use   10s", "USE 10s", "Timeout   IS\t10s"] {
            let (kept, report) =
                apply_restatements(vec![harbor_setting("timeout", "10s"), harbor_rule(object)]);
            assert_eq!(fact_keys(&kept), vec!["timeout"], "{object}");
            assert_eq!(
                report.drops.get(DROP_RESTATEMENT).copied(),
                Some(1),
                "{object}"
            );
        }
    }

    #[test]
    fn exceptions_modality_negation_and_constraints_stay() {
        for object in [
            "use 10s unless retrying",
            "do not use 10s",
            "only on retry",
            "must never go below 10s",
            "use 10s on retry",
            "use 10s as a fallback",
        ] {
            let (kept, report) =
                apply_restatements(vec![harbor_setting("timeout", "10s"), harbor_rule(object)]);
            assert_eq!(kept.len(), 2, "{object}");
            assert!(!report.drops.contains_key(DROP_RESTATEMENT), "{object}");
        }
    }

    #[test]
    fn different_keys_values_units_subjects_kinds_and_turns_stay() {
        let (kept, report) = apply_restatements(vec![
            harbor_setting("interval", "10s"),
            harbor_rule("timeout is 10s"),
        ]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "30s"),
            harbor_rule("use 10s"),
        ]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10ms"),
            harbor_rule("use 10s"),
        ]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let mut other = harbor_rule("use 10s");
        other.subject_name = "relay".into();
        let (kept, report) = apply_restatements(vec![harbor_setting("timeout", "10s"), other]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let mut component = harbor_setting("timeout", "10s");
        component.subject_kind = Kind::Component;
        let (kept, report) = apply_restatements(vec![component, harbor_rule("use 10s")]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let mut later = harbor_rule("use 10s");
        later.turn = 1;
        let (kept, report) = apply_restatements(vec![harbor_setting("timeout", "10s"), later]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
    }

    #[test]
    fn quote_only_and_token_only_overlap_stays() {
        for object in [
            "10s",
            "10",
            "\"10s\"",
            "the call waits 10s before giving up",
        ] {
            let (kept, report) =
                apply_restatements(vec![harbor_setting("timeout", "10s"), harbor_rule(object)]);
            assert_eq!(kept.len(), 2, "{object}");
            assert!(!report.drops.contains_key(DROP_RESTATEMENT), "{object}");
        }
    }

    #[test]
    fn several_settings_or_rules_keep_every_fact() {
        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10s"),
            harbor_setting("interval", "10s"),
            harbor_rule("use 10s"),
        ]);
        assert_eq!(kept.len(), 3);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10s"),
            harbor_rule("use 10s"),
            harbor_rule("timeout is 10s"),
        ]);
        assert_eq!(kept.len(), 3);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
    }

    #[test]
    fn a_mixed_ambiguous_group_keeps_every_fact() {
        let orders = [
            vec![
                harbor_setting("timeout", "10s"),
                harbor_setting("interval", "10s"),
                harbor_rule("use 10s"),
                harbor_rule("timeout is 10s"),
            ],
            vec![
                harbor_rule("use 10s"),
                harbor_rule("timeout is 10s"),
                harbor_setting("timeout", "10s"),
                harbor_setting("interval", "10s"),
            ],
            vec![
                harbor_setting("timeout", "10s"),
                harbor_rule("use 10s"),
                harbor_setting("interval", "10s"),
                harbor_rule("timeout is 10s"),
            ],
        ];
        for facts in orders {
            let (kept, report) = apply_restatements(facts.clone());
            assert_eq!(kept, facts);
            assert!(!report.drops.contains_key(DROP_RESTATEMENT));
        }
    }

    #[test]
    fn an_isolated_pair_still_drops_the_rule() {
        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10s"),
            harbor_rule("use 10s"),
        ]);
        assert_eq!(fact_keys(&kept), vec!["timeout"]);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
    }

    #[test]
    fn two_isolated_pairs_each_drop() {
        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10s"),
            harbor_rule("use 10s"),
            harbor_setting("retries", "3"),
            harbor_rule("use 3"),
        ]);
        assert_eq!(fact_keys(&kept), vec!["timeout", "retries"]);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(2));
    }

    #[test]
    fn a_keyed_rule_beside_an_unconnected_setting_still_drops() {
        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10s"),
            harbor_setting("interval", "10s"),
            harbor_rule("timeout is 10s"),
        ]);
        assert_eq!(fact_keys(&kept), vec!["timeout", "interval"]);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
    }

    #[test]
    fn a_rule_with_replaces_or_another_status_stays() {
        let mut replaced = harbor_rule("use 10s");
        replaced.replaces = "5s".into();
        let (kept, report) = apply_restatements(vec![harbor_setting("timeout", "10s"), replaced]);
        assert_eq!(kept.len(), 2);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));

        let mut considered = harbor_rule("use 10s");
        considered.status = "considered".into();
        let (kept, report) = apply_restatements(vec![harbor_setting("timeout", "10s"), considered]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].status, "considered");
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
    }

    #[test]
    fn unknown_expressions_stay() {
        for object in [
            "set the timeout to 10s",
            "keep the shelf labeled",
            "the timeout is 10s",
            "apply 10s",
        ] {
            let (kept, report) =
                apply_restatements(vec![harbor_setting("timeout", "10s"), harbor_rule(object)]);
            assert_eq!(kept.len(), 2, "{object}");
            assert!(!report.drops.contains_key(DROP_RESTATEMENT), "{object}");
        }
    }

    #[test]
    fn uses_roles_and_the_go_version_pair_stay() {
        for role in [Role::Language, Role::Ci, Role::Hosting] {
            let mut uses = harbor_setting("", "Go");
            uses.predicate = Predicate::Uses;
            uses.role = Some(role);
            uses.object_kind = Some(Kind::Technology);
            let mut version = harbor_setting("go_version", "1.22");
            version.subject_name = "tidepool".into();
            uses.subject_name = "tidepool".into();
            let (kept, report) = apply_restatements(vec![uses, version]);
            assert_eq!(kept.len(), 2, "{role:?}");
            assert!(!report.drops.contains_key(DROP_RESTATEMENT), "{role:?}");
        }

        let mut uses = harbor_setting("", "Go");
        uses.subject_name = "tidepool".into();
        uses.predicate = Predicate::Uses;
        uses.role = Some(Role::Language);
        uses.object_kind = Some(Kind::Technology);
        uses.quote = "a Go 1.22 service".into();
        let mut version = harbor_setting("go_version", "1.22");
        version.subject_name = "tidepool".into();
        version.quote = "a Go 1.22 service".into();
        let mut nearby = harbor_rule("use Go");
        nearby.subject_name = "tidepool".into();
        let (kept, report) = apply_restatements(vec![uses, version, nearby]);
        assert_eq!(kept.len(), 3);
        assert!(!report.drops.contains_key(DROP_RESTATEMENT));
    }

    #[test]
    fn remapped_part_of_stays() {
        let mention = Mention {
            subject: "tidepool".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Uses,
            role: Role::None,
            key: String::new(),
            object: "export command".into(),
            object_kind: Some(Kind::Component),
            status: "decided".into(),
            turn: 0,
            quote: "We use the export command instead of csv".into(),
            confidence: 0.9,
            provenance: TurnRole::User,
        };
        let part = remap(mention).expect("project uses component");
        assert_eq!(part.predicate, Predicate::PartOf);
        let (kept, report) = apply_restatements(vec![
            part,
            harbor_setting("timeout", "10s"),
            harbor_rule("use 10s"),
        ]);
        assert!(kept.iter().any(|fact| fact.predicate == Predicate::PartOf));
        assert!(kept.iter().any(|fact| fact.key == "timeout"));
        assert!(kept.iter().all(|fact| fact.predicate != Predicate::Rule));
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
    }

    #[test]
    fn bug_and_release_expansions_stay() {
        let bug_turns = vec![Turn {
            index: 0,
            role: TurnRole::User,
            text: "The cache returned stale entries.".into(),
        }];
        let bug_reply = serde_json::json!({
            "facts": [],
            "bugs": [{
                "symptom": "stale entries",
                "cause": "",
                "fix": "",
                "status": "open",
                "component": "cache",
                "turn": 0,
                "quote": "The cache returned stale entries.",
                "confidence": 0.9
            }],
            "releases": []
        });
        let mut collected = IngestionReport::default();
        let mut facts = facts_from_reply(
            bug_reply.as_object().expect("bug"),
            &session(bug_turns),
            &mut collected,
        );
        assert!(facts
            .iter()
            .any(|fact| fact.predicate == Predicate::IssueSymptom));
        facts.push(harbor_setting("timeout", "10s"));
        facts.push(harbor_rule("use 10s"));
        let (kept, report) = apply_restatements(facts);
        assert!(kept
            .iter()
            .any(|fact| fact.predicate == Predicate::IssueSymptom));
        assert!(kept
            .iter()
            .any(|fact| fact.predicate == Predicate::IssueStatus));
        assert!(kept.iter().any(|fact| fact.predicate == Predicate::Affects));
        assert!(kept.iter().all(|fact| fact.predicate != Predicate::Rule));
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));

        let release_reply = serde_json::json!({
            "facts": [],
            "bugs": [],
            "releases": [{
                "version": "1.4.0",
                "status": "planned",
                "contents": "the shelf",
                "turn": 1,
                "quote": "Tag v1.4.0 today.",
                "confidence": 0.9
            }]
        });
        let mut collected = IngestionReport::default();
        let mut facts = facts_from_reply(
            release_reply.as_object().expect("release"),
            &session(turns()),
            &mut collected,
        );
        assert!(facts
            .iter()
            .any(|fact| fact.predicate == Predicate::ReleaseStatus));
        assert!(facts.iter().any(|fact| fact.predicate == Predicate::PartOf));
        facts.push(harbor_setting("timeout", "10s"));
        facts.push(harbor_rule("timeout is 10s"));
        let (kept, report) = apply_restatements(facts);
        assert!(kept
            .iter()
            .any(|fact| fact.predicate == Predicate::ReleaseStatus));
        assert!(kept
            .iter()
            .any(|fact| fact.predicate == Predicate::ReleaseContents));
        assert!(kept.iter().any(|fact| fact.predicate == Predicate::PartOf));
        assert!(kept.iter().all(|fact| fact.predicate != Predicate::Rule));
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
    }

    #[test]
    fn an_extra_setting_stays() {
        let (kept, report) = apply_restatements(vec![
            harbor_setting("timeout", "10s"),
            harbor_setting("binder", "linen"),
            harbor_rule("use 10s"),
        ]);
        assert_eq!(fact_keys(&kept), vec!["timeout", "binder"]);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
    }

    #[test]
    fn sibling_order_does_not_change_the_decision() {
        let setting = harbor_setting("timeout", "10s");
        let extra = harbor_setting("retries", "3");
        let rule = harbor_rule("use 10s");
        let (forward, forward_report) =
            apply_restatements(vec![setting.clone(), rule.clone(), extra.clone()]);
        let (backward, backward_report) = apply_restatements(vec![extra, rule, setting]);
        assert_eq!(fact_keys(&forward), vec!["timeout", "retries"]);
        assert_eq!(fact_keys(&backward), vec!["retries", "timeout"]);
        assert_eq!(forward_report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(
            backward_report.drops.get(DROP_RESTATEMENT).copied(),
            Some(1)
        );
    }

    #[test]
    fn filtering_before_reconcile_keeps_match_targets_aligned() {
        let turn = "Switch the log format to json lines. The timeout is 10s. Use 10s.";
        let mut timeout = harbor_setting("timeout", "10s");
        timeout.subject_name = "tidepool".into();
        timeout.quote = "The timeout is 10s.".into();
        let mut rule = harbor_rule("use 10s");
        rule.subject_name = "tidepool".into();
        rule.quote = "Use 10s.".into();
        let mut filled = harbor_setting("", "json lines");
        filled.subject_name = "tidepool".into();
        filled.quote = "Switch the log format to json lines.".into();
        let (filtered, report) = apply_restatements(vec![rule, timeout, filled]);
        assert_eq!(filtered.len(), 2);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        let ground = SessionGround::prepare(&[Turn {
            index: 0,
            role: TurnRole::User,
            text: turn.into(),
        }]);
        let rows = vec![super::super::known::KnownRow {
            subject: "tidepool".into(),
            predicate: "setting".into(),
            key: "log_format".into(),
            value: "plain".into(),
            role: String::new(),
            id: "slot:log".into(),
            status: "decided".into(),
        }];
        let reconciled = super::super::known_match::reconcile(&filtered, &rows, &ground);
        assert_eq!(reconciled.targets.len(), reconciled.facts.len());
        let index = reconciled
            .facts
            .iter()
            .position(|fact| fact.object_name == "json lines")
            .expect("filled setting");
        assert_eq!(index, 1);
        assert_eq!(reconciled.facts[index].key, "log_format");
        let target = reconciled.targets[index].as_ref().expect("aligned target");
        assert_eq!(target.key, "log_format");
        assert_eq!(target.value, "plain");
        assert!(reconciled.targets[0].is_none());
        assert_eq!(reconciled.report.key_filled, 1);
    }

    #[test]
    fn drop_restatements_reads_only_its_own_flag() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!drop_restatements_enabled(dir.path()));
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\ndrop_restatements = false\n",
        )
        .unwrap();
        assert!(!drop_restatements_enabled(dir.path()));
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\nknown_match = true\nconsolidate = true\nknown_facts_context = true\nconsolidate_hold = true\n",
        )
        .unwrap();
        assert!(!drop_restatements_enabled(dir.path()));
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\ndrop_restatements = true\n",
        )
        .unwrap();
        assert!(drop_restatements_enabled(dir.path()));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn scripted_pair_writes_the_setting_without_a_rule() {
        use std::sync::atomic::{AtomicU64, Ordering};

        use crate::graph::llm::Completion;

        const TURN: &str = "The timeout is 10s. Use 10s.";
        const REPLY: &str = r#"{"facts":[{"subject":"tidepool","subject_type":"project","predicate":"rule","role":"none","key":"","object":"use 10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Use 10s.","confidence":0.9},{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"timeout","object":"10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The timeout is 10s.","confidence":0.9}],"bugs":[],"releases":[]}"#;

        struct Scripted {
            reply: &'static str,
            pass1_calls: AtomicU64,
            other_calls: AtomicU64,
        }

        #[async_trait::async_trait]
        impl LlmProvider for Scripted {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                self.other_calls.fetch_add(1, Ordering::SeqCst);
                Ok(r#"{"decision":"create","reason":"distinct"}"#.to_string())
            }

            async fn complete_structured_chat(
                &self,
                system: &str,
                _: &[Message],
                _: &Value,
                _: u32,
            ) -> Result<Completion, GraphError> {
                if system.starts_with(PASS1_SYSTEM_BARE) {
                    self.pass1_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(system, PASS1_SYSTEM_BARE);
                    return Ok(Completion::unmeasured(self.reply));
                }
                self.other_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Completion::unmeasured(
                    r#"{"decision":"create","reason":"distinct"}"#,
                ))
            }
        }

        fn plant(memory: &std::path::Path, config: &str) {
            let cwd = memory.join("tidepool");
            std::fs::create_dir_all(&cwd).unwrap();
            input::write_turns_sidecar(
                memory,
                1,
                "s",
                "2026-01-02",
                Some(cwd.to_str().unwrap()),
                &[crate::conversation::ConversationEntry::UserMessage(
                    TURN.into(),
                )],
            )
            .unwrap();
            if !config.is_empty() {
                std::fs::write(memory.join(".recall-echo.toml"), config).unwrap();
            }
        }

        async fn slots(gm: &GraphMemory) -> Vec<Value> {
            let entities = crate::graph::crud::list_entities(gm.db(), None)
                .await
                .unwrap();
            let tidepool = entities
                .iter()
                .find(|entity| entity.name == "tidepool")
                .expect("tidepool");
            tidepool.attributes.as_ref().unwrap()["v2"]["slots"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        }

        let on_dir = tempfile::tempdir().unwrap();
        plant(on_dir.path(), "[graph]\ndrop_restatements = true\n");
        let on_gm = GraphMemory::open_embedded(&on_dir.path().join("graph"))
            .await
            .unwrap();
        let on_provider = Scripted {
            reply: REPLY,
            pass1_calls: AtomicU64::new(0),
            other_calls: AtomicU64::new(0),
        };
        let on_report = extract_session(
            &on_gm,
            on_dir.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &on_provider,
        )
        .await
        .unwrap();
        assert!(on_report.errors.is_empty(), "{:?}", on_report.errors);
        assert_eq!(on_report.candidates, 2);
        assert_eq!(on_report.kept, 2);
        assert_eq!(on_report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(on_provider.pass1_calls.load(Ordering::SeqCst), 1);
        let on_slots = slots(&on_gm).await;
        assert!(
            on_slots.iter().any(|slot| {
                slot["predicate"] == "setting"
                    && slot["key"] == "timeout"
                    && slot["value"] == "10s"
                    && slot["status"] == "decided"
            }),
            "{on_slots:?}"
        );
        assert!(
            on_slots.iter().all(|slot| slot["predicate"] != "rule"),
            "{on_slots:?}"
        );
        assert!(
            on_slots
                .iter()
                .all(|slot| slot["value"] != "use 10s" && slot["status"] != "considered"),
            "{on_slots:?}"
        );

        let off_dir = tempfile::tempdir().unwrap();
        plant(off_dir.path(), "");
        let off_gm = GraphMemory::open_embedded(&off_dir.path().join("graph"))
            .await
            .unwrap();
        let off_provider = Scripted {
            reply: REPLY,
            pass1_calls: AtomicU64::new(0),
            other_calls: AtomicU64::new(0),
        };
        let off_report = extract_session(
            &off_gm,
            off_dir.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &off_provider,
        )
        .await
        .unwrap();
        assert!(off_report.errors.is_empty(), "{:?}", off_report.errors);
        assert!(!off_report.drops.contains_key(DROP_RESTATEMENT));
        assert_eq!(off_report.candidates, 2);
        assert_eq!(off_report.kept, 2);
        assert_eq!(off_provider.pass1_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            on_provider.other_calls.load(Ordering::SeqCst),
            off_provider.other_calls.load(Ordering::SeqCst)
        );
        assert_eq!(on_report.llm_calls, off_report.llm_calls);
        let off_slots = slots(&off_gm).await;
        assert!(off_slots
            .iter()
            .any(|slot| slot["predicate"] == "setting" && slot["value"] == "10s"));
        assert!(
            off_slots
                .iter()
                .any(|slot| slot["predicate"] == "rule" && slot["value"] == "use 10s"),
            "{off_slots:?}"
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    async fn known_match_after_the_session_pass_fills_the_surviving_setting() {
        use crate::graph::llm::Completion;

        const TURN: &str = "Switch the log format to json lines. The timeout is 10s. Use 10s.";
        const REPLY: &str = r#"{"facts":[{"subject":"tidepool","subject_type":"project","predicate":"rule","role":"none","key":"","object":"use 10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Use 10s.","confidence":0.9},{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"timeout","object":"10s","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"The timeout is 10s.","confidence":0.9},{"subject":"tidepool","subject_type":"project","predicate":"setting","role":"none","key":"","object":"json lines","object_type":"literal","status":"decided","replaces":"","turn":0,"quote":"Switch the log format to json lines.","confidence":0.9}],"bugs":[],"releases":[]}"#;

        struct Scripted(&'static str);

        #[async_trait::async_trait]
        impl LlmProvider for Scripted {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
                Ok(r#"{"decision":"create","reason":"distinct"}"#.to_string())
            }

            async fn complete_structured_chat(
                &self,
                system: &str,
                _: &[Message],
                _: &Value,
                _: u32,
            ) -> Result<Completion, GraphError> {
                assert_eq!(system, PASS1_SYSTEM_BARE);
                Ok(Completion::unmeasured(self.0))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("tidepool");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            dir.path().join(".recall-echo.toml"),
            "[graph]\ndrop_restatements = true\nknown_match = true\n",
        )
        .unwrap();
        input::write_turns_sidecar(
            dir.path(),
            1,
            "s",
            "2026-01-02",
            Some(cwd.to_str().unwrap()),
            &[crate::conversation::ConversationEntry::UserMessage(
                TURN.into(),
            )],
        )
        .unwrap();
        let gm = GraphMemory::open_embedded(&dir.path().join("graph"))
            .await
            .unwrap();
        write::ensure_v2_store(&gm).await.unwrap();
        let project = ProjectIdentity {
            name: "tidepool".into(),
            aliases: vec!["tidepool".into()],
        };
        let seeded = Fact {
            subject_name: "tidepool".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Setting,
            role: None,
            key: "log_format".into(),
            object_name: "plain".into(),
            object_kind: None,
            status: "decided".into(),
            turn: 0,
            quote: "The log format was plain.".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        };
        let quiet = Scripted(REPLY);
        let ctx = write::WriteCtx::new(&quiet, &project, "s0", "2026-01-01", &[]);
        write::write_facts(&gm, &ctx, &[seeded]).await.unwrap();
        let report = extract_session(
            &gm,
            dir.path(),
            1,
            &IngestContext::new("s", Some(1)),
            &quiet,
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.candidates, 3);
        assert_eq!(report.kept, 3);
        assert_eq!(report.drops.get(DROP_RESTATEMENT).copied(), Some(1));
        assert_eq!(report.known_match.key_filled, 1);
        let entities = crate::graph::crud::list_entities(gm.db(), None)
            .await
            .unwrap();
        let tidepool = entities
            .iter()
            .find(|entity| entity.name == "tidepool")
            .expect("tidepool");
        let stored = tidepool.attributes.as_ref().unwrap()["v2"]["slots"]
            .as_array()
            .unwrap();
        assert!(
            stored.iter().any(|slot| {
                slot["predicate"] == "setting"
                    && slot["key"] == "log_format"
                    && slot["value"] == "json lines"
                    && slot["status"] == "decided"
            }),
            "{stored:?}"
        );
        assert!(stored.iter().any(|slot| {
            slot["key"] == "timeout" && slot["value"] == "10s" && slot["status"] == "decided"
        }));
        assert!(stored.iter().all(|slot| slot["predicate"] != "rule"));
    }
}
