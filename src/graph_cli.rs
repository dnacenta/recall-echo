//! Graph memory CLI subcommands (behind `graph` feature flag).

use std::path::Path;

use recall_graph::traverse::format_traversal;
use recall_graph::types::*;
use recall_graph::GraphMemory;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Initialize the graph store at {memory_dir}/graph/.
pub fn init(memory_dir: &Path) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;
        println!(
            "{GREEN}✓{RESET} Graph store initialized at {}",
            graph_dir.display()
        );
        Ok(())
    })
}

/// Show graph stats.
pub fn graph_status(memory_dir: &Path) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err("Graph store not initialized. Run `recall-echo graph init` first.".into());
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;
        let stats = gm.stats().await.map_err(|e| e.to_string())?;

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
    })
}

/// Add an entity to the graph.
pub fn add_entity(
    memory_dir: &Path,
    name: &str,
    entity_type: &str,
    abstract_text: &str,
    overview: Option<&str>,
    source: Option<&str>,
) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    let et: EntityType = entity_type.parse().map_err(|e: String| e)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

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
            .await
            .map_err(|e| e.to_string())?;

        println!(
            "{GREEN}✓{RESET} Created entity: {BOLD}{}{RESET} ({}) [{}]",
            entity.name,
            entity.entity_type,
            entity.id_string()
        );
        Ok(())
    })
}

/// Create a relationship between two entities.
pub fn relate(
    memory_dir: &Path,
    from: &str,
    rel_type: &str,
    to: &str,
    description: Option<&str>,
    source: Option<&str>,
) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

        let rel = gm
            .add_relationship(NewRelationship {
                from_entity: from.to_string(),
                to_entity: to.to_string(),
                rel_type: rel_type.to_string(),
                description: description.map(String::from),
                confidence: None,
                source: source.map(String::from),
            })
            .await
            .map_err(|e| e.to_string())?;

        println!(
            "{GREEN}✓{RESET} {from} {CYAN}—[{rel_type}]→{RESET} {to} [{}]",
            rel.id_string()
        );
        Ok(())
    })
}

/// Semantic search across entities.
pub fn search(
    memory_dir: &Path,
    query: &str,
    limit: usize,
    entity_type: Option<&str>,
    keyword: Option<&str>,
) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

        let options = SearchOptions {
            limit,
            entity_type: entity_type.map(String::from),
            keyword: keyword.map(String::from),
        };

        let results = gm
            .search_with_options(query, &options)
            .await
            .map_err(|e| e.to_string())?;

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
    })
}

/// Ingest a single archive file into the graph (episodes only, no LLM extraction).
pub fn ingest(memory_dir: &Path, archive_path: &Path) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err("Graph store not initialized. Run `recall-echo graph init` first.".into());
    }

    let content = std::fs::read_to_string(archive_path)
        .map_err(|e| format!("Failed to read {}: {e}", archive_path.display()))?;

    // Extract session_id and log_number from frontmatter if available
    let (session_id, log_number) = extract_archive_metadata(&content, archive_path);

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

        let report = gm
            .ingest_archive(&content, &session_id, log_number, None)
            .await
            .map_err(|e| e.to_string())?;

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
    })
}

/// Ingest all un-ingested archives in conversations/.
pub fn ingest_all(memory_dir: &Path) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    if !graph_dir.exists() {
        return Err("Graph store not initialized. Run `recall-echo graph init` first.".into());
    }

    // Check both memory_dir/conversations/ and parent/conversations/ (Claude Code layout)
    let conversations_dir = memory_dir.join("conversations");
    let conversations_dir = if conversations_dir.exists() {
        conversations_dir
    } else if let Some(parent) = memory_dir.parent() {
        let parent_conv = parent.join("conversations");
        if parent_conv.exists() {
            parent_conv
        } else {
            return Err("conversations/ directory not found.".into());
        }
    } else {
        return Err("conversations/ directory not found.".into());
    };

    // Collect all conversation files, sorted
    let mut files: Vec<_> = std::fs::read_dir(&conversations_dir)
        .map_err(|e| e.to_string())?
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

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

        let mut total_episodes = 0u32;
        let mut ingested = 0u32;
        let mut skipped = 0u32;

        for entry in &files {
            let path = entry.path();
            let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;

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
                .await
                .map_err(|e| e.to_string())?;

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
    })
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
pub fn traverse(
    memory_dir: &Path,
    entity_name: &str,
    depth: u32,
    type_filter: Option<&str>,
) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

        let tree = gm
            .traverse_filtered(entity_name, depth, type_filter)
            .await
            .map_err(|e| e.to_string())?;

        let output = format_traversal(&tree, 0);
        print!("{output}");
        Ok(())
    })
}

/// Hybrid query: semantic + graph expansion + optional episodes.
pub fn hybrid_query(
    memory_dir: &Path,
    query: &str,
    limit: usize,
    entity_type: Option<&str>,
    keyword: Option<&str>,
    depth: u32,
    episodes: bool,
) -> Result<(), String> {
    let graph_dir = memory_dir.join("graph");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let gm = GraphMemory::open(&graph_dir)
            .await
            .map_err(|e| e.to_string())?;

        let options = QueryOptions {
            limit,
            entity_type: entity_type.map(String::from),
            keyword: keyword.map(String::from),
            graph_depth: depth,
            include_episodes: episodes,
        };

        let result = gm.query(query, &options).await.map_err(|e| e.to_string())?;

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
    })
}
