use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[cfg(feature = "graph")]
use recall_echo::graph_cli;
use recall_echo::{
    archive, checkpoint, config_cli, dashboard, distill, init, paths, search, status, RecallEcho,
};

#[derive(Parser)]
#[command(
    name = "recall-echo",
    about = "Persistent memory system with knowledge graph — for any LLM tool",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the memory system for an entity
    Init {
        /// Entity root directory (defaults to current directory)
        entity_root: Option<PathBuf>,
    },
    /// Memory system health check
    Status {
        /// Entity root directory (defaults to current directory)
        entity_root: Option<PathBuf>,
    },
    /// Search conversation archives
    Search {
        /// Search query
        query: String,
        /// Use ranked (file-level) search
        #[arg(long)]
        ranked: bool,
        /// Number of context lines around matches
        #[arg(long, short = 'C', default_value = "0")]
        context: usize,
        /// Maximum results for ranked search
        #[arg(long, default_value = "10")]
        max_results: usize,
    },
    /// Analyze MEMORY.md and suggest distillation
    Distill {
        /// Entity root directory (defaults to current directory)
        entity_root: Option<PathBuf>,
    },
    /// Output EPHEMERAL.md content
    Consume {
        /// Entity root directory (defaults to current directory)
        entity_root: Option<PathBuf>,
    },
    /// Memory dashboard with health, stats, and recent sessions
    Dashboard {
        /// Entity root directory (defaults to current directory)
        entity_root: Option<PathBuf>,
    },
    /// Archive a Claude Code session from JSONL transcript (SessionEnd hook)
    ArchiveSession,
    /// Archive JSONL transcripts
    Archive {
        /// Archive all unarchived JSONL transcripts under ~/.claude/projects/
        #[arg(long)]
        all_unarchived: bool,
    },
    /// Checkpoint during context compaction (PreCompact hook)
    Checkpoint {
        /// Trigger source (e.g., "precompact")
        #[arg(long)]
        trigger: String,
    },
    /// View or modify configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
        /// Entity root directory (defaults to current directory)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// Knowledge graph operations
    #[cfg(feature = "graph")]
    Graph {
        #[command(subcommand)]
        command: GraphCommands,
        /// Entity root directory (defaults to current directory)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Set a config value (e.g., `config set provider ollama`)
    Set {
        /// Config key (provider, model, api_base, llm.provider, ephemeral.max_entries)
        key: String,
        /// New value
        value: String,
    },
}

#[cfg(feature = "graph")]
#[derive(Subcommand)]
enum GraphCommands {
    /// Initialize the graph store
    Init,
    /// Show graph statistics
    Status,
    /// Add an entity to the graph
    AddEntity {
        /// Entity name
        #[arg(long)]
        name: String,
        /// Entity type (person, project, tool, service, preference, decision, etc.)
        #[arg(long, rename_all = "snake_case")]
        r#type: String,
        /// Abstract description (used for embedding and search)
        #[arg(long, rename_all = "snake_case")]
        r#abstract: String,
        /// Optional overview
        #[arg(long)]
        overview: Option<String>,
        /// Source identifier
        #[arg(long)]
        source: Option<String>,
    },
    /// Create a relationship between entities
    Relate {
        /// Source entity name
        from: String,
        /// Relationship type (e.g. USES, BUILDS, DEPENDS_ON, WRITTEN_IN)
        #[arg(long)]
        rel: String,
        /// Target entity name
        #[arg(long)]
        target: String,
        /// Description of the relationship
        #[arg(long)]
        description: Option<String>,
        /// Source identifier
        #[arg(long)]
        source: Option<String>,
    },
    /// Semantic search across entities
    Search {
        /// Search query
        query: String,
        /// Maximum results
        #[arg(long, default_value = "5")]
        limit: usize,
        /// Filter by entity type (e.g. tool, project, person)
        #[arg(long, rename_all = "snake_case")]
        r#type: Option<String>,
        /// Filter by keyword in name or abstract
        #[arg(long)]
        keyword: Option<String>,
    },
    /// Traverse the graph from an entity
    Traverse {
        /// Entity name to start from
        entity: String,
        /// Maximum traversal depth
        #[arg(long, default_value = "2")]
        depth: u32,
        /// Filter neighbors by entity type
        #[arg(long)]
        type_filter: Option<String>,
    },
    /// Hybrid query: semantic + graph expansion + optional episodes
    Query {
        /// Search query
        query: String,
        /// Maximum results
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Filter by entity type
        #[arg(long, rename_all = "snake_case")]
        r#type: Option<String>,
        /// Filter by keyword
        #[arg(long)]
        keyword: Option<String>,
        /// Graph expansion depth (0 = semantic only)
        #[arg(long, default_value = "1")]
        depth: u32,
        /// Include episode search results
        #[arg(long)]
        episodes: bool,
    },
    /// Ingest a single archive file into the graph (episodes only, no LLM)
    Ingest {
        /// Path to conversation archive file
        archive: PathBuf,
    },
    /// Scan conversations/ for un-ingested archives and ingest them all
    IngestAll,
    /// Extract entities from already-ingested archives using an LLM
    #[cfg(feature = "llm")]
    Extract {
        /// Extract from a single archive by log number
        #[arg(long)]
        log: Option<u32>,
        /// Extract from all un-extracted archives
        #[arg(long)]
        all: bool,
        /// Dry run — show what would be extracted without calling the LLM
        #[arg(long)]
        dry_run: bool,
        /// Override model (default from env or claude-haiku-4-5-20251001)
        #[arg(long)]
        model: Option<String>,
        /// Override provider (anthropic or openai)
        #[arg(long)]
        provider: Option<String>,
        /// Milliseconds delay between archives (default: 100)
        #[arg(long, default_value = "100")]
        delay_ms: u64,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        None => status::run(),
        Some(Commands::Init { entity_root }) => {
            let root = resolve_init_root(entity_root);
            init::run(&root)
        }
        Some(Commands::Status { entity_root }) => {
            let root = resolve_entity_root(entity_root);
            status::run_with_base(&root)
        }
        Some(Commands::Search {
            query,
            ranked,
            context,
            max_results,
        }) => {
            if ranked {
                search::run_ranked(&query, max_results)
            } else {
                search::run(&query, context)
            }
        }
        Some(Commands::Distill { entity_root }) => {
            let root = resolve_entity_root(entity_root);
            distill::run_with_base(&root)
        }
        Some(Commands::Consume { entity_root }) => {
            let root = resolve_entity_root(entity_root);
            let ephemeral = root.join("memory").join("EPHEMERAL.md");
            recall_echo::consume::run(&ephemeral)
        }
        Some(Commands::Dashboard { entity_root }) => {
            let root = resolve_entity_root(entity_root);
            let recall = RecallEcho::new(root);
            let version = env!("CARGO_PKG_VERSION");
            dashboard::render(&recall, "echo", version, 200);
            Ok(())
        }
        // JSONL commands (ported from recall-claude)
        Some(Commands::ArchiveSession) => archive::run_from_hook(),
        Some(Commands::Archive { all_unarchived }) => {
            if all_unarchived {
                archive::archive_all_unarchived()
            } else {
                Err("Use --all-unarchived to archive all unarchived JSONL transcripts.".to_string())
            }
        }
        Some(Commands::Checkpoint { trigger }) => checkpoint::run_from_hook(&trigger),
        Some(Commands::Config {
            command,
            entity_root,
        }) => {
            let root = resolve_entity_root(entity_root);
            let memory_dir = root.join("memory");
            match command {
                ConfigCommands::Show => config_cli::show(&memory_dir),
                ConfigCommands::Set { key, value } => config_cli::set(&memory_dir, &key, &value),
            }
        }
        #[cfg(feature = "graph")]
        Some(Commands::Graph {
            command,
            entity_root,
        }) => {
            let root = resolve_entity_root(entity_root);
            let memory_dir = root.join("memory");
            match command {
                GraphCommands::Init => graph_cli::init(&memory_dir),
                GraphCommands::Status => graph_cli::graph_status(&memory_dir),
                GraphCommands::AddEntity {
                    name,
                    r#type,
                    r#abstract,
                    overview,
                    source,
                } => graph_cli::add_entity(
                    &memory_dir,
                    &name,
                    &r#type,
                    &r#abstract,
                    overview.as_deref(),
                    source.as_deref(),
                ),
                GraphCommands::Relate {
                    from,
                    rel,
                    target,
                    description,
                    source,
                } => graph_cli::relate(
                    &memory_dir,
                    &from,
                    &rel,
                    &target,
                    description.as_deref(),
                    source.as_deref(),
                ),
                GraphCommands::Search {
                    query,
                    limit,
                    r#type,
                    keyword,
                } => graph_cli::search(
                    &memory_dir,
                    &query,
                    limit,
                    r#type.as_deref(),
                    keyword.as_deref(),
                ),
                GraphCommands::Traverse {
                    entity,
                    depth,
                    type_filter,
                } => graph_cli::traverse(&memory_dir, &entity, depth, type_filter.as_deref()),
                GraphCommands::Query {
                    query,
                    limit,
                    r#type,
                    keyword,
                    depth,
                    episodes,
                } => graph_cli::hybrid_query(
                    &memory_dir,
                    &query,
                    limit,
                    r#type.as_deref(),
                    keyword.as_deref(),
                    depth,
                    episodes,
                ),
                GraphCommands::Ingest { archive } => graph_cli::ingest(&memory_dir, &archive),
                GraphCommands::IngestAll => graph_cli::ingest_all(&memory_dir),
                #[cfg(feature = "llm")]
                GraphCommands::Extract {
                    log,
                    all,
                    dry_run,
                    model,
                    provider,
                    delay_ms,
                } => graph_cli::extract(&memory_dir, log, all, dry_run, model, provider, delay_ms),
            }
        }
    };

    if let Err(e) = result {
        eprintln!("\x1b[31m\u{2717}\x1b[0m {e}");
        std::process::exit(1);
    }
}

fn resolve_entity_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| paths::entity_root().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Resolve entity root for init, preferring Claude Code directory.
fn resolve_init_root(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    // If RECALL_ECHO_HOME is set, use it
    if let Ok(p) = std::env::var("RECALL_ECHO_HOME") {
        return PathBuf::from(p);
    }
    // If ~/.claude/ exists, use it (Claude Code user)
    if let Some(claude) = paths::detect_claude_code() {
        return claude;
    }
    // Fall back to cwd
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
