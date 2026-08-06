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
use crate::graph::{IngestContext, Provenance};
use crate::serve::{
    AddEntityArgs, CorrectArgs, IngestArchiveArgs, QueryArgs, RelateArgs, Request, SearchArgs,
    TraverseArgs,
};
use crate::serve_client;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Initialize the graph store at {memory_dir}/graph/.
pub async fn init(memory_dir: &Path) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    serve_client::exclusive(memory_dir, |_graph| async { Ok(()) }).await?;
    println!(
        "{GREEN}✓{RESET} Graph store initialized at {}",
        graph_dir.display()
    );
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
    // explaining — the wait, or the opt-out, is the answer either way.
    if stats.episode_count > 0 && stats.entity_count == 0 {
        let extraction = crate::config::load_from_dir(memory_dir).extraction;
        println!(
            "\n  {YELLOW}No entities yet.{RESET} Episodes are ingested automatically; turning"
        );
        println!("  them into entities is a separate LLM pass.");
        if extraction.background_enabled {
            println!(
                "  The daemon runs it after {}s of quiet. To run it now:",
                extraction.idle_after_secs
            );
        }
        println!("    {DIM}recall-echo graph extract --all{RESET}");
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
            println!("  State:  {GREEN}running{RESET}");
            println!("  Pid:    {}", info.pid);
            println!("  Version: {}", info.version);
            println!("  Uptime: {}s", info.uptime_secs);
            print_extraction_lines(&info.extraction);
        }
        None if serve_client::graph_mode(memory_dir) == "server" => {
            println!("  State:  not used — [graph] mode = server");
        }
        None => println!("  State:  {YELLOW}not running{RESET}"),
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
        println!("  Extraction: {YELLOW}off{RESET} — {DIM}{reason}{RESET}");
        return;
    }
    match status.last_run_secs_ago {
        Some(secs) => println!(
            "  Extraction: {GREEN}on{RESET} — {} archives in {} runs, last {}s ago ({}ms)",
            status.archives,
            status.runs,
            secs,
            status.last_run_ms.unwrap_or(0)
        ),
        None => println!("  Extraction: {GREEN}on{RESET} — nothing extracted yet"),
    }
    if let Some(error) = &status.last_error {
        println!("  {DIM}Last extraction error: {error}{RESET}");
    }
}

/// Stop the daemon serving this graph, if one is running.
pub async fn daemon_stop(memory_dir: &Path) -> Result<(), RecallError> {
    if serve_client::stop_daemon(memory_dir).await? {
        println!("{GREEN}✓{RESET} Graph daemon stopped");
    } else {
        println!("{YELLOW}No graph daemon running.{RESET}");
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
        "{GREEN}✓{RESET} Created entity: {BOLD}{}{RESET} ({}) [{}]",
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
        "{GREEN}✓{RESET} {from} {CYAN}—[{rel_type}]→{RESET} {to} [{}]",
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
        println!("{YELLOW}No results.{RESET}");
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

/// Ingest a single archive file into the graph (episodes only, no LLM extraction).
///
/// `provenance` forces an authorship class on every episode of the run — how
/// `--external` marks genuinely external material. `None` infers per chunk
/// from conversation turn roles.
pub async fn ingest(
    memory_dir: &Path,
    archive_path: &Path,
    provenance: Option<Provenance>,
) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let content = std::fs::read_to_string(archive_path)?;

    // Extract session_id and log_number from frontmatter if available
    let (session_id, log_number) = extract_archive_metadata(&content, archive_path);

    let request = Request::IngestArchive(IngestArchiveArgs {
        content,
        session_id,
        log_number,
        provenance,
    });
    let report: IngestionReport =
        serde_json::from_value(serve_client::execute(memory_dir, &request).await?)?;

    println!(
        "{GREEN}✓{RESET} Ingested {}: {} episodes created {DIM}(provenance: {}){RESET}",
        archive_path.display(),
        report.episodes_created,
        provenance_label(provenance)
    );
    if !report.errors.is_empty() {
        for err in &report.errors {
            println!("  {YELLOW}warning:{RESET} {err}");
        }
    }
    Ok(())
}

/// How an ingest run's provenance choice reads in its summary line.
fn provenance_label(provenance: Option<Provenance>) -> &'static str {
    match provenance {
        Some(Provenance::External) => "external",
        Some(Provenance::User) => "user",
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
        println!("{YELLOW}No conversation archives found.{RESET}");
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
                "  {GREEN}✓{RESET} {} — {} episodes",
                path.file_name().unwrap_or_default().to_string_lossy(),
                report.episodes_created
            );
        }

        println!(
            "\n{GREEN}✓{RESET} Ingested {ingested} archives ({total_episodes} episodes), skipped {skipped} already ingested {DIM}(provenance: {}){RESET}",
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
        println!("{YELLOW}No results.{RESET}");
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
    quarantined: Vec<u32>,
    dedup_llm_calls: u32,
    dedup_fast_path: u32,
}

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
    println!(
        "\n{GREEN}✓{RESET} Done: {} archives — +{} created, ~{} merged, -{} skipped, {} relationships",
        totals.processed,
        totals.entities_created,
        totals.entities_merged,
        totals.entities_skipped,
        totals.relationships,
    );
    println!(
        "  Estimated tokens: ~{}",
        format_tokens(totals.estimated_tokens)
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
            "  {YELLOW}Quarantined: {} archives{RESET}",
            totals.quarantined.len()
        );
    }

    if !totals.errors.is_empty() {
        println!("\n{YELLOW}Warnings ({}):{RESET}", totals.errors.len());
        for err in totals.errors.iter().take(10) {
            println!("  {DIM}{err}{RESET}");
        }
        if totals.errors.len() > 10 {
            println!("  {DIM}... and {} more{RESET}", totals.errors.len() - 10);
        }
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

/// Extract entities from already-ingested archives using an LLM.
#[cfg(feature = "llm")]
#[allow(clippy::too_many_arguments)]
pub async fn extract(
    memory_dir: &Path,
    log: Option<u32>,
    all: bool,
    dry_run: bool,
    model_override: Option<String>,
    provider_override: Option<String>,
    delay_ms: u64,
    max_tokens: u64,
) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    serve_client::exclusive(memory_dir, |gm| async move {
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
            println!("{YELLOW}No unextracted archives found.{RESET}");
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

        for (idx, ln) in log_numbers.iter().enumerate() {
            // Budget check
            if max_tokens > 0 && totals.estimated_tokens >= max_tokens {
                println!(
                    "\n{YELLOW}⚠ Token budget exhausted (~{} / {}). Stopping.{RESET}",
                    format_tokens(totals.estimated_tokens),
                    format_tokens(max_tokens),
                );
                println!("  Re-run to continue — resume is automatic via unextracted log numbers.");
                break;
            }

            let archive_path = match find_archive_file(&conversations_dir, *ln) {
                Ok(p) => p,
                Err(e) => {
                    println!(
                        "  {YELLOW}⚠{RESET} [{}/{}] log {ln:03}: {e}",
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

            // Try extraction, retry once on failure, quarantine on second failure
            let report = match gm.extract_from_archive(&content, &context, &*llm).await {
                Ok(r) => r,
                Err(e) => {
                    println!(
                        "  {YELLOW}⚠{RESET} [{}/{}] log {ln:03}: failed, retrying... ({e})",
                        idx + 1,
                        total_count
                    );
                    match gm.extract_from_archive(&content, &context, &*llm).await {
                        Ok(r) => r,
                        Err(e2) => {
                            println!(
                                "  {YELLOW}✗{RESET} [{}/{}] log {ln:03}: quarantined ({e2})",
                                idx + 1,
                                total_count
                            );
                            totals.quarantined.push(*ln);
                            totals
                                .errors
                                .push(format!("log {ln:03}: quarantined after retry: {e2}"));
                            continue;
                        }
                    }
                }
            };

            println!(
                "  {GREEN}✓{RESET} [{}/{}] log {ln:03}: +{} entities, ~{} merged, -{} skipped, {} rels (~{})",
                idx + 1,
                total_count,
                report.entities_created,
                report.entities_merged,
                report.entities_skipped,
                report.relationships_created,
                format_tokens(report.estimated_tokens),
            );

            gm.mark_extracted(*ln).await?;

            totals.entities_created += report.entities_created;
            totals.entities_merged += report.entities_merged;
            totals.entities_skipped += report.entities_skipped;
            totals.relationships += report.relationships_created;
            totals.errors.extend(report.errors);
            totals.processed += 1;
            totals.estimated_tokens += report.estimated_tokens;
            totals.dedup_llm_calls += report.dedup_llm_calls;
            totals.dedup_fast_path += report.dedup_fast_path;

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
                "\n  {YELLOW}Quarantined {} archives → {}{RESET}",
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

    // Default paths: look for vigil/ and caliber/ relative to memory_dir's parent (entity root)
    let entity_root = memory_dir.parent().unwrap_or(memory_dir);

    let default_signals = entity_root.join("vigil").join("signals.json");
    let default_outcomes = entity_root.join("caliber").join("outcomes.json");

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
            println!("\n  {YELLOW}Warnings:{RESET}");
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
        println!("\n  {YELLOW}Warnings:{RESET}");
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
                println!("\n  {CYAN}{}{RESET}", stage.to_uppercase());
                let mut items: Vec<_> = statuses.iter().collect();
                items.sort_by_key(|(s, _)| (*s).clone());
                for (status, count) in items {
                    println!("    {status}: {count}");
                }
            }
        }

        if !stats.stale_thoughts.is_empty() {
            println!("\n  {YELLOW}Stale thoughts (>{staleness_days}d):{RESET}");
            for entity in &stats.stale_thoughts {
                println!("    {DIM}•{RESET} {}", entity.name);
            }
        }

        if !stats.stale_questions.is_empty() {
            println!(
                "\n  {YELLOW}Stale questions (>{}d):{RESET}",
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
            println!("{YELLOW}No pipeline relationships found for \"{entity_name}\".{RESET}");
            return Ok(());
        }

        println!("{BOLD}Pipeline Flow: {entity_name}{RESET}\n");
        for (source, rel_type, target) in &chain {
            println!(
                "  {} ({}) {CYAN}—[{rel_type}]→{RESET} {} ({})",
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
            println!("{GREEN}✓{RESET} No stale pipeline entities.");
            return Ok(());
        }

        println!("{BOLD}Stale Pipeline Entities{RESET}\n");

        if !stats.stale_thoughts.is_empty() {
            println!("  {YELLOW}Thoughts (>{staleness_days} days):{RESET}");
            for entity in &stats.stale_thoughts {
                println!("    • {} {DIM}({}){RESET}", entity.name, entity.entity_type);
            }
        }

        if !stats.stale_questions.is_empty() {
            println!("  {YELLOW}Questions (>{} days):{RESET}", staleness_days * 2);
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
                "{BOLD}{YELLOW}GC Dry Run{RESET} {DIM}(pass --execute to actually delete){RESET}"
            );
        } else {
            println!("{BOLD}{GREEN}GC Executed{RESET}");
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
                    GcActionKind::StaleRelationship => format!("{YELLOW}⚠{RESET}"),
                    GcActionKind::DeadRelationship => format!("{YELLOW}✗{RESET}"),
                    GcActionKind::OrphanedEntity => format!("{CYAN}○{RESET}"),
                    GcActionKind::SpentEpisode => format!("{CYAN}◌{RESET}"),
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
                println!("  \x1b[31m✗\x1b[0m {err}");
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
            "{YELLOW}No entities recorded for session {session_id}.{RESET} \
             {DIM}Nothing to apply the outcome to.{RESET}"
        );
        return Ok(());
    }

    println!(
        "{GREEN}✓{RESET} Session {BOLD}{session_id}{RESET} recorded as {BOLD}{outcome}{RESET} \
         — {} entities updated",
        report.entities_updated
    );

    for entity in &report.utilities {
        println!(
            "  {DIM}{}{RESET} utility {CYAN}{:.3}{RESET}",
            entity.entity_id, entity.utility_score
        );
    }

    if !report.errors.is_empty() {
        println!("\n{YELLOW}Warnings:{RESET}");
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
                println!("{YELLOW}Nothing removed.{RESET}");
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
            println!("{YELLOW}No entity named{RESET} {BOLD}{query}{RESET}.");
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
                "{YELLOW}Memory holds no{RESET} {BOLD}{rel_type}{RESET} \
                 {YELLOW}relationship between{RESET} {BOLD}{from}{RESET} \
                 {YELLOW}and{RESET} {BOLD}{to}{RESET}."
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
                "{GREEN}✓{RESET} Removed {entity}{} {}.",
                removal.edges.len(),
                plural(removal.edges.len(), "relationship", "relationships")
            );
        }
    }
}

fn print_contradictions(edges: &[EdgeCorrection]) {
    println!(
        "{GREEN}✓{RESET} Recorded your correction on {} {}.\n",
        edges.len(),
        plural(edges.len(), "relationship", "relationships")
    );
    for correction in edges {
        let edge = &correction.edge;
        println!(
            "  {} {CYAN}—[{}]→{RESET} {}",
            edge.from, edge.rel_type, edge.to
        );
        println!(
            "    confidence {YELLOW}{:.2} → {:.2}{RESET}   {DIM}evidence {:.1} → {:.1}{RESET}",
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
    println!("{BOLD}{YELLOW}This would remove:{RESET}\n");
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
            format!(" {YELLOW}self×{}{RESET}", edge.self_reinforcements)
        } else {
            String::new()
        };
        println!(
            "    {} {CYAN}—[{}]→{RESET} {}  {:.0}%{coherence}{superseded}",
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
            println!("{YELLOW}No relationships found.{RESET}");
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
                format!("\x1b[31m↓{:.0}%\x1b[0m", decay_amount * 100.0)
            } else if decay_amount > 0.05 {
                format!("{YELLOW}↓{:.0}%{RESET}", decay_amount * 100.0)
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
                "  {from_short} {CYAN}—[{}]→{RESET} {to_short}  stored:{:.2} effective:{:.2} {decay_indicator}{evidence_tag}{reinforced_tag}",
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
