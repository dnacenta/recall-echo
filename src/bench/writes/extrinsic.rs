// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Exact-value checks for derived questions, on two channels.
//!
//! The store channel looks for the answer on a live decided row of the final
//! store, v2 slots included. The recall channel looks for it in the text
//! `recall_query` returns for the question. The first 8,000 characters of
//! that text are the read budget; the whole text, which the server stops at
//! 24,000 characters, is a sidecar. Current-value questions also scan the
//! recall text for stale values, anywhere in the window rather than only in
//! the first result.
//!
//! Matching is literal and ignores case and runs of whitespace. A value must
//! stand as its own span: `8080` does not hit inside `18080`, nor `black`
//! inside `blacklist`, nor `3.12` inside `3.12.1`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use super::grade::{FinalEdge, QuestionScore};
use super::script::{FactScript, QuestionKind};
use crate::error::RecallError;
use crate::graph::edge_view::record_id;
use crate::graph::error::GraphError;
use crate::graph::GraphMemory;
use crate::mcp::tools::Tool;
use crate::mcp::{GraphBackend, McpServer};
use crate::serve::Request;

/// Characters of the recall render a reader is assumed to see.
pub(crate) const READ_BUDGET_CHARS: usize = 8_000;
/// The MCP result ceiling. A recall render is never longer than this, plus
/// the server's cut notice.
pub(crate) const SIDECAR_CHARS: usize = 24_000;
/// SurrealKV's lock file inside `graph/surreal/`.
const LOCK_FILE: &str = "LOCK";
/// Polls of a store's lock before a copy gives up.
const LOCK_POLLS: u32 = 250;
/// Pause between polls: five seconds in all.
const LOCK_POLL: Duration = Duration::from_millis(20);

/// Answer and stale hits over one recall render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RenderHits {
    pub answer: bool,
    pub answer_24k: bool,
    pub stale: bool,
    pub stale_24k: bool,
}

/// Look for `answer` and every `stale` value in the 8k window and the 24k
/// sidecar of `render`. An empty render hits nothing.
pub(crate) fn scan_render(render: &str, answer: &str, stale: &[String]) -> RenderHits {
    let budget = clip_chars(render, READ_BUDGET_CHARS);
    let sidecar = clip_chars(render, SIDECAR_CHARS);
    RenderHits {
        answer: has_value(budget, answer),
        answer_24k: has_value(sidecar, answer),
        stale: has_any(budget, stale),
        stale_24k: has_any(sidecar, stale),
    }
}

/// Whether `answer` stands on a live decided row.
///
/// A row with no status is a v1 edge and counts as decided. Considered,
/// pending, rejected and retired rows do not count.
pub(crate) fn store_hit(rows: &[FinalEdge], answer: &str) -> bool {
    rows.iter().any(|row| {
        row.live
            && !row.pending
            && matches!(row.status.as_deref(), None | Some("decided"))
            && has_value(&row.object, answer)
    })
}

/// Fill the recall fields of `scores` from `renders`. Both are parallel to
/// `script.questions`.
pub(crate) fn fill_recall(
    script: &FactScript,
    scores: &mut [QuestionScore],
    renders: &[String],
) -> Result<(), RecallError> {
    for ((question, score), render) in script.questions.iter().zip(scores).zip(renders) {
        let stale = script.stale_values(question)?;
        let hits = scan_render(render, &question.answer, &stale);
        score.recall_hit = hits.answer;
        score.recall_hit_24k = hits.answer_24k;
        if question.kind == QuestionKind::CurrentValue {
            score.stale_in_budget = Some(hits.stale);
            score.stale_in_budget_24k = Some(hits.stale_24k);
        }
    }
    Ok(())
}

/// What `recall_query` returns for each of `queries`, read from a copy of
/// the store under `memory_dir`.
///
/// The copy is made under `scratch` and removed afterwards; the graded store
/// is never opened. Each query goes through the MCP server with the tool's
/// own defaults. A query moves access counts and hotness ranks on them, so
/// the copy's entity counts are put back after each one: no query sees
/// another's. A graph that is missing, or that cannot be copied or opened,
/// reads as empty renders.
pub(crate) async fn recall_renders(
    memory_dir: &Path,
    scratch: &Path,
    queries: &[&str],
) -> Vec<String> {
    let renders = read_copy(memory_dir, scratch, queries)
        .await
        .unwrap_or_else(|_| vec![String::new(); queries.len()]);
    let _ = fs::remove_dir_all(scratch);
    renders
}

async fn read_copy(
    memory_dir: &Path,
    scratch: &Path,
    queries: &[&str],
) -> Result<Vec<String>, RecallError> {
    let copy = scratch.join("memory");
    copy_closed_store(memory_dir, &copy).await?;
    let graph = GraphMemory::open(&copy.join("graph")).await?;
    let server = McpServer::new(CopyBackend { graph });
    let renders = query_each(&server, queries).await;
    drop(server);
    // The copy is removed next. Let its close finish first.
    drop(take_lock(&copy.join("graph").join("surreal")).await?);
    renders
}

async fn query_each(
    server: &McpServer<CopyBackend>,
    queries: &[&str],
) -> Result<Vec<String>, RecallError> {
    let graph = &server.backend().graph;
    let counts = access_counts(graph).await?;
    let mut renders = Vec::with_capacity(queries.len());
    for query in queries {
        renders.push(recall_query(server, query).await);
        restore_access_counts(graph, &counts).await?;
    }
    Ok(renders)
}

/// The text of one `recall_query` call. An error result reads as empty.
async fn recall_query(server: &McpServer<CopyBackend>, query: &str) -> String {
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": Tool::Query.name(), "arguments": { "query": query } },
    });
    let Some(reply) = server.handle_line(&call.to_string()).await else {
        return String::new();
    };
    let result = &reply["result"];
    if result["isError"] != serde_json::Value::Bool(false) {
        return String::new();
    }
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The copy, served by the same dispatch the daemon runs.
struct CopyBackend {
    graph: GraphMemory,
}

#[async_trait::async_trait]
impl GraphBackend for CopyBackend {
    async fn execute(&self, request: &Request) -> Result<serde_json::Value, RecallError> {
        crate::serve::dispatch_graph(&self.graph, request)
            .await
            .into_result()
    }
}

/// Entity access counts by record id. Episode reads move episode counts too,
/// but nothing in a query ranks on those.
async fn access_counts(graph: &GraphMemory) -> Result<HashMap<String, i64>, RecallError> {
    let mut response = graph
        .db()
        .query("SELECT id, access_count FROM entity")
        .await
        .map_err(GraphError::from)?;
    let rows: Vec<serde_json::Value> = crate::graph::deserialize_take(&mut response, 0)?;
    Ok(rows
        .iter()
        .map(|row| {
            (
                record_id(row.get("id").unwrap_or(&serde_json::Value::Null)),
                row.get("access_count")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            )
        })
        .collect())
}

/// Put back every entity count a query moved.
async fn restore_access_counts(
    graph: &GraphMemory,
    before: &HashMap<String, i64>,
) -> Result<(), RecallError> {
    for (id, count) in access_counts(graph).await? {
        let Some(&was) = before.get(&id) else {
            continue;
        };
        if count == was {
            continue;
        }
        graph
            .db()
            .query("UPDATE type::record($id) SET access_count = $count")
            .bind(("id", id))
            .bind(("count", was))
            .await
            .and_then(|response| response.check())
            .map_err(GraphError::from)?;
    }
    Ok(())
}

/// Copy the store under `memory_dir` to `copy` once its last connection has
/// closed: `graph/surreal/`, the run's config, and the embedding cache.
async fn copy_closed_store(memory_dir: &Path, copy: &Path) -> Result<(), RecallError> {
    let graph = memory_dir.join("graph");
    let surreal = graph.join("surreal");
    let lock = take_lock(&surreal).await?;
    let copy_graph = copy.join("graph");
    copy_tree(&surreal, &copy_graph.join("surreal"))?;
    share_cache(&graph.join("models"), &copy_graph.join("models"))?;
    let config = crate::config::config_path(memory_dir);
    if config.is_file() {
        fs::copy(&config, crate::config::config_path(copy))?;
    }
    drop(lock);
    Ok(())
}

/// Take a store's lock once its last connection has let go of it.
///
/// SurrealKV releases `LOCK` as the last step of closing, after its final
/// flush, so while this lock is held no connection is writing the store.
async fn take_lock(surreal: &Path) -> Result<fs::File, RecallError> {
    let lock = fs::File::open(surreal.join(LOCK_FILE))?;
    for _ in 0..LOCK_POLLS {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(fs::TryLockError::WouldBlock) => tokio::time::sleep(LOCK_POLL).await,
            Err(fs::TryLockError::Error(err)) => return Err(err.into()),
        }
    }
    Err(RecallError::Other(format!(
        "{} stayed locked",
        surreal.display()
    )))
}

/// Copy a directory tree. The lock file is left out: the copy makes its own.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == LOCK_FILE {
            continue;
        }
        let dest = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Point the copy at the embedding cache the run already filled.
#[cfg(unix)]
fn share_cache(cache: &Path, to: &Path) -> std::io::Result<()> {
    if !cache.is_dir() {
        return Ok(());
    }
    std::os::unix::fs::symlink(fs::canonicalize(cache)?, to)
}

/// Copy the embedding cache the run already filled.
#[cfg(not(unix))]
fn share_cache(cache: &Path, to: &Path) -> std::io::Result<()> {
    if !cache.is_dir() {
        return Ok(());
    }
    copy_tree(cache, to)
}

/// The first `max` characters of `text`.
pub(crate) fn clip_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Whether any of `values` stands in `text`.
pub(crate) fn has_any(text: &str, values: &[String]) -> bool {
    values.iter().any(|value| has_value(text, value))
}

/// Whether `value` stands as its own span in `text`.
pub(crate) fn has_value(text: &str, value: &str) -> bool {
    let needle = normalize(value);
    if needle.is_empty() {
        return false;
    }
    let hay = normalize(text);
    let mut from = 0;
    while let Some(found) = hay[from..].find(&needle) {
        let start = from + found;
        let end = start + needle.len();
        if open_before(&hay, start, &needle) && open_after(&hay, end, &needle) {
            return true;
        }
        from = start + hay[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn token_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// A dot or a hyphen continues a token when a token character is on its far
/// side: `3.12.1`, `shelf.db-wal`.
fn joiner(ch: char) -> bool {
    ch == '.' || ch == '-'
}

fn open_before(hay: &str, start: usize, needle: &str) -> bool {
    if !needle.chars().next().is_some_and(token_char) {
        return true;
    }
    open_side(hay[..start].chars().rev())
}

fn open_after(hay: &str, end: usize, needle: &str) -> bool {
    if !needle.chars().next_back().is_some_and(token_char) {
        return true;
    }
    open_side(hay[end..].chars())
}

fn open_side(mut outward: impl Iterator<Item = char>) -> bool {
    match outward.next() {
        None => true,
        Some(ch) if token_char(ch) => false,
        Some(ch) if joiner(ch) => !outward.next().is_some_and(token_char),
        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    /// `body` padded with filler lines until it is at least `chars` long.
    fn padded(body: &str, chars: usize) -> String {
        let mut text = body.to_string();
        while text.chars().count() < chars {
            text.push_str("\n   filler line about nothing in particular");
        }
        text
    }

    fn row(object: &str, live: bool, status: Option<&str>, pending: bool) -> FinalEdge {
        FinalEdge {
            id: format!("edge:{object}"),
            subject: "shelf".into(),
            relation: "setting:port".into(),
            object: object.into(),
            live,
            status: status.map(str::to_string),
            pending,
            retired_in: None,
            matched: None,
            pattern_fact: None,
        }
    }

    #[test]
    fn a_value_matches_as_its_own_span_ignoring_case_and_spacing() {
        assert!(has_value("1. Port [concept] — 8765, matched", "8765"));
        assert!(has_value("We use Ruff for linting.", "ruff"));
        assert!(has_value("ruff's formatter", "ruff"));
        assert!(has_value("listen on 127.0.0.1:8765", "8765"));
        assert!(has_value("move it to\n   shelf.db.", "shelf.db"));
        assert!(has_value("run `just   test` now", "just test"));
        assert!(has_value("Python 3.12, managed with uv", "3.12"));
    }

    #[test]
    fn a_short_generic_value_does_not_hit_inside_a_longer_string() {
        assert!(!has_value("listen on 18080", "8080"));
        assert!(!has_value("port 80801", "8080"));
        assert!(!has_value("the blacklist grew", "black"));
        assert!(!has_value("pinned 3.12.1", "3.12"));
        assert!(!has_value("released v13.12", "3.12"));
        assert!(!has_value("left shelf.db-wal behind", "shelf.db"));
        assert!(!has_value("bookmark_tags_idx", "bookmark_tags"));
        assert!(!has_value("anything", ""));
        assert!(!has_value("", "8080"));
    }

    #[test]
    fn a_later_clean_span_still_hits_after_an_embedded_one() {
        assert!(has_value("18080 then 8080", "8080"));
    }

    #[test]
    fn the_budget_and_the_sidecar_clip_on_characters() {
        let text = "é".repeat(10);
        assert_eq!(clip_chars(&text, 3), "ééé");
        assert_eq!(clip_chars(&text, 50), text);
        assert_eq!(clip_chars("", 8), "");
        let long = padded("start", SIDECAR_CHARS * 2);
        assert_eq!(
            clip_chars(&long, READ_BUDGET_CHARS).chars().count(),
            READ_BUDGET_CHARS
        );
        assert_eq!(
            clip_chars(&long, SIDECAR_CHARS).chars().count(),
            SIDECAR_CHARS
        );
    }

    #[test]
    fn a_stale_value_at_rank_one_is_in_budget() {
        let render = padded(
            "Memory for \"which port\":\n\n1. 8080 [concept] — score 0.91, matched directly\n\
             \n2. 8765 [concept] — score 0.80, matched directly",
            READ_BUDGET_CHARS * 2,
        );
        let hits = scan_render(&render, "8765", &values(&["8080"]));
        assert!(hits.answer && hits.answer_24k);
        assert!(hits.stale && hits.stale_24k);
    }

    #[test]
    fn a_stale_value_only_at_the_back_of_the_window_is_in_budget() {
        let head = padded("1. 8765 [concept] — score 0.91", READ_BUDGET_CHARS - 200);
        let render = padded(&format!("{head}\n5. port was 8080"), READ_BUDGET_CHARS * 2);
        let at = render.find("8080").unwrap();
        assert!(render[..at].chars().count() > READ_BUDGET_CHARS - 200);
        assert!(render[..at].chars().count() < READ_BUDGET_CHARS);
        let hits = scan_render(&render, "8765", &values(&["8080"]));
        assert!(hits.answer);
        assert!(hits.stale, "a stale value below rank 1 still counts");
        assert!(hits.stale_24k);
    }

    #[test]
    fn a_stale_value_only_past_the_budget_is_in_the_sidecar_alone() {
        let head = padded("1. 8765 [concept] — score 0.91", READ_BUDGET_CHARS + 100);
        let render = format!("{head}\n9. port was 8080");
        let hits = scan_render(&render, "8765", &values(&["8080"]));
        assert!(hits.answer);
        assert!(!hits.stale);
        assert!(hits.stale_24k);
    }

    #[test]
    fn a_value_past_the_sidecar_is_not_seen() {
        let render = format!("{}\nport 8765", padded("", SIDECAR_CHARS + 10));
        let hits = scan_render(&render, "8765", &[]);
        assert!(!hits.answer && !hits.answer_24k);
    }

    #[test]
    fn no_stale_value_present_reads_false() {
        let hits = scan_render("1. 8765 [concept] — score 0.91", "8765", &values(&["8080"]));
        assert!(hits.answer);
        assert!(!hits.stale && !hits.stale_24k);
        let none = scan_render("1. 8765 [concept]", "8765", &[]);
        assert!(!none.stale && !none.stale_24k);
    }

    #[test]
    fn a_cross_key_stale_value_is_found_by_its_text() {
        // The old value lived under another key. Only its text is scanned for,
        // so a mention under any heading counts.
        let render = "1. shelf [project]\n   storage_path was shelf.json, now storage is shelf.db";
        let hits = scan_render(render, "shelf.db", &values(&["shelf.json"]));
        assert!(hits.answer && hits.stale);
    }

    #[test]
    fn an_old_value_still_shown_beside_the_current_one_sets_both_flags() {
        let hits = scan_render("8765\n8080", "8765", &values(&["8080"]));
        assert!(hits.answer && hits.stale);
    }

    #[test]
    fn an_empty_render_hits_nothing() {
        assert_eq!(
            scan_render("", "8765", &values(&["8080"])),
            RenderHits::default()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_missing_or_unreadable_graph_reads_as_empty_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let scratch = tmp.path().join("copy");
        let renders = recall_renders(&missing, &scratch, &["which port", "which host"]).await;
        assert_eq!(renders, vec![String::new(), String::new()]);
        assert!(!scratch.exists());

        let unreadable = tmp.path().join("unreadable");
        fs::create_dir_all(unreadable.join("graph")).unwrap();
        fs::write(unreadable.join("graph").join("surreal"), "not a store").unwrap();
        let renders = recall_renders(&unreadable, &scratch, &["which port"]).await;
        assert_eq!(renders, vec![String::new()]);
        assert!(!scratch.exists());
    }

    /// Entity access counts on the store under `memory`, read and closed.
    #[cfg(feature = "embedded")]
    async fn graded_counts(memory: &Path) -> HashMap<String, i64> {
        let graph = GraphMemory::open(&memory.join("graph")).await.unwrap();
        let counts = access_counts(&graph).await.unwrap();
        drop(graph);
        drop(
            take_lock(&memory.join("graph").join("surreal"))
                .await
                .unwrap(),
        );
        counts
    }

    #[cfg(feature = "embedded")]
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(
        not(embed_tests),
        ignore = "embeds: set RECALL_ECHO_MODELS_DIR to a seeded models dir"
    )]
    async fn recall_reads_leave_the_graded_store_and_each_other_alone() {
        use super::super::run::{
            run, Consolidate, ConsolidateComponents, ConsolidateKeyRename, ConsolidateLinks,
            ConsolidateOrphanSweep, ConsolidateReplacesCue, ConsolidateReplacesDeparture,
            ConsolidateRetireComponent, ConsolidateRuleFragments, DropGenericUses,
            DropRestatements, Extractor, GroundComponentEdges, KnownFactsContext, KnownKeyBinding,
            KnownMatch, OptOuts, RecoverTurns,
        };
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/writes/history.toml");
        let script = super::super::script::load_script(&path).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        run(
            &script,
            tmp.path(),
            Extractor::Scripted,
            crate::graph::PipelineVersion::V1,
            KnownFactsContext::Off,
            KnownMatch::Off,
            Consolidate::Off,
            DropRestatements::Off,
            ConsolidateLinks::Off,
            GroundComponentEdges::Off,
            ConsolidateComponents::Off,
            RecoverTurns::Off,
            DropGenericUses::Off,
            ConsolidateKeyRename::Off,
            ConsolidateReplacesCue::Off,
            ConsolidateReplacesDeparture::Off,
            ConsolidateOrphanSweep::Off,
            ConsolidateRuleFragments::Off,
            ConsolidateRetireComponent::Off,
            KnownKeyBinding::Off,
            OptOuts::stack_off(),
        )
        .await
        .unwrap();
        let memory = tmp.path().join("memory");
        let before = graded_counts(&memory).await;
        assert!(!before.is_empty());

        let scratch = tmp.path().join("again");
        let query = script.questions[0].text.as_str();
        let renders = recall_renders(&memory, &scratch, &[query, query]).await;
        assert!(renders[0].starts_with("Memory for"), "{}", renders[0]);
        assert_eq!(renders[0], renders[1], "a read moved the next one");
        assert_eq!(graded_counts(&memory).await, before);
        assert!(!scratch.exists());
    }

    #[test]
    fn recall_fields_follow_the_question_kind() {
        let script = super::super::script::parse_script(
            r#"
name = "port"

[[sessions]]
id = "s1"
date = "2026-01-01T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "Serve on 8080."

[[sessions.turns.claims]]
op = "create"
fact = "old"

[[sessions]]
id = "s2"
date = "2026-01-02T00:00:00Z"

[[sessions.turns]]
role = "user"
text = "Move the server to 8765."

[[sessions.turns.claims]]
op = "supersede"
fact = "old"
replacement = "new"

[[facts]]
id = "old"
subject = "app"
relation = "setting:port"
object = "8080"
subject_type = "project"
object_type = "concept"
end_state = "superseded"
superseded_in = "s2"

[[facts]]
id = "new"
subject = "app"
relation = "setting:port"
object = "8765"
subject_type = "project"
object_type = "concept"

[[expect]]
fact = "old"
live = false

[[expect]]
fact = "new"
live = true

[[superseded_pairs]]
old = "old"
new = "new"
retire_in = "s2"

[[questions]]
text = "which port"
answer = "8765"
kind = "current_value"
needs = ["new"]

[[questions]]
text = "which port exactly"
answer = "8765"
needs = ["new"]
"#,
        )
        .unwrap();
        let blank = QuestionScore {
            text: String::new(),
            keyword_hit: false,
            graph_hit: false,
            store_hit: false,
            recall_hit: false,
            recall_hit_24k: false,
            stale_in_budget: None,
            stale_in_budget_24k: None,
        };
        let mut scores = vec![blank.clone(), blank];
        let render = padded(
            "1. app [project]\n   moved from 8080",
            READ_BUDGET_CHARS + 50,
        );
        let render = format!("{render}\nnow 8765");
        fill_recall(&script, &mut scores, &[render.clone(), render]).unwrap();
        for score in &scores {
            assert!(!score.recall_hit && score.recall_hit_24k);
        }
        assert_eq!(scores[0].stale_in_budget, Some(true));
        assert_eq!(scores[0].stale_in_budget_24k, Some(true));
        assert_eq!(scores[1].stale_in_budget, None);
        assert_eq!(scores[1].stale_in_budget_24k, None);
    }

    #[test]
    fn the_store_channel_reads_live_decided_rows_only() {
        assert!(store_hit(&[row("8765", true, None, false)], "8765"));
        assert!(store_hit(
            &[row("8765", true, Some("decided"), false)],
            "8765"
        ));
        assert!(!store_hit(
            &[row("8765", true, Some("considered"), false)],
            "8765"
        ));
        assert!(!store_hit(
            &[row("8765", false, Some("rejected"), false)],
            "8765"
        ));
        assert!(!store_hit(&[row("8765", false, None, true)], "8765"));
        assert!(!store_hit(
            &[row("8765", false, Some("decided"), false)],
            "8765"
        ));
        assert!(!store_hit(&[row("18765", true, None, false)], "8765"));
        assert!(!store_hit(&[], "8765"));
    }
}
