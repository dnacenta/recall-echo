//! Graph memory CLI subcommands (behind `graph` feature flag).

use std::path::{Path, PathBuf};

use crate::error::RecallError;
use crate::graph::traverse::format_traversal;
use crate::graph::types::*;
use crate::graph::GraphMemory;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Initialize the graph store at {memory_dir}/graph/.
pub async fn init(memory_dir: &Path) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    GraphMemory::open(&graph_dir).await?;
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
    let gm = GraphMemory::open(&graph_dir).await?;
    let stats = gm.stats().await?;

    println!("{BOLD}Graph Memory Status{RESET}");
    println!("  Entities:      {}", stats.entity_count);
    println!("  Relationships: {}", stats.relationship_count);
    println!("  Episodes:      {}", stats.episode_count);

    if !stats.entity_type_counts.is_empty() {
        println!("\n  {DIM}By type:{RESET}");
        let mut types: Vec<_> = stats.entity_type_counts.iter().collect();
        types.sort_by(|a, b| b.1.cmp(a.1));
        for (t, count) in types {
            println!("    {t}: {count}");
        }
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
    let graph_dir = memory_dir.join("graph");
    let et: EntityType = entity_type
        .parse()
        .map_err(|e: String| RecallError::Other(e))?;

    let gm = GraphMemory::open(&graph_dir).await?;

    let entity = gm
        .add_entity(NewEntity {
            name: name.to_string(),
            entity_type: et,
            abstract_text: abstract_text.to_string(),
            overview: overview.map(String::from),
            content: None,
            attributes: None,
            source: source.map(String::from),
        })
        .await?;

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
    let graph_dir = memory_dir.join("graph");
    let gm = GraphMemory::open(&graph_dir).await?;

    let rel = gm
        .add_relationship(NewRelationship {
            from_entity: from.to_string(),
            to_entity: to.to_string(),
            rel_type: rel_type.to_string(),
            description: description.map(String::from),
            confidence: None,
            source: source.map(String::from),
        })
        .await?;

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
    let graph_dir = memory_dir.join("graph");
    let gm = GraphMemory::open(&graph_dir).await?;

    let options = SearchOptions {
        limit,
        entity_type: entity_type.map(String::from),
        keyword: keyword.map(String::from),
    };

    let results = gm.search_with_options(query, &options).await?;

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
pub async fn ingest(memory_dir: &Path, archive_path: &Path) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let content = std::fs::read_to_string(archive_path)?;

    // Extract session_id and log_number from frontmatter if available
    let (session_id, log_number) = extract_archive_metadata(&content, archive_path);

    let gm = GraphMemory::open(&graph_dir).await?;

    let report = gm
        .ingest_archive(&content, &session_id, log_number, None)
        .await?;

    println!(
        "{GREEN}✓{RESET} Ingested {}: {} episodes created",
        archive_path.display(),
        report.episodes_created
    );
    if !report.errors.is_empty() {
        for err in &report.errors {
            println!("  {YELLOW}warning:{RESET} {err}");
        }
    }
    Ok(())
}

/// Ingest all un-ingested archives in conversations/.
pub async fn ingest_all(memory_dir: &Path) -> Result<(), RecallError> {
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

    let gm = GraphMemory::open(&graph_dir).await?;

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

        let report = gm
            .ingest_archive(&content, &session_id, log_number, None)
            .await?;

        total_episodes += report.episodes_created;
        ingested += 1;

        println!(
            "  {GREEN}✓{RESET} {} — {} episodes",
            path.file_name().unwrap_or_default().to_string_lossy(),
            report.episodes_created
        );
    }

    println!(
        "\n{GREEN}✓{RESET} Ingested {ingested} archives ({total_episodes} episodes), skipped {skipped} already ingested"
    );
    Ok(())
}

/// Extract session_id and log_number from a conversation archive's frontmatter.
fn extract_archive_metadata(content: &str, path: &Path) -> (String, Option<u32>) {
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
    let graph_dir = memory_dir.join("graph");
    let gm = GraphMemory::open(&graph_dir).await?;

    let tree = gm
        .traverse_filtered(entity_name, depth, type_filter)
        .await?;

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
    let graph_dir = memory_dir.join("graph");
    let gm = GraphMemory::open(&graph_dir).await?;

    let options = QueryOptions {
        limit,
        entity_type: entity_type.map(String::from),
        keyword: keyword.map(String::from),
        graph_depth: depth,
        include_episodes: episodes,
    };

    let result = gm.query(query, &options).await?;

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

    let gm = GraphMemory::open(&graph_dir).await?;

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
    println!(
        "{BOLD}Extracting entities from {total_count} archives using {model_name}{budget_label}{RESET}",
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

        // Try extraction, retry once on failure, quarantine on second failure
        let report = match gm
            .extract_from_archive(&content, &session_id, Some(*ln), &*llm)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                println!(
                    "  {YELLOW}⚠{RESET} [{}/{}] log {ln:03}: failed, retrying... ({e})",
                    idx + 1,
                    total_count
                );
                match gm
                    .extract_from_archive(&content, &session_id, Some(*ln), &*llm)
                    .await
                {
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

    let gm = GraphMemory::open(&graph_dir).await?;

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

    let gm = GraphMemory::open(&graph_dir).await?;

    let report = gm.sync_pipeline(&docs).await?;

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

    let gm = GraphMemory::open(&graph_dir).await?;

    let stats = gm.pipeline_stats(staleness_days).await?;

    println!(
        "{BOLD}Pipeline Status{RESET} ({} entities)",
        stats.total_entities
    );

    if stats.by_stage.is_empty() {
        println!("  {DIM}No pipeline entities in graph. Run `graph pipeline sync` first.{RESET}");
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
}

/// Trace pipeline flow for an entity.
pub async fn pipeline_flow(memory_dir: &Path, entity_name: &str) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let gm = GraphMemory::open(&graph_dir).await?;

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
}

/// List stale pipeline entities.
pub async fn pipeline_stale(memory_dir: &Path, staleness_days: u32) -> Result<(), RecallError> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let gm = GraphMemory::open(&graph_dir).await?;

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
fn find_conversations_dir(memory_dir: &Path) -> Result<PathBuf, RecallError> {
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

/// Run garbage collection on the graph.
#[allow(clippy::too_many_arguments)]
pub async fn gc(
    memory_dir: &Path,
    execute: bool,
    stale_days: u64,
    stale_confidence: f64,
    dead_confidence: f64,
    dead_min_age_days: u64,
    stats_only: bool,
) -> Result<(), RecallError> {
    use crate::graph::gc::{GcActionKind, GcConfig};

    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err(RecallError::NotInitialized(
            "Graph store not initialized. Run `recall-echo graph init` first.".into(),
        ));
    }

    let gm = GraphMemory::open(&graph_dir).await?;

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

    let config = GcConfig {
        stale_days,
        stale_confidence,
        dead_confidence,
        dead_min_age_days,
        dry_run: !execute,
        protect_pipeline: true,
    };

    let report = gm.run_gc(&config).await?;

    // Header
    if report.dry_run {
        println!("{BOLD}{YELLOW}GC Dry Run{RESET} {DIM}(pass --execute to actually delete){RESET}");
    } else {
        println!("{BOLD}{GREEN}GC Executed{RESET}");
    }

    println!("\n{BOLD}Scan{RESET}");
    println!("  Entities scanned:      {}", report.entities_scanned);
    println!("  Relationships scanned: {}", report.relationships_scanned);

    println!("\n{BOLD}Results{RESET}");
    println!("  Stale relationships:   {}", report.stale_relationships);
    println!("  Dead relationships:    {}", report.dead_relationships);
    println!("  Orphaned entities:     {}", report.orphaned_entities);

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

    let gm = GraphMemory::open(&graph_dir).await?;

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

        println!(
            "  {from_short} {CYAN}—[{}]→{RESET} {to_short}  stored:{:.2} effective:{:.2} {decay_indicator}{reinforced_tag}",
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
}

/// Find the archive file for a given log number.
#[cfg(feature = "llm")]
fn find_archive_file(conversations_dir: &Path, log_number: u32) -> Result<PathBuf, RecallError> {
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
