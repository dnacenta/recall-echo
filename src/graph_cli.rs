// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Graph memory CLI subcommands (behind `graph` feature flag).

use std::path::{Path, PathBuf};

use crate::error::RecallError;
use crate::graph::correct::{CorrectTarget, Correction, CorrectionReport, EdgeCorrection, Removal};
use crate::graph::edge_view::EdgeView;
use crate::graph::traverse::format_traversal;
use crate::graph::types::*;
use crate::graph::{IngestContext, PipelineVersion, Provenance};
use crate::serve::{
    AddEntityArgs, CorrectArgs, IngestArchiveArgs, QueryArgs, RelateArgs, Request, SearchArgs,
    TraverseArgs,
};
use crate::serve_client;
use crate::theme::{ACCENT, BAD, BOLD, DIM, GOOD, RESET, WARN};

/// Initialize the graph store at {memory_dir}/graph/.
pub async fn init(memory_dir: &Path) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    serve_client::exclusive(memory_dir, |_graph| async { Ok(()) }).await?;
    println!(
        "{GOOD}✓{RESET} Graph store initialized at {}",
        graph_dir.display()
    );
    Ok(())
}

/// Re-run schema migrations against the store, taking it exclusively.
///
/// Normally a no-op with a line saying so — opening the store migrates it.
/// `--force` forgets which migrations have run and runs them all again, which
/// is the repair for a marker that claims work that did not land.
pub async fn migrate(memory_dir: &Path, force: bool) -> Result<(), RecallError> {
    let report = serve_client::exclusive(memory_dir, |graph| async move {
        Ok(graph.run_migrations(force).await?)
    })
    .await?;

    if report.ran() {
        println!("{GOOD}✓{RESET} {}", report.summary());
    } else {
        println!(
            "{GOOD}✓{RESET} Graph schema is already at v{}; nothing to migrate.",
            report.to_version
        );
        if !force {
            println!(
                "  {DIM}If episodes are still missing their extracted flag, \
                 re-run with --force.{RESET}"
            );
        }
    }
    Ok(())
}

/// Show graph stats.
pub async fn graph_status(memory_dir: &Path) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }
    let data = serve_client::execute(memory_dir, &Request::Status).await?;
    let stats: GraphStats = serde_json::from_value(data)?;

    println!("{BOLD}Graph Memory Status{RESET}");
    println!("  Entities:      {}", stats.entity_count);
    println!("  Relationships: {}", stats.relationship_count);
    println!("  Episodes:      {}", stats.episode_count);

    // Episodes arrive automatically on SessionEnd; turning them into entities
    // is an LLM-costing pass. The daemon now runs it once the machine is
    // quiet, but a store with episodes and no entities is still worth
    // explaining — and "the pass is pending" and "the pass can never run"
    // print identically without consulting the scan, so it decides which
    // explanation is true.
    // Printed on every status, not only the zero-entity one: an episode with
    // no extracted flag is invisible to the scan whether or not the store has
    // entities, and the zero-entity gate would make the only detector for an
    // unfinished migration unreachable the moment one entity exists.
    if stats.extracted_absent > 0 {
        print!("{}", unmigrated_episodes_warning(stats.extracted_absent));
    }

    if stats.episode_count > 0 && stats.entity_count == 0 {
        let extraction = crate::config::load_from_dir(memory_dir).extraction;
        print!(
            "{}",
            zero_entity_explanation(
                &stats,
                extraction.background_enabled,
                extraction.idle_after_secs
            )
        );
    }

    if !stats.entity_type_counts.is_empty() {
        println!("\n  {DIM}By type:{RESET}");
        let mut types: Vec<_> = stats.entity_type_counts.iter().collect();
        types.sort_by(|a, b| b.1.cmp(a.1));
        for (t, count) in types {
            println!("    {t}: {count}");
        }
    }

    print_daemon_line(memory_dir).await;
    Ok(())
}

/// Explain episodes that carry no `extracted` value.
///
/// The extraction scan matches `extracted = false`, so an episode without the
/// field is not slow to find — it cannot be found at all, and no amount of
/// extracting, ingesting or waiting will reach it. Schema version 2 gives
/// every episode a value on open, so a non-zero count means that migration
/// did not land: the marker claims it ran, which is exactly why re-opening
/// will not repair it and `graph migrate --force` will.
///
/// `graph ingest-all` is deliberately *not* offered here: it skips every
/// archive that already has an episode, which is precisely this set.
fn unmigrated_episodes_warning(count: u64) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "\n  {WARN}Unfinished schema migration.{RESET} {count} episode{} no extracted flag,",
        if count == 1 { " has" } else { "s have" }
    );
    let _ = writeln!(
        out,
        "  so the extraction scan cannot see {} at all. The version marker says the",
        if count == 1 { "it" } else { "them" }
    );
    let _ = writeln!(
        out,
        "  migration already ran, so re-opening the store will not repair this:"
    );
    let _ = writeln!(out, "    {DIM}recall-echo graph migrate --force{RESET}");
    out
}

/// Explain episodes with no `log_number`: real, and unreachable by
/// extraction, which finds its work by archive number.
fn unreachable_episodes_explanation(log_number_absent: u64) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "\n  {WARN}Inconsistent store.{RESET} Episodes exist but the extraction scan finds"
    );
    let _ = writeln!(
        out,
        "  nothing to process — waiting for the daemon will not help."
    );
    let _ = writeln!(
        out,
        "    episodes missing a log_number:       {log_number_absent}"
    );
    let _ = writeln!(
        out,
        "  Episodes without a log_number cannot be matched to an archive file,"
    );
    let _ = writeln!(
        out,
        "  so extraction cannot reach them. To rebuild episodes from archives:"
    );
    let _ = writeln!(out, "    {DIM}recall-echo graph ingest-all{RESET}");
    out
}

/// Explain a store with episodes and no entities.
///
/// Two very different states print the same two zeros. When the scan has
/// work, extraction simply hasn't run — say how much is pending and how to
/// run it. When the scan is *also* empty, no amount of waiting will ever
/// produce an entity: report the shape of the breakage (which field the
/// episodes are missing) instead of promising a daemon pass that will no-op.
///
/// The other way a store can be unscannable — episodes with no `extracted`
/// value at all — is not diagnosed here: it is reported by
/// [`unmigrated_episodes_warning`] on every status, zero entities or not,
/// because the indexed scan cannot see those episodes on any store.
fn zero_entity_explanation(
    stats: &crate::graph::types::GraphStats,
    background_enabled: bool,
    idle_after_secs: u64,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    if stats.unextracted_log_count > 0 {
        let _ = writeln!(
            out,
            "\n  {WARN}No entities yet.{RESET} Episodes are ingested automatically; turning"
        );
        let _ = writeln!(out, "  them into entities is a separate LLM pass.");
        let _ = writeln!(
            out,
            "  {} archive{} pending extraction.",
            stats.unextracted_log_count,
            if stats.unextracted_log_count == 1 {
                " is"
            } else {
                "s are"
            }
        );
        if background_enabled {
            let _ = writeln!(
                out,
                "  The daemon runs it after {idle_after_secs}s of quiet. To run it now:"
            );
        }
        let _ = writeln!(out, "    {DIM}recall-echo graph extract --all{RESET}");
        return out;
    }

    if stats.log_number_absent > 0 {
        out.push_str(&unreachable_episodes_explanation(stats.log_number_absent));
    } else {
        // Every episode has been through extraction and none yielded an
        // entity. Legitimate for short or trivial sessions, or after a gc
        // pass swept the graph — not a breakage, and not a bug report.
        let _ = writeln!(
            out,
            "\n  {WARN}Extraction is up to date but produced no entities.{RESET}"
        );
        let _ = writeln!(
            out,
            "  Every archive has been processed. That can be legitimate (short or"
        );
        let _ = writeln!(
            out,
            "  trivial sessions, or a gc pass); if it seems wrong, check the"
        );
        let _ = writeln!(
            out,
            "  extraction provider in .recall-echo.toml and any quarantined"
        );
        let _ = writeln!(out, "  archives in graph/extraction-quarantine.txt.");
    }
    out
}

/// Print the identity of the daemon serving this graph, or why there is none.
async fn print_daemon_line(memory_dir: &Path) {
    println!();
    match serve_client::daemon_info(memory_dir).await {
        Ok(Some(info)) => println!(
            "  {DIM}Daemon:{RESET} running — pid {}, v{}, up {}s",
            info.pid, info.version, info.uptime_secs
        ),
        Ok(None) if serve_client::graph_mode(memory_dir) == "server" => {
            println!("  {DIM}Daemon:{RESET} not used — [graph] mode = server");
        }
        Ok(None) => println!("  {DIM}Daemon:{RESET} not running"),
        Err(e) => println!("  {DIM}Daemon:{RESET} unknown ({e})"),
    }
}

/// Show daemon status without touching the graph.
pub async fn daemon_status(memory_dir: &Path) -> Result<(), RecallError> {
    println!("{BOLD}Graph Daemon{RESET}");
    println!(
        "  Socket: {}",
        serve_client::socket_path(memory_dir)?.display()
    );
    match serve_client::daemon_info(memory_dir).await? {
        Some(info) => {
            println!("  State:  {GOOD}running{RESET}");
            println!("  Pid:    {}", info.pid);
            println!("  Version: {}", info.version);
            println!("  Uptime: {}s", info.uptime_secs);
            print_extraction_lines(&info.extraction);
        }
        None if serve_client::graph_mode(memory_dir) == "server" => {
            println!("  State:  not used — [graph] mode = server");
        }
        None => println!("  State:  {WARN}not running{RESET}"),
    }
    println!(
        "  Log:    {}",
        serve_client::daemon_log_path(memory_dir).display()
    );
    Ok(())
}

/// Report what the daemon's background extraction worker has done.
fn print_extraction_lines(status: &crate::serve::ExtractionStatus) {
    if !status.enabled {
        let reason = status.disabled_reason.as_deref().unwrap_or("not running");
        println!("  Extraction: {WARN}off{RESET} — {DIM}{reason}{RESET}");
        return;
    }
    match status.last_run_secs_ago {
        Some(secs) => println!(
            "  Extraction: {GOOD}on{RESET} — {} archives in {} runs, last {}s ago ({}ms)",
            status.archives,
            status.runs,
            secs,
            status.last_run_ms.unwrap_or(0)
        ),
        None => println!("  Extraction: {GOOD}on{RESET} — nothing extracted yet"),
    }
    if let Some(error) = &status.last_error {
        println!("  {DIM}Last extraction error: {error}{RESET}");
    }
}

/// Stop the daemon serving this graph, if one is running.
pub async fn daemon_stop(memory_dir: &Path) -> Result<(), RecallError> {
    if serve_client::stop_daemon(memory_dir).await? {
        println!("{GOOD}✓{RESET} Graph daemon stopped");
    } else {
        println!("{WARN}No graph daemon running.{RESET}");
    }
    Ok(())
}

/// Add an entity to the graph.
pub async fn add_entity(
    memory_dir: &Path,
    name: &str,
    entity_type: &str,
    abstract_text: &str,
    overview: Option<&str>,
    source: Option<&str>,
) -> Result<(), RecallError> {
    let request = Request::AddEntity(AddEntityArgs {
        name: name.to_string(),
        entity_type: entity_type.to_string(),
        abstract_text: abstract_text.to_string(),
        overview: overview.map(String::from),
        source: source.map(String::from),
    });
    let entity: Entity =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    println!(
        "{GOOD}✓{RESET} Created entity: {BOLD}{}{RESET} ({}) [{}]",
        entity.name,
        entity.entity_type,
        entity.id_string()
    );
    Ok(())
}

/// Create a relationship between two entities.
pub async fn relate(
    memory_dir: &Path,
    from: &str,
    rel_type: &str,
    to: &str,
    description: Option<&str>,
    source: Option<&str>,
) -> Result<(), RecallError> {
    let request = Request::Relate(RelateArgs {
        from: from.to_string(),
        rel_type: rel_type.to_string(),
        to: to.to_string(),
        description: description.map(String::from),
        source: source.map(String::from),
    });
    let rel: Relationship =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    println!(
        "{GOOD}✓{RESET} {from} {ACCENT}—[{rel_type}]→{RESET} {to} [{}]",
        rel.id_string()
    );
    Ok(())
}

/// Semantic search across entities.
pub async fn search(
    memory_dir: &Path,
    query: &str,
    limit: usize,
    entity_type: Option<&str>,
    keyword: Option<&str>,
) -> Result<(), RecallError> {
    let request = Request::Search(SearchArgs {
        query: query.to_string(),
        limit,
        entity_type: entity_type.map(String::from),
        keyword: keyword.map(String::from),
    });
    let results: Vec<ScoredEntity> =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    if results.is_empty() {
        println!("{WARN}No results.{RESET}");
        return Ok(());
    }

    for (i, r) in results.iter().enumerate() {
        println!(
            "{BOLD}{}. {}{RESET} ({}) — score: {:.3}",
            i + 1,
            r.entity.name,
            r.entity.entity_type,
            r.score
        );
        println!("   {DIM}{}{RESET}", r.entity.abstract_text);
    }
    Ok(())
}

/// Ingest a single archive file into the graph.
///
/// Episodes are always written, with no model call. `pipeline` v2 then
/// extracts the session; v1 leaves extraction to `graph extract` and the
/// daemon, as before. `provenance` forces an authorship class on every
/// episode of the run — how `--external` marks genuinely external material.
/// `None` infers per chunk from conversation turn roles.
///
/// The pipeline choice consults a running daemon and does not open the store.
/// The episode write that follows is what opens it.
pub async fn ingest(
    memory_dir: &Path,
    archive_path: &Path,
    provenance: Option<Provenance>,
    pipeline: Option<PipelineVersion>,
) -> Result<(), RecallError> {
    let pipeline = crate::graph::v2::chosen_pipeline(memory_dir, pipeline, None).await?;
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let content = std::fs::read_to_string(archive_path)?;

    // Extract session_id and log_number from frontmatter if available
    let (session_id, log_number) = extract_archive_metadata(&content, archive_path);
    let v2_text = (pipeline == PipelineVersion::V2).then(|| content.clone());

    let request = Request::IngestArchive(IngestArchiveArgs {
        content,
        session_id: session_id.clone(),
        log_number,
        provenance,
    });
    let report: IngestionReport =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    println!(
        "{GOOD}✓{RESET} Ingested {}: {} episodes created {DIM}(provenance: {}){RESET}",
        archive_path.display(),
        report.episodes_created,
        provenance_label(provenance)
    );
    if !report.errors.is_empty() {
        for err in &report.errors {
            println!("  {WARN}warning:{RESET} {err}");
        }
    }
    if let Some(text) = v2_text {
        extract_ingested_v2(memory_dir, &text, &session_id, log_number).await?;
    }
    Ok(())
}

/// Run pipeline v2 on an archive whose episodes were just ingested.
#[cfg(feature = "llm")]
async fn extract_ingested_v2(
    memory_dir: &Path,
    text: &str,
    session_id: &str,
    log_number: Option<u32>,
) -> Result<(), RecallError> {
    let Some(log) = log_number else {
        return Err(RecallError::Config(
            "pipeline v2 needs a conversation-NNN archive so the session can be loaded".into(),
        ));
    };
    let (llm, _) = crate::llm_provider::create_provider(memory_dir, None, None)?;
    let memory = memory_dir.to_path_buf();
    let text = text.to_string();
    let session_id = session_id.to_string();
    let report = serve_client::exclusive(memory_dir, |gm| async move {
        let context = IngestContext::new(session_id, Some(log));
        let report = crate::graph::v2::extract_log(
            &gm,
            &memory,
            log,
            &text,
            &context,
            llm.as_ref(),
            PipelineVersion::V2,
        )
        .await?;
        if !report.leaves_pending() {
            gm.mark_extracted(log).await?;
        }
        Ok(report)
    })
    .await?;
    if report.leaves_pending() {
        let first = report
            .errors
            .first()
            .map(String::as_str)
            .unwrap_or("parse failure");
        return Err(RecallError::Other(format!(
            "pipeline v2 left log {log:03} pending: {first}"
        )));
    }
    println!(
        "  {GOOD}✓{RESET} pipeline v2: +{} entities, {} rels, {} slots{}",
        report.entities_created,
        report.relationships_created,
        report.slots_written,
        v2_report_suffix(&report)
    );
    for err in &report.errors {
        println!("  {WARN}warning:{RESET} {err}");
    }
    Ok(())
}

#[cfg(not(feature = "llm"))]
async fn extract_ingested_v2(
    _memory_dir: &Path,
    _text: &str,
    _session_id: &str,
    _log_number: Option<u32>,
) -> Result<(), RecallError> {
    Err(RecallError::Config(
        "pipeline v2 extracts with an LLM provider, which this build does not include".into(),
    ))
}

/// How an ingest run's provenance choice reads in its summary line.
fn provenance_label(provenance: Option<Provenance>) -> &'static str {
    match provenance {
        Some(Provenance::External) => "external",
        Some(Provenance::User) => "user",
        Some(Provenance::Assistant) => "assistant",
        Some(Provenance::SelfGenerated) => "self",
        None => "per turn role",
    }
}

/// Ingest all un-ingested archives in conversations/.
pub async fn ingest_all(
    memory_dir: &Path,
    provenance: Option<Provenance>,
) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let conversations_dir = find_conversations_dir(memory_dir)?;

    // Collect all conversation files, sorted
    let mut files: Vec<_> = std::fs::read_dir(&conversations_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("conversation-") || name.starts_with("archive-log-")
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    if files.is_empty() {
        println!("{WARN}No conversation archives found.{RESET}");
        return Ok(());
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        let mut total_episodes = 0u32;
        let mut ingested = 0u32;
        let mut skipped = 0u32;

        for entry in &files {
            let path = entry.path();
            let content = std::fs::read_to_string(&path)?;

            let (session_id, log_number) = extract_archive_metadata(&content, &path);

            // Check if already ingested (has episodes for this log_number)
            if let Some(ln) = log_number {
                if let Ok(Some(_)) = gm.get_episode_by_log_number(ln).await {
                    skipped += 1;
                    continue;
                }
            }

            let context = IngestContext::new(session_id, log_number).with_override(provenance);
            let report = gm.ingest_archive(&content, &context, None).await?;

            total_episodes += report.episodes_created;
            ingested += 1;

            println!(
                "  {GOOD}✓{RESET} {} — {} episodes",
                path.file_name().unwrap_or_default().to_string_lossy(),
                report.episodes_created
            );
        }

        println!(
            "\n{GOOD}✓{RESET} Ingested {ingested} archives ({total_episodes} episodes), skipped {skipped} already ingested {DIM}(provenance: {}){RESET}",
            provenance_label(provenance)
        );
        Ok(())
    })
    .await
}

/// Extract session_id and log_number from a conversation archive's frontmatter.
pub(crate) fn extract_archive_metadata(content: &str, path: &Path) -> (String, Option<u32>) {
    let mut session_id = "unknown".to_string();
    let mut log_number: Option<u32> = None;

    // Try to extract log number from filename
    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
        let num_str = name
            .strip_prefix("conversation-")
            .or_else(|| name.strip_prefix("archive-log-"));
        if let Some(num_str) = num_str {
            if let Ok(n) = num_str.parse::<u32>() {
                log_number = Some(n);
            }
        }
    }

    // Try to extract session_id from frontmatter
    if let Some(stripped) = content.strip_prefix("---") {
        if let Some(end) = stripped.find("---") {
            let frontmatter = &stripped[..end];
            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("session_id:") {
                    session_id = val.trim().trim_matches('"').to_string();
                }
            }
        }
    }

    (session_id, log_number)
}

/// Traverse the graph from an entity.
pub async fn traverse(
    memory_dir: &Path,
    entity_name: &str,
    depth: u32,
    type_filter: Option<&str>,
) -> Result<(), RecallError> {
    let request = Request::Traverse(TraverseArgs {
        entity: entity_name.to_string(),
        depth,
        type_filter: type_filter.map(String::from),
    });
    let tree: TraversalNode =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    let output = format_traversal(&tree, 0);
    print!("{output}");
    Ok(())
}

/// Hybrid query: semantic + graph expansion + optional episodes.
pub async fn hybrid_query(
    memory_dir: &Path,
    query: &str,
    limit: usize,
    entity_type: Option<&str>,
    keyword: Option<&str>,
    depth: u32,
    episodes: bool,
) -> Result<(), RecallError> {
    let request = Request::Query(QueryArgs {
        query: query.to_string(),
        limit,
        entity_type: entity_type.map(String::from),
        keyword: keyword.map(String::from),
        depth,
        episodes,
    });
    let result: QueryResult =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    if result.entities.is_empty() && result.episodes.is_empty() {
        println!("{WARN}No results.{RESET}");
        return Ok(());
    }

    if !result.entities.is_empty() {
        println!("{BOLD}Entities:{RESET}");
        for (i, r) in result.entities.iter().enumerate() {
            let source_tag = match &r.source {
                MatchSource::Semantic => "semantic".to_string(),
                MatchSource::Graph { parent, rel_type } => {
                    format!("graph: {parent} —[{rel_type}]")
                }
                MatchSource::Keyword => "keyword".to_string(),
            };
            println!(
                "  {BOLD}{}. {}{RESET} ({}) — {:.3} [{DIM}{source_tag}{RESET}]",
                i + 1,
                r.entity.name,
                r.entity.entity_type,
                r.score
            );
            println!("     {DIM}{}{RESET}", r.entity.abstract_text);
        }
    }

    if !result.episodes.is_empty() {
        println!("\n{BOLD}Episodes:{RESET}");
        for (i, ep) in result.episodes.iter().enumerate() {
            let log = ep
                .episode
                .log_number
                .map(|n| format!("#{n}"))
                .unwrap_or_default();
            println!(
                "  {BOLD}{}. {}{RESET} ({}) — {:.3}",
                i + 1,
                ep.episode.session_id,
                log,
                ep.score
            );
            println!("     {DIM}{}{RESET}", ep.episode.abstract_text);
        }
    }

    Ok(())
}

/// Accumulated extraction totals across multiple archives.
#[cfg(feature = "llm")]
#[derive(Default)]
struct ExtractionTotals {
    entities_created: u32,
    entities_merged: u32,
    entities_skipped: u32,
    relationships: u32,
    errors: Vec<String>,
    processed: u32,
    estimated_tokens: u64,
    measured_tokens: u64,
    quarantined: Vec<u32>,
    /// Archives whose every chunk failed. Not marked extracted, not retried
    /// here: the user is watching and will re-run.
    left_pending: Vec<u32>,
    dedup_llm_calls: u32,
    dedup_fast_path: u32,
}

/// Archives that fail outright in a row before `graph extract` stops. Mirrors
/// the daemon's rule: three is a broken provider, not three unlucky archives,
/// and every further attempt is N more spawns for nothing.
#[cfg(feature = "llm")]
const MAX_CONSECUTIVE_TOTAL_FAILURES: u32 = 3;

/// Print a dry-run listing of archives that would be extracted.
#[cfg(feature = "llm")]
fn print_extract_dry_run(conversations_dir: &Path, log_numbers: &[u32]) {
    println!(
        "{BOLD}Dry run — {}{RESET} archives to extract",
        log_numbers.len()
    );
    for ln in log_numbers {
        let path = find_archive_file(conversations_dir, *ln);
        let label = match &path {
            Ok(p) => p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            Err(_) => format!("log {ln:03} (file not found)"),
        };
        println!("  {label}");
    }
}

/// Print the final extraction summary.
#[cfg(feature = "llm")]
fn print_extract_summary(totals: &ExtractionTotals) {
    // A run that extracted nothing and left archives behind is not a green
    // tick — that is the shape of failure this summary exists to expose.
    let nothing_done = totals.processed == 0
        && (!totals.left_pending.is_empty() || !totals.quarantined.is_empty());
    let mark = if nothing_done {
        format!("{WARN}✗{RESET}")
    } else {
        format!("{GOOD}✓{RESET}")
    };
    println!(
        "\n{mark} Done: {} archives — +{} created, ~{} merged, -{} skipped, {} relationships",
        totals.processed,
        totals.entities_created,
        totals.entities_merged,
        totals.entities_skipped,
        totals.relationships,
    );
    println!(
        "  Tokens: {}",
        format_token_bill(totals.measured_tokens, totals.estimated_tokens)
    );
    let dedup_total = totals.dedup_llm_calls + totals.dedup_fast_path;
    if dedup_total > 0 {
        println!(
            "  Dedup: {} of {} candidates needed a model call ({} resolved locally)",
            totals.dedup_llm_calls, dedup_total, totals.dedup_fast_path
        );
    }

    if !totals.quarantined.is_empty() {
        println!(
            "  {WARN}Quarantined: {} archives{RESET}",
            totals.quarantined.len()
        );
    }
    if !totals.left_pending.is_empty() {
        println!(
            "  {WARN}Left pending (every chunk failed): {} archives{RESET} — fix the provider and re-run",
            totals.left_pending.len()
        );
    }

    if !totals.errors.is_empty() {
        println!("\n{WARN}Warnings ({}):{RESET}", totals.errors.len());
        for err in totals.errors.iter().take(10) {
            println!("  {DIM}{err}{RESET}");
        }
        if totals.errors.len() > 10 {
            println!("  {DIM}... and {} more{RESET}", totals.errors.len() - 10);
        }
    }
}

/// State a token bill without overstating it.
///
/// A provider that reports usage (codex, grok, the HTTP APIs) is quoted; one
/// that does not (claude-code's prose output, any CLI without usage paths) is
/// estimated, and the `~` says so. A run that did some of each prints both
/// rather than adding a measurement to a guess and calling the sum measured.
#[cfg(feature = "llm")]
fn format_token_bill(measured: u64, estimated: u64) -> String {
    match (measured, estimated) {
        (0, 0) => "0".to_string(),
        (measured, 0) => format!("{} measured", format_tokens(measured)),
        (0, estimated) => format!("~{} estimated", format_tokens(estimated)),
        (measured, estimated) => format!(
            "{} measured + ~{} estimated",
            format_tokens(measured),
            format_tokens(estimated)
        ),
    }
}

/// Format token count human-readable (e.g., "1.2M", "350K").
#[cfg(feature = "llm")]
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(feature = "llm")]
const V2_BUILD_DIR: &str = "graph.v2-build";

/// Stops the graph daemon the way `graph daemon stop` does.
///
/// The CLI passes that stop. A caller that must not touch a running daemon
/// passes its own implementation.
#[cfg(feature = "llm")]
pub trait RebuildStop: Send + Sync {
    /// `Ok(true)` when a daemon was stopped, `Ok(false)` when none was
    /// running, `Err` when it could not be stopped.
    fn stop<'a>(
        &'a self,
        memory_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, RecallError>> + Send + 'a>>;
}

/// The stopper `graph daemon stop` uses.
#[cfg(feature = "llm")]
struct LiveDaemonStop;

#[cfg(feature = "llm")]
impl RebuildStop for LiveDaemonStop {
    fn stop<'a>(
        &'a self,
        memory_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, RecallError>> + Send + 'a>>
    {
        Box::pin(serve_client::stop_daemon_for_admin(memory_dir))
    }
}

#[cfg(feature = "llm")]
struct RebuildPlan {
    conversations: PathBuf,
    pending: Vec<u32>,
    resume: bool,
}

#[cfg(feature = "llm")]
struct BuildTotals {
    finished: bool,
    facts: u32,
    archives: u32,
}

/// One archive after its episodes were brought up to the chunk count.
#[cfg(feature = "llm")]
enum BuiltArchive {
    /// Extraction ran.
    Done(Box<IngestionReport>),
    /// `chunk_conversation` produced nothing. There is no episode to mark
    /// and no provider call; a later resume must not try again.
    NoChunks,
}

/// Episode rows for one log, grouped once from the scan.
#[cfg(feature = "llm")]
struct EpisodeRows {
    count: usize,
    all_extracted: bool,
}

/// Re-extract every archive into `graph.v2-build/` and swap it over `graph/`.
///
/// `stop` is the same operation as `graph daemon stop`. A caller that must
/// not touch a running daemon passes its own stopper. `llm` overrides
/// provider construction; `None` reads the memory directory config.
///
/// Server mode, a pipeline other than v2, a config that cannot be saved, and
/// a build directory that is not already stamped v2 are refused before the
/// daemon is stopped and before the old store is touched. A stamped build
/// directory is a resume of its unfinished logs. `--dry-run` lists archives
/// and estimated calls and does not stop the daemon.
///
/// The build, the catch-up of logs that appear while it runs, the swap, and
/// the config write all hold the admin lock. `stop` runs under that lock;
/// the CLI's stop is `stop_daemon_for_admin`.
#[cfg(feature = "llm")]
pub async fn rebuild(
    memory_dir: &Path,
    options: ExtractOptions,
    stop: &impl RebuildStop,
    llm: Option<&dyn crate::graph::llm::LlmProvider>,
) -> Result<(), RecallError> {
    let plan = prepare_rebuild(memory_dir, &options).await?;
    if options.dry_run {
        return print_rebuild_dry_run(memory_dir, &plan);
    }
    serve_client::exclusive_without_store(
        memory_dir,
        || stop_for_rebuild(memory_dir, stop),
        || finish_rebuild(memory_dir, &plan, &options, llm),
    )
    .await
}

#[cfg(feature = "llm")]
async fn finish_rebuild(
    memory_dir: &Path,
    plan: &RebuildPlan,
    options: &ExtractOptions,
    llm: Option<&dyn crate::graph::llm::LlmProvider>,
) -> Result<(), RecallError> {
    let totals = if let Some(provider) = llm {
        drive_rebuild(memory_dir, plan, provider, options).await?
    } else {
        let (owned, _) = crate::llm_provider::create_provider(
            memory_dir,
            options.provider.as_deref(),
            options.model.as_deref(),
        )?;
        drive_rebuild(memory_dir, plan, owned.as_ref(), options).await?
    };
    if !totals.finished {
        return Ok(());
    }
    swap_and_record(memory_dir, totals.archives, totals.facts)
}

#[cfg(feature = "llm")]
async fn prepare_rebuild(
    memory_dir: &Path,
    options: &ExtractOptions,
) -> Result<RebuildPlan, RecallError> {
    // Flag and config only. A stamp read would open the store, and a refused
    // rebuild must leave `graph/` byte-identical.
    let configured = crate::config::load_from_dir(memory_dir)
        .graph
        .and_then(|section| section.pipeline);
    let pipeline = PipelineVersion::resolve(options.pipeline, configured.as_deref())
        .map_err(RecallError::Config)?;
    if pipeline != PipelineVersion::V2 {
        return Err(RecallError::Config(
            "rebuild requires pipeline v2; pass --pipeline v2".into(),
        ));
    }
    // Same parse-and-validate `set_graph_pipeline` uses, before the daemon
    // stops or any directory is opened. `mode` is read from that parse:
    // `load` drops a file it cannot validate, which would hide `mode = "server"`.
    if let Some(parsed) = crate::config::read_config_file(memory_dir)? {
        let mode = parsed
            .graph
            .as_ref()
            .map(|graph| graph.mode.clone())
            .unwrap_or_else(|| "embedded".to_string());
        let mut problems = Vec::new();
        if mode == "server" {
            problems.push(
                "rebuild refuses [graph] mode = \"server\": there is no embedded directory to swap, so nothing was changed"
                    .to_string(),
            );
        }
        if let Err(err) = crate::config::validate_for_rewrite(memory_dir, parsed) {
            problems.push(err.to_string());
        }
        if !problems.is_empty() {
            return Err(RecallError::Config(problems.join("; ")));
        }
    }
    let conversations = find_conversations_dir(memory_dir)?;
    let logs = conversation_logs(&conversations)?;
    if logs.is_empty() {
        return Err(RecallError::Other(
            "no conversation archives found to rebuild".into(),
        ));
    }
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.is_dir() {
        if let Some(message) = half_swap_refusal(memory_dir, &conversations, &logs).await? {
            return Err(RecallError::Other(message));
        }
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }
    let (resume, pending) =
        pending_in_build(&memory_dir.join(V2_BUILD_DIR), &conversations, &logs).await?;
    Ok(RebuildPlan {
        conversations,
        pending,
        resume,
    })
}

/// Crash between the two renames: `graph/` is gone, a backup holds it, and
/// the build is a finished v2 store. Refuse, with the commands to finish or
/// roll back. Do not rename anything.
#[cfg(feature = "llm")]
async fn half_swap_refusal(
    memory_dir: &Path,
    conversations: &Path,
    logs: &[u32],
) -> Result<Option<String>, RecallError> {
    let Some(backup) = newest_backup(memory_dir)? else {
        return Ok(None);
    };
    let build = memory_dir.join(V2_BUILD_DIR);
    if !build_is_complete(&build, conversations, logs).await? {
        return Ok(None);
    }
    Ok(Some(half_swap_message(
        &memory_dir.join("graph"),
        &build,
        &backup,
    )))
}

#[cfg(feature = "llm")]
fn newest_backup(memory_dir: &Path) -> Result<Option<PathBuf>, RecallError> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(memory_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("graph.v1-backup-") {
            continue;
        }
        let modified = path.metadata().and_then(|meta| meta.modified()).ok();
        found.push((modified, path));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(found.pop().map(|(_, path)| path))
}

/// A v2-stamped build whose every current archive is fully stored.
#[cfg(feature = "llm")]
async fn build_is_complete(
    build: &Path,
    conversations: &Path,
    logs: &[u32],
) -> Result<bool, RecallError> {
    if !crate::graph::store::has_kv_manifest(build) {
        return Ok(false);
    }
    let db = crate::graph::store::open(build).await?;
    let stamp = crate::graph::v2::read_stamp(&db).await.unwrap_or(None);
    if stamp.as_deref() != Some("v2") {
        return Ok(false);
    }
    let pending = pending_logs(&db, conversations, logs).await?;
    Ok(pending.is_empty())
}

#[cfg(feature = "llm")]
fn half_swap_message(graph: &Path, build: &Path, backup: &Path) -> String {
    let graph = shell_quote(graph);
    let build = shell_quote(build);
    let backup = shell_quote(backup);
    format!(
        "rebuild stopped between the two renames, so nothing was finished automatically. \
         {graph} is missing, {backup} holds the previous store, and {build} is a complete v2 build.\n\
         To finish the swap:\n  \
         mv {build} {graph}\n  \
         [graph]\n  \
         pipeline = \"v2\"\n\
         To roll back:\n  \
         mv {backup} {graph}\n  \
         [graph]\n  \
         pipeline = \"v1\""
    )
}

#[cfg(feature = "llm")]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(feature = "llm")]
async fn pending_in_build(
    build: &Path,
    conversations: &Path,
    logs: &[u32],
) -> Result<(bool, Vec<u32>), RecallError> {
    if !build.exists() {
        return Ok((false, logs_with_chunks(conversations, logs)?));
    }
    if !build.join("surreal").is_dir() || !crate::graph::store::has_kv_manifest(build) {
        return Err(RecallError::Other(removal_hint(build)));
    }
    let db = crate::graph::store::open(build).await?;
    let stamp = match crate::graph::v2::read_stamp(&db).await {
        Ok(stamp) => stamp,
        Err(err) => {
            return Err(RecallError::Other(format!(
                "{}; {err}",
                removal_hint(build)
            )));
        }
    };
    if stamp.as_deref() != Some("v2") {
        return Err(RecallError::Other(removal_hint(build)));
    }
    let pending = pending_logs(&db, conversations, logs).await?;
    Ok((true, pending))
}

/// Logs that have at least one chunk. A zero-chunk log is not pending work.
#[cfg(feature = "llm")]
fn logs_with_chunks(conversations: &Path, logs: &[u32]) -> Result<Vec<u32>, RecallError> {
    let mut pending = Vec::new();
    for log in logs {
        if expected_chunks(conversations, *log)? > 0 {
            pending.push(*log);
        }
    }
    Ok(pending)
}

#[cfg(feature = "llm")]
async fn pending_logs(
    db: &surrealdb::Surreal<crate::graph::store::Db>,
    conversations: &Path,
    logs: &[u32],
) -> Result<Vec<u32>, RecallError> {
    let mut response = db
        .query("SELECT log_number, extracted FROM episode")
        .await
        .map_err(crate::graph::error::GraphError::from)?;
    let rows: Vec<serde_json::Value> = crate::graph::deserialize_take(&mut response, 0)?;
    let grouped = group_episodes(&rows);
    let mut pending = Vec::new();
    for log in logs {
        let expected = expected_chunks(conversations, *log)?;
        if !log_is_complete(grouped.get(log), expected) {
            pending.push(*log);
        }
    }
    Ok(pending)
}

#[cfg(feature = "llm")]
fn group_episodes(rows: &[serde_json::Value]) -> std::collections::HashMap<u32, EpisodeRows> {
    let mut grouped = std::collections::HashMap::new();
    for row in rows {
        let Some(log) = json_u32(row.get("log_number")) else {
            continue;
        };
        let entry = grouped.entry(log).or_insert(EpisodeRows {
            count: 0,
            all_extracted: true,
        });
        entry.count += 1;
        if row.get("extracted").and_then(serde_json::Value::as_bool) != Some(true) {
            entry.all_extracted = false;
        }
    }
    grouped
}

/// `expected == 0` is complete: there is nothing to extract, and a resume
/// must not call the provider for that log again.
#[cfg(feature = "llm")]
fn log_is_complete(rows: Option<&EpisodeRows>, expected: usize) -> bool {
    if expected == 0 {
        return true;
    }
    match rows {
        Some(rows) => rows.count >= expected && rows.all_extracted,
        None => false,
    }
}

#[cfg(feature = "llm")]
fn expected_chunks(conversations: &Path, log: u32) -> Result<usize, RecallError> {
    let archive = find_archive_file(conversations, log)?;
    let content = std::fs::read_to_string(&archive)?;
    Ok(crate::graph::extract::chunk_conversation(
        &content,
        crate::graph::ingest::INGEST_CHUNK_TOKENS,
    )
    .len())
}

#[cfg(feature = "llm")]
fn json_u32(value: Option<&serde_json::Value>) -> Option<u32> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .and_then(|n| u32::try_from(n).ok())
}

#[cfg(feature = "llm")]
fn removal_hint(path: &Path) -> String {
    format!(
        "{display} exists and is not a v2 build store to resume (resume continues only when that store is stamped v2). Remove it and re-run, for example: rm -r {display}",
        display = path.display()
    )
}

#[cfg(feature = "llm")]
fn conversation_logs(conversations: &Path) -> Result<Vec<u32>, RecallError> {
    let mut logs = Vec::new();
    for entry in std::fs::read_dir(conversations)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(log) = log_from_archive_name(name) {
            logs.push(log);
        }
    }
    logs.sort_unstable();
    logs.dedup();
    Ok(logs)
}

#[cfg(feature = "llm")]
fn log_from_archive_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".md")?;
    let rest = stem
        .strip_prefix("conversation-")
        .or_else(|| stem.strip_prefix("archive-log-"))?;
    rest.parse().ok()
}

#[cfg(feature = "llm")]
fn print_rebuild_dry_run(memory_dir: &Path, plan: &RebuildPlan) -> Result<(), RecallError> {
    if plan.resume {
        println!(
            "Resuming rebuild from {} (stamped v2); continuing from unextracted logs.",
            memory_dir.join(V2_BUILD_DIR).display()
        );
    }
    println!("{BOLD}Dry run — {}{RESET} archives", plan.pending.len());
    let mut total_calls = 0u32;
    for log in &plan.pending {
        let estimate = crate::graph::v2::estimate_session(memory_dir, *log)?;
        let sidecar = if estimate.has_sidecar { "yes" } else { "no" };
        println!(
            "  {}  sidecar: {sidecar}  windows: {}  calls: {}",
            archive_label(&plan.conversations, *log),
            estimate.windows,
            estimate.calls
        );
        total_calls = total_calls.saturating_add(estimate.calls);
    }
    println!(
        "Estimated provider calls: {total_calls} (windows × 1 + resolver bound {})",
        crate::graph::v2::RESOLVER_CALL_BOUND
    );
    Ok(())
}

#[cfg(feature = "llm")]
fn archive_label(conversations: &Path, log: u32) -> String {
    find_archive_file(conversations, log)
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| format!("log {log:03}"))
}

#[cfg(feature = "llm")]
async fn stop_for_rebuild(memory_dir: &Path, stop: &impl RebuildStop) -> Result<(), RecallError> {
    match stop.stop(memory_dir).await {
        Ok(true) => {
            println!("{GOOD}✓{RESET} Graph daemon stopped");
            Ok(())
        }
        Ok(false) => {
            println!("{WARN}No graph daemon running.{RESET}");
            Ok(())
        }
        Err(err) => Err(RecallError::Daemon(format!(
            "cannot stop the graph daemon: {err}"
        ))),
    }
}

#[cfg(feature = "llm")]
async fn drive_rebuild(
    memory_dir: &Path,
    plan: &RebuildPlan,
    llm: &dyn crate::graph::llm::LlmProvider,
    options: &ExtractOptions,
) -> Result<BuildTotals, RecallError> {
    if plan.resume {
        println!(
            "Resuming rebuild from {} (stamped v2); continuing from unextracted logs.",
            memory_dir.join(V2_BUILD_DIR).display()
        );
    }
    let build = memory_dir.join(V2_BUILD_DIR);
    let gm = match crate::graph::GraphMemory::open_embedded(&build).await {
        Ok(gm) => gm,
        Err(err) => {
            return Err(stopped_opening(
                memory_dir,
                &format!("could not open the build store: {err}"),
            ));
        }
    };
    if let Err(err) = crate::graph::v2::ensure_v2_store(&gm).await {
        return Err(stopped_opening(memory_dir, &err.to_string()));
    }
    let mut measured = 0u64;
    let mut estimated = 0u64;
    let mut facts = 0u32;
    let mut batch = plan.pending.clone();
    let mut processed = false;
    loop {
        if batch.is_empty() {
            batch = pending_logs(
                gm.db(),
                &plan.conversations,
                &conversation_logs(&plan.conversations)?,
            )
            .await?;
            if batch.is_empty() {
                break;
            }
            if options.delay_ms > 0 && processed {
                tokio::time::sleep(std::time::Duration::from_millis(options.delay_ms)).await;
            }
        }
        let total = batch.len();
        for (index, log) in batch.iter().copied().enumerate() {
            let spent = measured.saturating_add(estimated);
            if options.max_tokens > 0 && spent >= options.max_tokens {
                println!(
                    "\n{WARN}⚠ Token budget exhausted ({} of {}). Stopping.{RESET}",
                    format_token_bill(measured, estimated),
                    format_tokens(options.max_tokens),
                );
                println!(
                    "  Partial build left at {}. {} was not modified. Re-run to resume.",
                    build.display(),
                    memory_dir.join("graph").display()
                );
                return Ok(BuildTotals {
                    finished: false,
                    facts,
                    archives: 0,
                });
            }
            let built =
                match rebuild_with_retry(&gm, memory_dir, &plan.conversations, log, llm).await {
                    Ok(built) => built,
                    Err(err) => return Err(stopped(memory_dir, log, &err.to_string())),
                };
            if let BuiltArchive::Done(report) = built {
                measured = measured.saturating_add(report.measured_tokens);
                estimated = estimated.saturating_add(report.estimated_tokens);
                if report.leaves_pending() {
                    let detail = report
                        .errors
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "extraction left the archive pending".to_string());
                    return Err(stopped(memory_dir, log, &detail));
                }
                gm.mark_extracted(log)
                    .await
                    .map_err(|err| stopped(memory_dir, log, &err.to_string()))?;
                let written = report
                    .slots_written
                    .saturating_add(report.relationships_created);
                facts = facts.saturating_add(written);
                println!(
                    "  {GOOD}✓{RESET} [{}/{}] log {log:03}: {written} facts{}",
                    index + 1,
                    total,
                    v2_report_suffix(&report),
                );
            }
            processed = true;
            if options.delay_ms > 0 && index + 1 != total {
                tokio::time::sleep(std::time::Duration::from_millis(options.delay_ms)).await;
            }
        }
        batch.clear();
    }
    let archives = conversation_logs(&plan.conversations)?;
    Ok(BuildTotals {
        finished: true,
        facts,
        archives: u32::try_from(archives.len()).unwrap_or(u32::MAX),
    })
}

/// One attempt, and on a non-stamp failure exactly one more. The second
/// failure stops the rebuild; it is not quarantined.
#[cfg(feature = "llm")]
async fn rebuild_with_retry(
    gm: &crate::graph::GraphMemory,
    memory_dir: &Path,
    conversations: &Path,
    log: u32,
    llm: &dyn crate::graph::llm::LlmProvider,
) -> Result<BuiltArchive, RecallError> {
    let first = rebuild_archive(gm, memory_dir, conversations, log, llm).await;
    if let Err(err) = &first {
        if err.to_string().contains(crate::graph::v2::V2_REFUSAL) {
            return first;
        }
        println!("  {WARN}⚠{RESET} log {log:03}: failed, retrying... ({err})");
    }
    let retry = first.is_err();
    retry_once(first, retry, || {
        rebuild_archive(gm, memory_dir, conversations, log, llm)
    })
    .await
}

#[cfg(feature = "llm")]
async fn rebuild_archive(
    gm: &crate::graph::GraphMemory,
    memory_dir: &Path,
    conversations: &Path,
    log: u32,
    llm: &dyn crate::graph::llm::LlmProvider,
) -> Result<BuiltArchive, RecallError> {
    let archive = find_archive_file(conversations, log)?;
    let content = std::fs::read_to_string(&archive)?;
    let chunks = crate::graph::extract::chunk_conversation(
        &content,
        crate::graph::ingest::INGEST_CHUNK_TOKENS,
    );
    if chunks.is_empty() {
        return Ok(BuiltArchive::NoChunks);
    }
    let (session_id, _) = extract_archive_metadata(&content, &archive);
    let context = IngestContext::new(session_id, Some(log));
    ensure_episodes(gm, log, &content, &chunks, &context).await?;
    let report = crate::graph::v2::extract_log(
        gm,
        memory_dir,
        log,
        &content,
        &context,
        llm,
        PipelineVersion::V2,
    )
    .await?;
    Ok(BuiltArchive::Done(Box::new(report)))
}

/// Bring the log's episode count up to `chunks.len()`. A short prefix is
/// extended; a full set is left alone. Ingest errors stop the archive.
#[cfg(feature = "llm")]
async fn ensure_episodes(
    gm: &crate::graph::GraphMemory,
    log: u32,
    content: &str,
    chunks: &[String],
    context: &IngestContext,
) -> Result<(), RecallError> {
    let have = episode_count(gm, log).await?;
    if have >= chunks.len() {
        return Ok(());
    }
    let report = if have == 0 {
        gm.ingest_archive(content, context, None).await?
    } else {
        crate::graph::ingest::ingest_remaining_episodes(gm, &chunks[have..], context, have).await?
    };
    if report.errors.is_empty() {
        Ok(())
    } else {
        Err(RecallError::Other(report.errors.join("; ")))
    }
}

#[cfg(feature = "llm")]
async fn episode_count(gm: &crate::graph::GraphMemory, log: u32) -> Result<usize, RecallError> {
    let mut response = gm
        .db()
        .query("SELECT count() AS count FROM episode WHERE log_number = $ln GROUP ALL")
        .bind(("ln", i64::from(log)))
        .await
        .map_err(crate::graph::error::GraphError::from)?;
    let rows: Vec<serde_json::Value> = crate::graph::deserialize_take(&mut response, 0)?;
    Ok(rows
        .first()
        .and_then(|row| json_u32(row.get("count")))
        .unwrap_or(0) as usize)
}

#[cfg(feature = "llm")]
fn stopped(memory_dir: &Path, log: u32, detail: &str) -> RecallError {
    unfinished(memory_dir, Some(log), detail)
}

/// The build store could not be opened. No archive has run, so there is no
/// log number; the sentence is the one [`stopped`] uses.
#[cfg(feature = "llm")]
fn stopped_opening(memory_dir: &Path, detail: &str) -> RecallError {
    unfinished(memory_dir, None, detail)
}

#[cfg(feature = "llm")]
fn unfinished(memory_dir: &Path, log: Option<u32>, detail: &str) -> RecallError {
    let place = match log {
        Some(log) => format!("on log {log:03}: "),
        None => String::new(),
    };
    RecallError::Other(format!(
        "rebuild stopped {place}{detail}; {} was not modified; partial build is at {}",
        memory_dir.join("graph").display(),
        memory_dir.join(V2_BUILD_DIR).display()
    ))
}

#[cfg(feature = "llm")]
fn swap_and_record(memory_dir: &Path, archives: u32, facts: u32) -> Result<(), RecallError> {
    let backup = swap_store(memory_dir)?;
    if let Err(err) = crate::config::set_graph_pipeline(memory_dir, "v2") {
        let config = crate::config::config_path(memory_dir);
        return Err(RecallError::Config(format!(
            "swapped the v2 store into place ({} holds the previous store) but did not set [graph] pipeline: {err}\n\
             Recovery: set [graph] pipeline = \"v2\" in {}",
            shell_quote(&backup),
            shell_quote(&config)
        )));
    }
    print_swapped(memory_dir, &backup, archives, facts);
    Ok(())
}

#[cfg(feature = "llm")]
fn swap_store(memory_dir: &Path) -> Result<PathBuf, RecallError> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M").to_string();
    let backup = unique_backup(memory_dir, &stamp);
    rename_swapping(
        &memory_dir.join("graph"),
        &memory_dir.join(V2_BUILD_DIR),
        &backup,
    )?;
    Ok(backup)
}

#[cfg(feature = "llm")]
fn unique_backup(memory_dir: &Path, stamp: &str) -> PathBuf {
    let base = format!("graph.v1-backup-{stamp}");
    let mut candidate = memory_dir.join(&base);
    let mut n = 2u32;
    while candidate.exists() {
        candidate = memory_dir.join(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

#[cfg(feature = "llm")]
fn rename_swapping(graph: &Path, build: &Path, backup: &Path) -> Result<(), RecallError> {
    if let Err(err) = std::fs::rename(graph, backup) {
        return Err(io_msg(format!(
            "could not move {} to {}: {err}; {} was left in place",
            graph.display(),
            backup.display(),
            graph.display()
        )));
    }
    if let Err(err) = std::fs::rename(build, graph) {
        return match std::fs::rename(backup, graph) {
            Ok(()) => Err(io_msg(format!(
                "could not move {} to {} ({err}); moved {} back so the original store is in place and the build is still at {}",
                build.display(),
                graph.display(),
                backup.display(),
                build.display()
            ))),
            Err(rollback) => Err(io_msg(format!(
                "could not move {} to {} ({err}) and could not move {} back ({rollback}); the original store is at {} and the build is at {}",
                build.display(),
                graph.display(),
                backup.display(),
                backup.display(),
                build.display()
            ))),
        };
    }
    Ok(())
}

#[cfg(feature = "llm")]
fn io_msg(message: String) -> RecallError {
    RecallError::Io(std::io::Error::other(message))
}

#[cfg(feature = "llm")]
fn print_swapped(memory_dir: &Path, backup: &Path, archives: u32, facts: u32) {
    print!("{}", swap_report(memory_dir, backup, archives, facts));
}

#[cfg(feature = "llm")]
fn swap_report(memory_dir: &Path, backup: &Path, archives: u32, facts: u32) -> String {
    let graph = memory_dir.join("graph");
    let build = memory_dir.join(V2_BUILD_DIR);
    format!(
        "\n{GOOD}✓{RESET} Rebuilt {archives} archives, {facts} facts written.\n\
         A symlinked graph/ becomes a real directory in memory/.\n\
         Rollback (stop the daemon first):\n  \
         mv {graph} {build}\n  \
         mv {backup} {graph}\n\
         After a rollback, set [graph] pipeline = \"v1\".\n",
        graph = shell_quote(&graph),
        build = shell_quote(&build),
        backup = shell_quote(backup),
    )
}

/// Call `again` once when `first` failed and `retry` is set.
///
/// Extract quarantines the second failure. Rebuild stops. Neither path
/// makes a third call.
#[cfg(feature = "llm")]
async fn retry_once<T, E, F, Fut>(first: Result<T, E>, retry: bool, again: F) -> Result<T, E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    match first {
        Ok(value) => Ok(value),
        Err(err) if !retry => Err(err),
        Err(_) => again().await,
    }
}

/// What `graph extract` does with an `Err` from one archive.
#[cfg(feature = "llm")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// Stamp refusal. Stop the run; do not retry or quarantine.
    Stop,
    /// v2 write or load error. One attempt, left pending.
    LeavePending,
    /// v1. Call once more, then quarantine.
    Retry,
}

#[cfg(feature = "llm")]
fn attempt_after_error(
    pipeline: PipelineVersion,
    err: &crate::graph::error::GraphError,
) -> Attempt {
    // v1 into a v2 store is a refusal, not a flaky write. Retrying would
    // call the provider again and still write nothing.
    if matches!(err, crate::graph::error::GraphError::PipelineRefused(_)) {
        return Attempt::Stop;
    }
    if pipeline != PipelineVersion::V2 {
        return Attempt::Retry;
    }
    if crate::graph::v2::is_stamp_refusal(err) {
        Attempt::Stop
    } else {
        Attempt::LeavePending
    }
}

#[cfg(feature = "llm")]
fn refusal_text(err: &crate::graph::error::GraphError) -> &'static str {
    match err {
        crate::graph::error::GraphError::PipelineRefused(message) => message,
        _ => crate::graph::v2::V2_REFUSAL,
    }
}

/// Windows, calls, candidates, drops by reason, and kept. Empty for v1,
/// whose counters stay zero, so the v1 success line does not change.
#[cfg(feature = "llm")]
fn v2_report_suffix(report: &IngestionReport) -> String {
    if report.windows == 0 && report.candidates == 0 && report.kept == 0 && report.drops.is_empty()
    {
        return String::new();
    }
    let drops = if report.drops.is_empty() {
        "none".to_string()
    } else {
        let mut parts: Vec<_> = report.drops.iter().collect();
        parts.sort_by(|left, right| left.0.cmp(right.0));
        parts
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        ", {} windows, {} calls, {} candidates, drops {drops}, {} kept{}",
        report.windows,
        report.llm_calls,
        report.candidates,
        report.kept,
        resolve_suffix(&report.resolve)
    )
}

#[cfg(feature = "llm")]
fn resolve_suffix(stats: &ResolveStats) -> String {
    if stats.is_blank() {
        return String::new();
    }
    let rejected = if stats.rejected.is_empty() {
        "none".to_string()
    } else {
        stats
            .rejected
            .iter()
            .map(|item| format!("{}->{}", item.mention, item.match_id))
            .collect::<Vec<_>>()
            .join(",")
    };
    let notes = if stats.notes.is_empty() {
        String::new()
    } else {
        format!(", notes {}", stats.notes.join("; "))
    };
    format!(
        ", resolve mentions {}, tier {}/{}/{}, called {}, accepted {}, rejected {rejected}{notes}",
        stats.mentions,
        stats.tier_name_kind,
        stats.tier_alias,
        stats.tier_name_any,
        stats.resolver_called,
        stats.accepted
    )
}

/// Arguments for [`extract`]. One value, so the pipeline flag does not become
/// a ninth parameter.
#[cfg(feature = "llm")]
pub struct ExtractOptions {
    /// One archive, by log number.
    pub log: Option<u32>,
    /// Every archive that has not been extracted.
    pub all: bool,
    /// List the archives and call nothing.
    pub dry_run: bool,
    /// Provider model override.
    pub model: Option<String>,
    /// Provider override.
    pub provider: Option<String>,
    /// Milliseconds to wait between archives.
    pub delay_ms: u64,
    /// Spend cap across the run. Zero means no cap.
    pub max_tokens: u64,
    /// Flag value. `None` reads `[graph] pipeline`, which defaults to v1.
    pub pipeline: Option<PipelineVersion>,
    /// Build a fresh v2 store and swap it into place. Ignores `--log` and `--all`.
    pub rebuild: bool,
}

/// Extract entities from already-ingested archives using an LLM.
#[cfg(feature = "llm")]
pub async fn extract(memory_dir: &Path, options: ExtractOptions) -> Result<(), RecallError> {
    if options.rebuild {
        return rebuild(memory_dir, options, &LiveDaemonStop, None).await;
    }
    let ExtractOptions {
        log,
        all,
        dry_run,
        model: model_override,
        provider: provider_override,
        delay_ms,
        max_tokens,
        pipeline,
        rebuild: _,
    } = options;
    let pipeline_flag = pipeline;
    let memory = memory_dir.to_path_buf();
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        // The store is already open, so the stamp is read from it. A refusal
        // happens before any archive is marked or rewritten.
        let pipeline =
            crate::graph::v2::chosen_pipeline(&memory, pipeline_flag, Some(gm.db())).await?;
        // Determine which log numbers to process
        let log_numbers: Vec<u32> = if let Some(ln) = log {
            vec![ln]
        } else if all {
            gm.unextracted_log_numbers()
                .await?
                .into_iter()
                .map(|n| n as u32)
                .collect()
        } else {
            return Err(RecallError::Other("Specify --log <N> or --all".into()));
        };

        if log_numbers.is_empty() {
            println!("{WARN}No unextracted archives found.{RESET}");
            return Ok(());
        }

        let conversations_dir = find_conversations_dir(memory_dir)?;

        if dry_run {
            print_extract_dry_run(&conversations_dir, &log_numbers);
            return Ok(());
        }

        // Build LLM provider from .recall-echo.toml (CLI flags override)
        let (llm, model_name) = crate::llm_provider::create_provider(
            memory_dir,
            provider_override.as_deref(),
            model_override.as_deref(),
        )?;

        let total_count = log_numbers.len();
        let budget_label = if max_tokens > 0 {
            format!(" (budget: {})", format_tokens(max_tokens))
        } else {
            String::new()
        };
        // A CLI provider with no configured model uses its own default, and
        // says so rather than printing "using ".
        let model_label = if model_name.is_empty() {
            "the provider default"
        } else {
            &model_name
        };
        println!(
            "{BOLD}Extracting entities from {total_count} archives using {model_label}{budget_label}{RESET}",
        );

        let quarantine_path = graph_dir.join("extraction-quarantine.txt");
        let mut totals = ExtractionTotals::default();
    let mut consecutive_total_failures: u32 = 0;

        for (idx, ln) in log_numbers.iter().enumerate() {
            // Budget check. Measured and estimated tokens are both spent
            // tokens, so the budget is charged the sum of the two.
            let spent = totals.measured_tokens + totals.estimated_tokens;
            if max_tokens > 0 && spent >= max_tokens {
                println!(
                    "\n{WARN}⚠ Token budget exhausted ({} of {}). Stopping.{RESET}",
                    format_token_bill(totals.measured_tokens, totals.estimated_tokens),
                    format_tokens(max_tokens),
                );
                println!("  Re-run to continue — resume is automatic via unextracted log numbers.");
                break;
            }

            let archive_path = match find_archive_file(&conversations_dir, *ln) {
                Ok(p) => p,
                Err(e) => {
                    println!(
                        "  {WARN}⚠{RESET} [{}/{}] log {ln:03}: {e}",
                        idx + 1,
                        total_count
                    );
                    totals.errors.push(format!("log {ln:03}: {e}"));
                    continue;
                }
            };

            let content = std::fs::read_to_string(&archive_path)?;
            let (session_id, _) = extract_archive_metadata(&content, &archive_path);
            let context = IngestContext::new(session_id, Some(*ln));

            // v1 retries once, then quarantines. v2 does not: a stamp refusal
            // stops the run, and any other error (a write that already landed)
            // is left pending without a second provider call. The second call
            // goes through `retry_once`, which rebuild also uses; rebuild
            // stops on that second failure instead of quarantining.
            let first = crate::graph::v2::extract_log(
                &gm,
                &memory,
                *ln,
                &content,
                &context,
                &*llm,
                pipeline,
            )
            .await;
            if let Err(ref err) = first {
                match attempt_after_error(pipeline, err) {
                    Attempt::Stop => {
                        let message = refusal_text(err);
                        println!("\n{WARN}✗{RESET} {message}");
                        return Err(RecallError::Other(message.to_string()));
                    }
                    Attempt::LeavePending => {
                        println!(
                            "  {WARN}✗{RESET} [{}/{}] log {ln:03}: left pending ({err})",
                            idx + 1,
                            total_count
                        );
                        totals.left_pending.push(*ln);
                        totals
                            .errors
                            .push(format!("log {ln:03}: left pending: {err}"));
                        continue;
                    }
                    Attempt::Retry => {
                        println!(
                            "  {WARN}⚠{RESET} [{}/{}] log {ln:03}: failed, retrying... ({err})",
                            idx + 1,
                            total_count
                        );
                    }
                }
            }
            let retry = matches!(
                first.as_ref().err().map(|err| attempt_after_error(pipeline, err)),
                Some(Attempt::Retry)
            );
            let report = match retry_once(first, retry, || {
                crate::graph::v2::extract_log(
                    &gm,
                    &memory,
                    *ln,
                    &content,
                    &context,
                    &*llm,
                    pipeline,
                )
            })
            .await
            {
                Ok(report) => report,
                Err(err) => {
                    println!(
                        "  {WARN}✗{RESET} [{}/{}] log {ln:03}: quarantined ({err})",
                        idx + 1,
                        total_count
                    );
                    totals.quarantined.push(*ln);
                    totals
                        .errors
                        .push(format!("log {ln:03}: quarantined after retry: {err}"));
                    continue;
                }
            };

            // Whatever happened, the tokens were spent: keep the budget honest.
            totals.estimated_tokens += report.estimated_tokens;
            totals.measured_tokens += report.measured_tokens;
            totals.dedup_llm_calls += report.dedup_llm_calls;
            totals.dedup_fast_path += report.dedup_fast_path;

            // Every chunk failed: the provider is the problem, not the archive.
            // Leave it pending — marking it would hide a broken provider behind
            // "extracted, empty". Deliberately no retry here, unlike an Err
            // above: the user is watching and will re-run; the daemon has its
            // own ladder. Three in a row and we stop burning spawns.
            if report.leaves_pending() {
                if report.is_total_failure() {
                    println!(
                        "  {WARN}✗{RESET} [{}/{}] log {ln:03}: every chunk failed ({}), nothing extracted — left pending ({})",
                        idx + 1,
                        total_count,
                        report.chunks_failed,
                        report.errors.first().map_or("", String::as_str),
                    );
                } else {
                    println!(
                        "  {WARN}✗{RESET} [{}/{}] log {ln:03}: {} of {} windows failed — left pending ({})",
                        idx + 1,
                        total_count,
                        report.chunks_failed,
                        report.windows,
                        report.errors.first().map_or("", String::as_str),
                    );
                }
                totals.left_pending.push(*ln);
                let total = report.is_total_failure();
                totals.errors.extend(report.errors);
                if !total {
                    consecutive_total_failures = 0;
                    continue;
                }
                consecutive_total_failures += 1;
                if consecutive_total_failures >= MAX_CONSECUTIVE_TOTAL_FAILURES {
                    println!(
                        "\n{WARN}✗ {MAX_CONSECUTIVE_TOTAL_FAILURES} archives failed outright in a row — the provider looks broken. Stopping.{RESET}"
                    );
                    break;
                }
                continue;
            }
            consecutive_total_failures = 0;

            // A chunk that failed inside an otherwise successful archive is
            // partial yield, not success — say so on the line itself, not
            // only in the trailing warnings.
            let warn_label = if report.errors.is_empty() {
                String::new()
            } else {
                format!(
                    ", {WARN}{} warning{}{RESET}",
                    report.errors.len(),
                    if report.errors.len() == 1 { "" } else { "s" }
                )
            };
            let slots_label = if report.slots_written > 0 {
                format!(", {} slots", report.slots_written)
            } else {
                String::new()
            };
            let v2_label = v2_report_suffix(&report);
            println!(
                "  {GOOD}✓{RESET} [{}/{}] log {ln:03}: +{} entities, ~{} merged, -{} skipped, {} rels{slots_label}{v2_label} ({}){warn_label}",
                idx + 1,
                total_count,
                report.entities_created,
                report.entities_merged,
                report.entities_skipped,
                report.relationships_created,
                format_token_bill(report.measured_tokens, report.estimated_tokens),
            );

            gm.mark_extracted(*ln).await?;

            totals.entities_created += report.entities_created;
            totals.entities_merged += report.entities_merged;
            totals.entities_skipped += report.entities_skipped;
            totals.relationships += report.relationships_created;
            totals.errors.extend(report.errors);
            totals.processed += 1;

            // Rate limiting between archives
            if delay_ms > 0 && *ln != *log_numbers.last().unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        // Write quarantine file if any archives failed
        if !totals.quarantined.is_empty() {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&quarantine_path)?;
            for ln in &totals.quarantined {
                writeln!(file, "{ln:03}")?;
            }
            println!(
                "\n  {WARN}Quarantined {} archives → {}{RESET}",
                totals.quarantined.len(),
                quarantine_path.display()
            );
        }

        print_extract_summary(&totals);
        Ok(())
    })
    .await
}

// ── Vigil sync commands ──────────────────────────────────────────────

/// Sync vigil-pulse signals and outcomes into the graph.
pub async fn vigil_sync(
    memory_dir: &Path,
    signals_path: Option<&Path>,
    outcomes_path: Option<&Path>,
) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    // Default paths: look for vigil/ and caliber/ relative to memory_dir's parent (pulse root)
    let pulse_root = memory_dir.parent().unwrap_or(memory_dir);

    let default_signals = pulse_root.join("vigil").join("signals.json");
    let default_outcomes = pulse_root.join("caliber").join("outcomes.json");

    let sig_path = signals_path.unwrap_or(&default_signals);
    let out_path = outcomes_path.unwrap_or(&default_outcomes);

    serve_client::exclusive(memory_dir, |gm| async move {
        let report = gm.sync_vigil(sig_path, out_path).await?;

        println!("{BOLD}Vigil Sync{RESET}");
        println!("  Measurements: +{}", report.measurements_created);
        println!("  Outcomes:     +{}", report.outcomes_created);
        println!("  Relationships: +{}", report.relationships_created);
        println!("  Skipped:       {}", report.skipped);

        if !report.errors.is_empty() {
            println!("\n  {WARN}Warnings:{RESET}");
            for err in &report.errors {
                println!("    {DIM}{err}{RESET}");
            }
        }

        if report.measurements_created == 0 && report.outcomes_created == 0 {
            println!("\n  {DIM}No new data — graph is in sync.{RESET}");
        }

        Ok(())
    })
    .await
}

// ── Pipeline commands ──────────────────────────────────────────────────

/// Sync pipeline documents into the graph.
pub async fn pipeline_sync(
    memory_dir: &Path,
    docs_dir_override: Option<&Path>,
) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    // Resolve docs directory: CLI flag > config > error
    let docs_dir = if let Some(d) = docs_dir_override {
        d.to_path_buf()
    } else {
        let cfg = crate::config::load_from_dir(memory_dir);
        match cfg.pipeline.and_then(|p| p.docs_dir) {
            Some(d) => {
                let path = PathBuf::from(shellexpand(&d));
                if !path.exists() {
                    return Err(RecallError::Config(format!(
                        "Configured docs_dir does not exist: {}",
                        path.display()
                    )));
                }
                path
            }
            None => {
                return Err(
                    "No docs directory specified. Use --docs-dir or set [pipeline] docs_dir in config.".into(),
                );
            }
        }
    };

    // Read pipeline documents
    let docs = read_pipeline_docs(&docs_dir)?;

    let report = crate::graph_bridge::sync_pipeline_into_graph(memory_dir, docs).await?;

    println!("{BOLD}Pipeline Sync{RESET}");
    println!("  Created:      {}", report.entities_created);
    println!("  Updated:      {}", report.entities_updated);
    println!("  Archived:     {}", report.entities_archived);
    println!(
        "  Relationships: +{} / ~{} skipped",
        report.relationships_created, report.relationships_skipped
    );

    if !report.errors.is_empty() {
        println!("\n  {WARN}Warnings:{RESET}");
        for err in &report.errors {
            println!("    {DIM}{err}{RESET}");
        }
    }

    if report.entities_created == 0 && report.entities_updated == 0 && report.entities_archived == 0
    {
        println!("\n  {DIM}No changes — graph is in sync.{RESET}");
    }

    Ok(())
}

/// Show pipeline health stats.
pub async fn pipeline_status(memory_dir: &Path, staleness_days: u32) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        let stats = gm.pipeline_stats(staleness_days).await?;

        println!(
            "{BOLD}Pipeline Status{RESET} ({} entities)",
            stats.total_entities
        );

        if stats.by_stage.is_empty() {
            println!(
                "  {DIM}No pipeline entities in graph. Run `graph pipeline sync` first.{RESET}"
            );
            return Ok(());
        }

        // Display stages in pipeline order
        let stage_order = ["learning", "thoughts", "curiosity", "reflections", "praxis"];
        for stage in &stage_order {
            if let Some(statuses) = stats.by_stage.get(*stage) {
                println!("\n  {ACCENT}{}{RESET}", stage.to_uppercase());
                let mut items: Vec<_> = statuses.iter().collect();
                items.sort_by_key(|(s, _)| (*s).clone());
                for (status, count) in items {
                    println!("    {status}: {count}");
                }
            }
        }

        if !stats.stale_thoughts.is_empty() {
            println!("\n  {WARN}Stale thoughts (>{staleness_days}d):{RESET}");
            for entity in &stats.stale_thoughts {
                println!("    {DIM}•{RESET} {}", entity.name);
            }
        }

        if !stats.stale_questions.is_empty() {
            println!(
                "\n  {WARN}Stale questions (>{}d):{RESET}",
                staleness_days * 2
            );
            for entity in &stats.stale_questions {
                println!("    {DIM}•{RESET} {}", entity.name);
            }
        }

        if let Some(ref last) = stats.last_movement {
            println!("\n  {DIM}Last movement: {last}{RESET}");
        }

        Ok(())
    })
    .await
}

/// Trace pipeline flow for an entity.
pub async fn pipeline_flow(memory_dir: &Path, entity_name: &str) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        let chain = gm.pipeline_flow(entity_name).await?;

        if chain.is_empty() {
            println!("{WARN}No pipeline relationships found for \"{entity_name}\".{RESET}");
            return Ok(());
        }

        println!("{BOLD}Pipeline Flow: {entity_name}{RESET}\n");
        for (source, rel_type, target) in &chain {
            println!(
                "  {} ({}) {ACCENT}—[{rel_type}]→{RESET} {} ({})",
                source.name, source.entity_type, target.name, target.entity_type
            );
        }

        Ok(())
    })
    .await
}

/// List stale pipeline entities.
pub async fn pipeline_stale(memory_dir: &Path, staleness_days: u32) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        let stats = gm.pipeline_stats(staleness_days).await?;

        let total_stale = stats.stale_thoughts.len() + stats.stale_questions.len();
        if total_stale == 0 {
            println!("{GOOD}✓{RESET} No stale pipeline entities.");
            return Ok(());
        }

        println!("{BOLD}Stale Pipeline Entities{RESET}\n");

        if !stats.stale_thoughts.is_empty() {
            println!("  {WARN}Thoughts (>{staleness_days} days):{RESET}");
            for entity in &stats.stale_thoughts {
                println!("    • {} {DIM}({}){RESET}", entity.name, entity.entity_type);
            }
        }

        if !stats.stale_questions.is_empty() {
            println!("  {WARN}Questions (>{} days):{RESET}", staleness_days * 2);
            for entity in &stats.stale_questions {
                println!("    • {} {DIM}({}){RESET}", entity.name, entity.entity_type);
            }
        }

        Ok(())
    })
    .await
}

/// Read pipeline documents from a directory.
fn read_pipeline_docs(dir: &Path) -> Result<PipelineDocuments, RecallError> {
    let read_or_empty = |name: &str| -> String {
        let path = dir.join(name);
        std::fs::read_to_string(&path).unwrap_or_default()
    };

    Ok(PipelineDocuments {
        learning: read_or_empty("LEARNING.md"),
        thoughts: read_or_empty("THOUGHTS.md"),
        curiosity: read_or_empty("CURIOSITY.md"),
        reflections: read_or_empty("REFLECTIONS.md"),
        praxis: read_or_empty("PRAXIS.md"),
    })
}

/// Expand ~ to home directory in paths.
fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// Find the conversations directory — checks memory_dir/conversations/ then parent/conversations/.
pub(crate) fn find_conversations_dir(memory_dir: &Path) -> Result<PathBuf, RecallError> {
    let conv = memory_dir.join("conversations");
    if conv.exists() {
        return Ok(conv);
    }
    if let Some(parent) = memory_dir.parent() {
        let parent_conv = parent.join("conversations");
        if parent_conv.exists() {
            return Ok(parent_conv);
        }
    }
    Err(RecallError::NotInitialized(
        "conversations/ directory not found".into(),
    ))
}

/// Thresholds and mode for one `graph gc` invocation.
///
/// A struct rather than eight positional arguments: every field is a knob the
/// CLI exposes, and callers should be able to take the defaults.
#[derive(Debug, Clone)]
pub struct GcOptions {
    /// Actually delete. The default is a dry run.
    pub execute: bool,
    pub stale_days: u64,
    pub stale_confidence: f64,
    pub dead_confidence: f64,
    pub dead_min_age_days: u64,
    /// Also sweep episodes.
    pub episodes: bool,
    pub episode_max_age_days: u64,
    /// Report health only, computing no deletion candidates.
    pub stats_only: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        let defaults = crate::graph::gc::GcConfig::default();
        Self {
            execute: false,
            stale_days: defaults.stale_days,
            stale_confidence: defaults.stale_confidence,
            dead_confidence: defaults.dead_confidence,
            dead_min_age_days: defaults.dead_min_age_days,
            episodes: false,
            episode_max_age_days: defaults.episode_max_age_days,
            stats_only: false,
        }
    }
}

impl GcOptions {
    fn to_config(&self) -> crate::graph::gc::GcConfig {
        crate::graph::gc::GcConfig {
            stale_days: self.stale_days,
            stale_confidence: self.stale_confidence,
            dead_confidence: self.dead_confidence,
            dead_min_age_days: self.dead_min_age_days,
            collect_episodes: self.episodes,
            episode_max_age_days: self.episode_max_age_days,
            dry_run: !self.execute,
            protect_pipeline: true,
        }
    }
}

/// Run garbage collection on the graph.
pub async fn gc(memory_dir: &Path, options: &GcOptions) -> Result<(), RecallError> {
    use crate::graph::gc::GcActionKind;

    let stats_only = options.stats_only;
    let config = options.to_config();

    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        if stats_only {
            let stats = gm.gc_stats().await?;
            println!("{BOLD}Graph Health{RESET}");
            println!("  Entities:              {}", stats.total_entities);
            println!("  Relationships:         {}", stats.total_relationships);
            println!(
                "  Pipeline entities:     {} {DIM}(protected){RESET}",
                stats.pipeline_entities
            );
            println!("  Zero-access entities:  {}", stats.zero_access_entities);
            println!(
                "  Low confidence rels:   {} {DIM}(< 0.5){RESET}",
                stats.low_confidence_rels
            );
            println!(
                "  Very low conf. rels:   {} {DIM}(< 0.2){RESET}",
                stats.very_low_confidence_rels
            );
            println!("  Superseded rels:       {}", stats.superseded_rels);
            return Ok(());
        }

        let report = gm.run_gc(&config).await?;

        // Header
        if report.dry_run {
            println!(
                "{BOLD}{WARN}GC Dry Run{RESET} {DIM}(pass --execute to actually delete){RESET}"
            );
        } else {
            println!("{BOLD}{GOOD}GC Executed{RESET}");
        }

        println!("\n{BOLD}Scan{RESET}");
        println!("  Entities scanned:      {}", report.entities_scanned);
        println!("  Relationships scanned: {}", report.relationships_scanned);
        if config.collect_episodes {
            println!("  Episodes scanned:      {}", report.episodes_scanned);
        }

        println!("\n{BOLD}Results{RESET}");
        println!("  Stale relationships:   {}", report.stale_relationships);
        println!("  Dead relationships:    {}", report.dead_relationships);
        println!("  Orphaned entities:     {}", report.orphaned_entities);
        if config.collect_episodes {
            println!("  Spent episodes:        {}", report.spent_episodes);
        }

        let verb = if report.dry_run {
            "would remove"
        } else {
            "removed"
        };
        println!("  Total {verb}:         {}", report.total_removed);

        // Details
        if !report.actions.is_empty() {
            println!("\n{BOLD}Actions{RESET}");
            for action in &report.actions {
                let icon = match action.kind {
                    GcActionKind::StaleRelationship => format!("{WARN}⚠{RESET}"),
                    GcActionKind::DeadRelationship => format!("{WARN}✗{RESET}"),
                    GcActionKind::OrphanedEntity => format!("{ACCENT}○{RESET}"),
                    GcActionKind::SpentEpisode => format!("{ACCENT}◌{RESET}"),
                };
                println!(
                    "  {icon} [{kind}] {name}",
                    kind = action.kind,
                    name = action.target_name,
                );
                println!("    {DIM}{reason}{RESET}", reason = action.reason);
            }
        }

        if !report.errors.is_empty() {
            println!("\n{BOLD}Errors{RESET}");
            for err in &report.errors {
                println!("  {BAD}✗{RESET} {err}");
            }
        }

        Ok(())
    })
    .await
}

/// Apply an outcome to every entity a session touched.
///
/// A hot operation: it goes through the daemon like search and ingest, so it
/// can be run while a session is still using the store.
pub async fn feedback(
    memory_dir: &Path,
    session_id: &str,
    outcome: &str,
) -> Result<(), RecallError> {
    use crate::graph::utility::OutcomeKind;
    use crate::serve::FeedbackArgs;

    let outcome: OutcomeKind = outcome.parse().map_err(RecallError::Other)?;

    let request = Request::Feedback(FeedbackArgs {
        session_id: session_id.to_string(),
        outcome,
    });
    let report: crate::graph::utility::FeedbackReport =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    if report.entities_updated == 0 && report.utilities.is_empty() {
        println!(
            "{WARN}No entities recorded for session {session_id}.{RESET} \
             {DIM}Nothing to apply the outcome to.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{GOOD}✓{RESET} Session {BOLD}{session_id}{RESET} recorded as {BOLD}{outcome}{RESET} \
         — {} entities updated",
        report.entities_updated
    );

    for entity in &report.utilities {
        println!(
            "  {DIM}{}{RESET} utility {ACCENT}{:.3}{RESET}",
            entity.entity_id, entity.utility_score
        );
    }

    if !report.errors.is_empty() {
        println!("\n{WARN}Warnings:{RESET}");
        for err in &report.errors {
            println!("  {DIM}{err}{RESET}");
        }
    }

    Ok(())
}

// ── Correction ───────────────────────────────────────────────────────────

/// One `graph correct` invocation, as the flags were given.
///
/// A struct rather than seven positional arguments, and unvalidated on purpose:
/// turning it into a [`CorrectTarget`] and a [`Correction`] is where the
/// contradictory combinations are named and refused.
#[derive(Debug, Clone)]
pub struct CorrectOptions {
    /// Entity name, or the source entity of a relationship.
    pub subject: String,
    /// Relationship type — only for a relationship target.
    pub rel_type: Option<String>,
    /// Target entity — only for a relationship target.
    pub object: Option<String>,
    /// Record contradicting evidence at user authority.
    pub wrong: bool,
    /// Remove outright.
    pub forget: bool,
    /// Contradict every relationship of an entity rather than being asked which.
    pub all_edges: bool,
    /// Skip the confirmation a removal otherwise requires.
    pub yes: bool,
}

impl CorrectOptions {
    /// What the correction is aimed at.
    fn target(&self) -> Result<CorrectTarget, RecallError> {
        match (&self.rel_type, &self.object) {
            (None, None) => Ok(CorrectTarget::Entity {
                name: self.subject.clone(),
            }),
            (Some(rel_type), Some(object)) => Ok(CorrectTarget::Edge {
                from: self.subject.clone(),
                rel_type: rel_type.clone(),
                to: object.clone(),
            }),
            _ => Err(RecallError::Other(
                "a relationship needs all three names: \
                 `graph correct <from> <REL> <to> --wrong`"
                    .into(),
            )),
        }
    }

    /// What to do to it.
    fn correction(&self) -> Result<Correction, RecallError> {
        match (self.wrong, self.forget) {
            (true, false) => Ok(Correction::Wrong {
                all_edges: self.all_edges,
            }),
            (false, true) => Ok(Correction::Forget { confirmed: false }),
            (true, true) => Err(RecallError::Other(
                "--wrong and --forget are different corrections; pass one".into(),
            )),
            (false, false) => Err(RecallError::Other(
                "say what to do: --wrong records that it is mistaken (confidence falls with \
                 evidence), --forget removes it outright"
                    .into(),
            )),
        }
    }
}

/// Tell memory that something it learned is wrong.
///
/// A hot operation: it goes through the daemon like search and ingest, so a
/// correction lands while a session is still using the store.
pub async fn correct(memory_dir: &Path, options: &CorrectOptions) -> Result<(), RecallError> {
    let target = options.target()?;
    let report = send_correction(memory_dir, &target, options.correction()?).await?;

    match report {
        // A removal is planned before it is applied, so the human sees what
        // goes before anything does.
        CorrectionReport::Planned { removal } => {
            print_removal_plan(&removal);
            if !options.yes && !confirm_removal(&removal)? {
                println!("{WARN}Nothing removed.{RESET}");
                return Ok(());
            }
            let applied =
                send_correction(memory_dir, &target, Correction::Forget { confirmed: true })
                    .await?;
            // Still through `refusal`: the graph can have moved between the
            // plan and the confirmation, and a removal that found nothing to
            // remove must not exit as though it had.
            print_correction(&applied);
            refusal(&applied)
        }
        other => {
            print_correction(&other);
            refusal(&other)
        }
    }
}

async fn send_correction(
    memory_dir: &Path,
    target: &CorrectTarget,
    correction: Correction,
) -> Result<CorrectionReport, RecallError> {
    let request = Request::Correct(CorrectArgs {
        target: target.clone(),
        correction,
    });
    Ok(serde_json::from_value(
        serve_client::execute(memory_dir, &request).await?,
    )?)
}

/// A correction that changed nothing exits non-zero: the user asked for a
/// change and did not get one, and a script must be able to tell.
fn refusal(report: &CorrectionReport) -> Result<(), RecallError> {
    match report {
        CorrectionReport::UnknownEntity { query, .. } => Err(RecallError::Other(format!(
            "no entity named \"{query}\" — nothing was changed"
        ))),
        CorrectionReport::NoSuchEdge {
            from, rel_type, to, ..
        } => Err(RecallError::Other(format!(
            "no {rel_type} relationship between \"{from}\" and \"{to}\" — nothing was changed"
        ))),
        CorrectionReport::Ambiguous { entity, .. } => Err(RecallError::Other(format!(
            "\"{entity}\" takes part in several relationships — name the one that is wrong"
        ))),
        CorrectionReport::NothingToCorrect { entity } => Err(RecallError::Other(format!(
            "\"{entity}\" has no relationships to contradict"
        ))),
        _ => Ok(()),
    }
}

fn print_correction(report: &CorrectionReport) {
    match report {
        CorrectionReport::UnknownEntity { query, candidates } => {
            println!("{WARN}No entity named{RESET} {BOLD}{query}{RESET}.");
            if candidates.is_empty() {
                println!("  {DIM}Nothing stored is close to that name.{RESET}");
            } else {
                println!("\n  {DIM}Closest names in memory:{RESET}");
                for candidate in candidates {
                    println!(
                        "    {BOLD}{}{RESET} {DIM}({}){RESET}",
                        candidate.name, candidate.entity_type
                    );
                }
            }
        }
        CorrectionReport::NoSuchEdge {
            from,
            rel_type,
            to,
            existing,
        } => {
            println!(
                "{WARN}Memory holds no{RESET} {BOLD}{rel_type}{RESET} \
                 {WARN}relationship between{RESET} {BOLD}{from}{RESET} \
                 {WARN}and{RESET} {BOLD}{to}{RESET}."
            );
            if existing.is_empty() {
                println!("  {DIM}They are not connected at all.{RESET}");
            } else {
                println!("\n  {DIM}What does connect them:{RESET}");
                print_edges(existing);
            }
        }
        CorrectionReport::Ambiguous { entity, edges } => {
            println!(
                "{BOLD}{entity}{RESET} takes part in {} relationships. Which one is wrong?\n",
                edges.len()
            );
            print_edges(edges);
            println!("\n  {DIM}Name it:{RESET}");
            if let Some(edge) = edges.first() {
                println!(
                    "    {DIM}recall-echo graph correct \"{}\" \"{}\" \"{}\" --wrong{RESET}",
                    edge.from, edge.rel_type, edge.to
                );
            }
            println!("  {DIM}Or contradict every one of them:{RESET}");
            println!("    {DIM}recall-echo graph correct \"{entity}\" --wrong --all-edges{RESET}");
        }
        CorrectionReport::NothingToCorrect { entity } => {
            println!(
                "{BOLD}{entity}{RESET} is in memory but takes part in no relationships, \
                 so there is no claim to contradict."
            );
            println!(
                "  {DIM}To remove the entity itself: \
                 recall-echo graph correct \"{entity}\" --forget{RESET}"
            );
        }
        CorrectionReport::Contradicted { edges } => print_contradictions(edges),
        CorrectionReport::Planned { removal } => print_removal_plan(removal),
        CorrectionReport::Removed { removal } => {
            let entity = removal
                .entity
                .as_ref()
                .map(|entity| format!("{BOLD}{}{RESET} and ", entity.name))
                .unwrap_or_default();
            println!(
                "{GOOD}✓{RESET} Removed {entity}{} {}.",
                removal.edges.len(),
                plural(removal.edges.len(), "relationship", "relationships")
            );
        }
    }
}

fn print_contradictions(edges: &[EdgeCorrection]) {
    println!(
        "{GOOD}✓{RESET} Recorded your correction on {} {}.\n",
        edges.len(),
        plural(edges.len(), "relationship", "relationships")
    );
    for correction in edges {
        let edge = &correction.edge;
        println!(
            "  {} {ACCENT}—[{}]→{RESET} {}",
            edge.from, edge.rel_type, edge.to
        );
        println!(
            "    confidence {WARN}{:.2} → {:.2}{RESET}   {DIM}evidence {:.1} → {:.1}{RESET}",
            correction.confidence_before,
            edge.confidence,
            correction.evidence_before,
            edge.evidence,
        );
    }
    println!(
        "\n  {DIM}Your correction is evidence, not a decree: confidence falls by the weight of \
         one observation you authored. Say it again if memory is still wrong.{RESET}"
    );
}

fn print_removal_plan(removal: &Removal) {
    println!("{BOLD}{WARN}This would remove:{RESET}\n");
    if let Some(entity) = &removal.entity {
        println!(
            "  {BOLD}{}{RESET} {DIM}({}){RESET}",
            entity.name, entity.entity_type
        );
    }
    if removal.edges.is_empty() {
        println!("  {DIM}and no relationships.{RESET}");
    } else {
        println!(
            "  {} {}:",
            removal.edges.len(),
            plural(removal.edges.len(), "relationship", "relationships")
        );
        print_edges(&removal.edges);
    }
    println!(
        "\n  {DIM}Removal is not evidence — it leaves no trace and cannot be re-weighed. \
         Prefer --wrong unless the memory should never have existed.{RESET}"
    );
}

fn print_edges(edges: &[EdgeView]) {
    for edge in edges {
        let superseded = if edge.superseded {
            format!(" {DIM}[superseded]{RESET}")
        } else {
            String::new()
        };
        let coherence = if edge.self_reinforcements > 0 {
            format!(" {WARN}self×{}{RESET}", edge.self_reinforcements)
        } else {
            String::new()
        };
        println!(
            "    {} {ACCENT}—[{}]→{RESET} {}  {:.0}%{coherence}{superseded}",
            edge.from,
            edge.rel_type,
            edge.to,
            edge.confidence * 100.0,
        );
    }
}

/// Ask before destroying anything.
///
/// A non-interactive stdin is never taken as consent: a script that pipes into
/// this command must say `--yes` in the script, where a reader can see it.
///
/// The read blocks the runtime, which is what we want — there is nothing else
/// in flight, and the daemon connection was closed with the planning request.
fn confirm_removal(removal: &Removal) -> Result<bool, RecallError> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return Err(RecallError::Other(
            "refusing to remove memory without confirmation — re-run with --yes".into(),
        ));
    }

    let what = removal
        .entity
        .as_ref()
        .map_or_else(|| "these relationships".to_string(), |e| e.name.clone());
    print!("{BOLD}Remove {what}?{RESET} [y/N] ");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// Show relationship decay report — lists all relationships with their stored vs effective confidence.
pub async fn decay_report(
    memory_dir: &Path,
    entity_name: Option<&str>,
    show_all: bool,
) -> Result<(), RecallError> {
    use crate::graph::confidence;
    use crate::graph::types::Direction;

    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
        let now = chrono::Utc::now();

        let rels = if let Some(name) = entity_name {
            gm.get_relationships(name, Direction::Both).await?
        } else {
            crate::graph::crud::list_all_relationships(gm.db()).await?
        };

        if rels.is_empty() {
            println!("{WARN}No relationships found.{RESET}");
            return Ok(());
        }

        println!(
            "{BOLD}Decay Report{RESET} ({} relationships, half-life: {} days)\n",
            rels.len(),
            confidence::DEFAULT_HALF_LIFE_DAYS
        );

        let mut decayed_count = 0u32;
        let mut total_decay = 0.0_f64;

        for rel in &rels {
            let effective = confidence::effective_confidence(
                rel.confidence,
                rel.last_reinforced.as_ref(),
                &rel.valid_from,
                &now,
            );

            let decay_amount = rel.confidence - effective;
            if decay_amount > 0.001 {
                decayed_count += 1;
            }
            total_decay += decay_amount;

            if !show_all && decay_amount < 0.001 {
                continue;
            }

            let from_short = match &rel.from_id {
                serde_json::Value::String(s) => s.split(':').next_back().unwrap_or(s).to_string(),
                other => other.to_string(),
            };
            let to_short = match &rel.to_id {
                serde_json::Value::String(s) => s.split(':').next_back().unwrap_or(s).to_string(),
                other => other.to_string(),
            };

            let reinforced_tag = match &rel.last_reinforced {
                Some(serde_json::Value::String(s)) => format!(" {DIM}(reinforced: {s}){RESET}"),
                _ => String::new(),
            };

            let decay_indicator = if decay_amount > 0.2 {
                format!("{BAD}↓{:.0}%{RESET}", decay_amount * 100.0)
            } else if decay_amount > 0.05 {
                format!("{WARN}↓{:.0}%{RESET}", decay_amount * 100.0)
            } else {
                format!("{DIM}≈{RESET}")
            };

            // Evidence behind the score: how much corroboration it rests on,
            // and how much of that was the agent re-asserting itself.
            let edge_evidence = rel.edge_evidence();
            let coherence = edge_evidence.self_reinforcements();
            let evidence = rel.evidence();
            let evidence_tag = format!(
                " {DIM}[n={:.1} ±{:.2}{}]{RESET}",
                evidence.concentration(),
                evidence.variance().sqrt(),
                if coherence > 0 {
                    format!(", self×{coherence}")
                } else {
                    String::new()
                }
            );

            println!(
                "  {from_short} {ACCENT}—[{}]→{RESET} {to_short}  stored:{:.2} effective:{:.2} {decay_indicator}{evidence_tag}{reinforced_tag}",
                rel.rel_type, rel.confidence, effective,
            );
        }

        println!(
            "\n{BOLD}Summary{RESET}: {decayed_count}/{} relationships decayed, avg decay: {:.3}",
            rels.len(),
            if rels.is_empty() {
                0.0
            } else {
                total_decay / rels.len() as f64
            }
        );

        Ok(())
    })
    .await
}

/// Find the archive file for a given log number.
#[cfg(feature = "llm")]
pub(crate) fn find_archive_file(
    conversations_dir: &Path,
    log_number: u32,
) -> Result<PathBuf, RecallError> {
    // Try both naming conventions
    let patterns = [
        format!("conversation-{log_number:03}.md"),
        format!("conversation-{log_number}.md"),
        format!("archive-log-{log_number:03}.md"),
        format!("archive-log-{log_number}.md"),
    ];

    for name in &patterns {
        let path = conversations_dir.join(name);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(RecallError::Other(format!(
        "no archive file for log {log_number:03}",
    )))
}

#[cfg(all(test, feature = "llm"))]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_refusal_stops_and_a_write_error_is_not_retried() {
        use crate::graph::error::GraphError;
        use crate::graph::v2::V2_REFUSAL;

        let refusal = GraphError::Extraction(V2_REFUSAL.to_string());
        assert_eq!(
            attempt_after_error(PipelineVersion::V2, &refusal),
            Attempt::Stop
        );
        let write = GraphError::Extraction("database: write failed".into());
        assert_eq!(
            attempt_after_error(PipelineVersion::V2, &write),
            Attempt::LeavePending
        );
        assert_eq!(
            attempt_after_error(PipelineVersion::V1, &refusal),
            Attempt::Retry
        );
        assert_eq!(
            attempt_after_error(PipelineVersion::V1, &write),
            Attempt::Retry
        );
        let v1_into_v2 = GraphError::PipelineRefused(crate::graph::v2::V1_REFUSAL);
        assert_eq!(
            attempt_after_error(PipelineVersion::V1, &v1_into_v2),
            Attempt::Stop
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn extract_pipeline_v1_on_a_v2_store_refuses_and_leaves_the_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        let conversations = memory.join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        let archive = conversations.join("conversation-001.md");
        let body = b"---\nlog: 1\n---\n\n### User\n\nhello\n";
        std::fs::write(&archive, body).unwrap();
        {
            let gm = crate::graph::GraphMemory::open_embedded(&memory.join("graph"))
                .await
                .unwrap();
            crate::graph::v2::ensure_v2_store(&gm).await.unwrap();
        }
        let err = extract(
            memory,
            ExtractOptions {
                log: Some(1),
                all: false,
                dry_run: false,
                model: None,
                provider: None,
                delay_ms: 0,
                max_tokens: 0,
                pipeline: Some(PipelineVersion::V1),
                rebuild: false,
            },
        )
        .await
        .expect_err("refused");
        assert!(err.to_string().contains("stamped pipeline v2"), "{err}");
        assert_eq!(std::fs::read(&archive).unwrap(), body);
    }

    #[test]
    fn v2_counters_appear_on_the_extract_line_and_v1_stays_bare() {
        let v1 = IngestionReport::default();
        assert_eq!(v2_report_suffix(&v1), "");

        let mut v2 = IngestionReport {
            windows: 2,
            llm_calls: 3,
            candidates: 4,
            kept: 3,
            ..IngestionReport::default()
        };
        v2.drops.insert("parse".into(), 1);
        assert_eq!(
            v2_report_suffix(&v2),
            ", 2 windows, 3 calls, 4 candidates, drops parse=1, 3 kept"
        );

        v2.resolve.rejected.push(RejectedMatch {
            mention: "uvicorn".into(),
            match_id: "entity:uv".into(),
        });
        v2.resolve.mentions = 2;
        v2.resolve.resolver_called = 1;
        assert!(
            v2_report_suffix(&v2).contains("rejected uvicorn->entity:uv"),
            "{}",
            v2_report_suffix(&v2)
        );
    }

    /// The line a user reads must not claim a measurement it does not have.
    #[test]
    fn a_bill_says_which_of_its_numbers_were_measured() {
        assert_eq!(format_token_bill(13_663, 0), "14K measured");
        assert_eq!(format_token_bill(0, 2_500), "~2K estimated");
        assert_eq!(
            format_token_bill(13_663, 2_500),
            "14K measured + ~2K estimated"
        );
    }

    /// A run that made no model calls has no bill to qualify.
    #[test]
    fn a_run_that_spent_nothing_says_zero() {
        assert_eq!(format_token_bill(0, 0), "0");
    }

    fn zero_entity_stats(
        unextracted: u64,
        log_number_absent: u64,
    ) -> crate::graph::types::GraphStats {
        crate::graph::types::GraphStats {
            episode_count: 5118,
            unextracted_log_count: unextracted,
            log_number_absent,
            ..Default::default()
        }
    }

    /// Work pending: the wait message, with the size of the wait.
    #[test]
    fn a_pending_store_explains_the_wait_and_its_size() {
        let text = zero_entity_explanation(&zero_entity_stats(1900, 0), true, 120);
        assert!(text.contains("No entities yet"), "{text}");
        assert!(text.contains("1900 archives are pending"), "{text}");
        assert!(text.contains("extract --all"), "{text}");
    }

    /// Episodes exist, the scan is empty: waiting can never help, and the
    /// old message must not promise that it will.
    #[test]
    fn an_unscannable_store_reports_breakage_not_patience() {
        let text = zero_entity_explanation(&zero_entity_stats(0, 5118), true, 120);
        assert!(text.contains("Inconsistent store"), "{text}");
        assert!(!text.contains("No entities yet"), "{text}");
        assert!(
            text.contains("episodes missing a log_number:       5118"),
            "{text}"
        );
        assert!(text.contains("ingest-all"), "{text}");
        // The extracted-flag count is zero here and must not be listed: after
        // RE-44 a zero is the expected state, not a diagnosis.
        assert!(!text.contains("missing the extracted flag"), "{text}");
    }

    /// A migration that did not reach every episode is its own breakage, told
    /// apart from every other one and given the only remedy that works.
    #[test]
    fn an_unmigrated_store_names_the_missing_extracted_flag() {
        let text = unmigrated_episodes_warning(12);

        assert!(text.contains("Unfinished schema migration"), "{text}");
        assert!(
            text.contains("12 episodes have no extracted flag"),
            "{text}"
        );
        assert!(text.contains("graph migrate --force"), "{text}");
        // `ingest-all` skips every archive that already has an episode —
        // exactly this set — so it must not be offered as the fix.
        assert!(!text.contains("ingest-all"), "{text}");
    }

    /// One episode reads as one episode.
    #[test]
    fn the_unmigrated_warning_agrees_with_itself_about_number() {
        let one = unmigrated_episodes_warning(1);
        assert!(one.contains("1 episode has no extracted flag"), "{one}");
        assert!(one.contains("cannot see it at all"), "{one}");

        let many = unmigrated_episodes_warning(2);
        assert!(many.contains("2 episodes have no extracted flag"), "{many}");
        assert!(many.contains("cannot see them at all"), "{many}");
    }

    /// The zero-entity explanation no longer owns this diagnosis: it is
    /// printed on every status, so repeating it here would double it.
    #[test]
    fn the_zero_entity_explanation_leaves_the_migration_alone() {
        let mut stats = zero_entity_stats(0, 0);
        stats.extracted_absent = 12;

        let text = zero_entity_explanation(&stats, true, 120);
        assert!(!text.contains("extracted flag"), "{text}");
        assert!(!text.contains("migrate"), "{text}");
    }

    /// A store where every archive was extracted and none yielded entities is
    /// a legitimate outcome — trivial sessions, or a gc sweep — not a
    /// breakage. It must not be called inconsistent or sent to file a bug.
    #[test]
    fn an_extracted_but_empty_store_is_not_called_broken() {
        let text = zero_entity_explanation(&zero_entity_stats(0, 0), false, 120);
        assert!(!text.contains("Inconsistent"), "{text}");
        assert!(text.contains("produced no entities"), "{text}");
        assert!(text.contains("quarantine"), "{text}");
    }

    #[test]
    fn a_backup_name_gains_a_suffix_instead_of_overwriting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let stamp = "20261025-1530";
        let first = unique_backup(dir.path(), stamp);
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("graph.v1-backup-20261025-1530")
        );
        std::fs::create_dir(&first).expect("first");
        std::fs::create_dir(dir.path().join(format!("graph.v1-backup-{stamp}-2"))).expect("second");
        let third = unique_backup(dir.path(), stamp);
        assert_eq!(
            third.file_name().and_then(|name| name.to_str()),
            Some("graph.v1-backup-20261025-1530-3")
        );
    }

    #[test]
    fn a_failed_second_rename_puts_the_original_store_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let graph = dir.path().join("graph");
        let build = dir.path().join(V2_BUILD_DIR);
        let backup = dir.path().join("graph.v1-backup-20261025-1530");
        std::fs::create_dir(&graph).expect("graph");
        std::fs::write(graph.join("KEEP"), b"old").expect("keep");

        let err = rename_swapping(&graph, &build, &backup).expect_err("missing build");
        assert!(
            err.to_string().contains("original store is in place"),
            "{err}"
        );
        assert_eq!(std::fs::read(graph.join("KEEP")).expect("kept"), b"old");
        assert!(!backup.exists());
        assert!(!build.exists());
    }

    #[test]
    fn both_renames_land_on_the_memory_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let graph = dir.path().join("graph");
        let build = dir.path().join(V2_BUILD_DIR);
        let backup = dir.path().join("graph.v1-backup-20261025-1530");
        std::fs::create_dir(&graph).expect("graph");
        std::fs::write(graph.join("KEEP"), b"old").expect("keep");
        std::fs::create_dir(&build).expect("build");
        std::fs::write(build.join("NEW"), b"new").expect("new");

        rename_swapping(&graph, &build, &backup).expect("swap");
        assert_eq!(std::fs::read(graph.join("NEW")).expect("new store"), b"new");
        assert_eq!(std::fs::read(backup.join("KEEP")).expect("backup"), b"old");
        assert!(!build.exists());
    }

    #[test]
    fn an_archive_listing_follows_log_number() {
        let dir = tempfile::tempdir().expect("temp dir");
        let conversations = dir.path().join("conversations");
        std::fs::create_dir(&conversations).expect("conversations");
        for name in [
            "notes.md",
            "conversation-010.md",
            "archive-log-003.md",
            "conversation-002.md",
        ] {
            std::fs::write(conversations.join(name), "x").expect("archive");
        }
        assert_eq!(
            conversation_logs(&conversations).expect("logs"),
            vec![2, 3, 10]
        );
    }

    #[test]
    fn a_dry_run_estimate_is_one_call_per_window_plus_the_resolver_bound() {
        let dir = tempfile::tempdir().expect("temp dir");
        let memory = dir.path();
        std::fs::create_dir(memory.join("conversations")).expect("conversations");
        std::fs::write(
            memory.join("conversations/conversation-001.md"),
            "---\nlog: 1\ndate: \"2026-03-01T00:00:00Z\"\nsession_id: \"s1\"\n---\n\n\
             ### User\n\nUse Rust for the shelf service.\n",
        )
        .expect("markdown");
        let markdown = crate::graph::v2::estimate_session(memory, 1).expect("markdown estimate");
        assert!(!markdown.has_sidecar);
        assert_eq!(markdown.windows, 1);
        assert_eq!(
            markdown.calls,
            markdown
                .windows
                .saturating_add(crate::graph::v2::RESOLVER_CALL_BOUND)
        );

        std::fs::create_dir(memory.join("turns")).expect("turns");
        std::fs::write(
            memory.join("turns/conversation-002.json"),
            r#"{"version":1,"session_id":"s2","date":"2026-03-01T00:00:00Z","cwd":null,"project":{"name":"shelf","aliases":[]},"turns":[],"tools":[]}"#,
        )
        .expect("empty sidecar");
        let empty = crate::graph::v2::estimate_session(memory, 2).expect("empty estimate");
        assert!(empty.has_sidecar);
        assert_eq!(empty.windows, 0);
        assert_eq!(empty.calls, crate::graph::v2::RESOLVER_CALL_BOUND);
    }

    #[test]
    fn a_swap_report_quotes_paths_and_sends_the_pipeline_back_to_v1() {
        let memory = PathBuf::from("/tmp/memory");
        let backup = memory.join("graph.v1-backup-20261025-1200");
        let text = swap_report(&memory, &backup, 3, 4);
        assert!(
            text.contains("mv '/tmp/memory/graph' '/tmp/memory/graph.v2-build'"),
            "{text}"
        );
        assert!(
            text.contains("mv '/tmp/memory/graph.v1-backup-20261025-1200' '/tmp/memory/graph'"),
            "{text}"
        );
        assert!(text.contains("pipeline = \"v1\""), "{text}");
        assert!(text.contains("symlinked graph/"), "{text}");
        assert!(text.contains("real directory in memory/"), "{text}");
    }

    #[test]
    fn a_half_swap_message_quotes_both_renames_and_both_pipeline_lines() {
        let graph = PathBuf::from("/tmp/memory/graph");
        let build = PathBuf::from("/tmp/memory/graph.v2-build");
        let backup = PathBuf::from("/tmp/memory/graph.v1-backup-20261025-1200");
        let text = half_swap_message(&graph, &build, &backup);
        assert!(
            text.contains("mv '/tmp/memory/graph.v2-build' '/tmp/memory/graph'"),
            "{text}"
        );
        assert!(
            text.contains("mv '/tmp/memory/graph.v1-backup-20261025-1200' '/tmp/memory/graph'"),
            "{text}"
        );
        assert!(text.contains("pipeline = \"v2\""), "{text}");
        assert!(text.contains("pipeline = \"v1\""), "{text}");
        assert!(
            text.contains("nothing was finished automatically"),
            "{text}"
        );
    }

    #[test]
    fn a_zero_chunk_log_is_complete_and_a_short_log_is_not() {
        assert!(log_is_complete(None, 0));
        assert!(!log_is_complete(None, 2));
        let short = EpisodeRows {
            count: 1,
            all_extracted: true,
        };
        assert!(!log_is_complete(Some(&short), 2));
        let full = EpisodeRows {
            count: 2,
            all_extracted: true,
        };
        assert!(log_is_complete(Some(&full), 2));
        let pending = EpisodeRows {
            count: 2,
            all_extracted: false,
        };
        assert!(!log_is_complete(Some(&pending), 2));
    }

    #[test]
    fn episode_rows_are_grouped_once() {
        let rows = [
            serde_json::json!({"log_number": 1, "extracted": true}),
            serde_json::json!({"log_number": 1, "extracted": false}),
            serde_json::json!({"log_number": 2, "extracted": true}),
        ];
        let grouped = group_episodes(&rows);
        assert_eq!(grouped.get(&1).expect("log 1").count, 2);
        assert!(!grouped.get(&1).expect("log 1").all_extracted);
        assert!(grouped.get(&2).expect("log 2").all_extracted);
        assert_eq!(grouped.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_attempt_is_retried_once() {
        let mut calls = 0u32;
        let err = retry_once(Err::<(), &str>("first"), true, || {
            calls += 1;
            async { Err::<(), &str>("second") }
        })
        .await
        .expect_err("second failure");
        assert_eq!(err, "second");
        assert_eq!(calls, 1);

        let kept = retry_once(Err::<(), &str>("kept"), false, || async {
            panic!("a non-retry must not call the archive again");
            #[allow(unreachable_code)]
            Ok::<(), &str>(())
        })
        .await
        .expect_err("original");
        assert_eq!(kept, "kept");

        let mut calls = 0u32;
        let value = retry_once(Ok::<u32, &str>(7), true, || {
            calls += 1;
            async { Ok::<u32, &str>(9) }
        })
        .await
        .expect("first success");
        assert_eq!(value, 7);
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_non_v2_build_directory_says_how_to_remove_it() {
        let path = PathBuf::from("/tmp/memory/graph.v2-build");
        let hint = removal_hint(&path);
        assert!(hint.contains(&path.display().to_string()), "{hint}");
        assert!(hint.contains("rm -r"), "{hint}");
        assert!(hint.contains("stamped v2"), "{hint}");
    }
}
