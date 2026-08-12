// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    ArchiveSession {
        /// Entity root directory (defaults to ~/.claude for legacy hooks)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// Archive JSONL transcripts
    Archive {
        /// Archive all unarchived JSONL transcripts under ~/.claude/projects/
        #[arg(long)]
        all_unarchived: bool,
    },
    /// Import sessions from an agent CLI's own transcripts (codex, grok, claude-code)
    Ingest {
        /// CLI to import from; repeatable. Omit with --all for every configured CLI
        #[arg(long = "from", value_name = "CLI")]
        from: Vec<String>,
        /// Import from every CLI in [capture] sources, or every one installed
        #[arg(long)]
        all: bool,
        /// Memory directory to import into (defaults to the one hooks write to)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Checkpoint during context compaction (PreCompact hook)
    Checkpoint {
        /// Trigger source (e.g., "precompact")
        #[arg(long)]
        trigger: String,
        /// Entity root directory (defaults to ~/.claude for legacy hooks)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// View or modify configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
        /// Entity root directory (defaults to current directory)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// Run the graph daemon (started automatically by graph commands and hooks)
    Serve {
        /// Memory directory to serve (defaults to {entity_root}/memory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Stay in the foreground: log to stderr and never idle-shut-down
        /// (for systemd units and debugging)
        #[arg(long)]
        foreground: bool,
    },
    /// Run the MCP server (stdio) so an agent can query its own memory
    Mcp {
        /// Entity root directory (defaults to current directory)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// Show what memory holds — everything, or one subject
    WhatDoYouKnow {
        /// Narrow it to one subject
        #[arg(long = "about", value_name = "TOPIC")]
        about: Option<String>,
        /// Entities listed per type (or results, with --about)
        #[arg(long, default_value = "3")]
        limit: usize,
        /// Entity root directory (defaults to current directory)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// Knowledge graph operations
    Graph {
        #[command(subcommand)]
        command: GraphCommands,
        /// Entity root directory (defaults to current directory)
        #[arg(long)]
        entity_root: Option<PathBuf>,
    },
    /// LoCoMo benchmark harness (ingest a conversation, answer a question)
    #[cfg(feature = "bench")]
    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },
}

#[cfg(feature = "bench")]
#[derive(Subcommand)]
enum BenchCommands {
    /// Ingest a single LoCoMo conversation JSON
    Ingest {
        /// Entity root directory
        #[arg(long)]
        entity_root: PathBuf,
        /// Path to a JSON file containing a BenchConversation; omit to read stdin
        #[arg(long)]
        conv_json: Option<PathBuf>,
        /// Override LLM provider for extraction (anthropic, openai, claude-code, gemini, grok, cli)
        #[arg(long)]
        provider: Option<String>,
        /// Override LLM model for extraction
        #[arg(long)]
        model: Option<String>,
        /// Skip the LLM during ingest — episodes only, no entity extraction
        #[arg(long)]
        no_llm: bool,
    },
    /// Answer a question against an already-ingested entity
    Answer {
        /// Entity root directory
        #[arg(long)]
        entity_root: PathBuf,
        /// The question to answer (use `--question-stdin` to read from stdin instead)
        #[arg(long)]
        question: Option<String>,
        /// Read the question from stdin
        #[arg(long)]
        question_stdin: bool,
        /// Override LLM provider (anthropic, openai, claude-code, gemini, grok, cli)
        #[arg(long)]
        provider: Option<String>,
        /// Override LLM model
        #[arg(long)]
        model: Option<String>,
        /// Graph expansion depth
        #[arg(long, default_value = "2")]
        graph_depth: usize,
        /// Graph result limit
        #[arg(long, default_value = "20")]
        graph_limit: usize,
        /// Archive top-K
        #[arg(long, default_value = "5")]
        archive_top_k: usize,
        /// Graph episode top-K
        #[arg(long, default_value = "20")]
        episode_top_k: usize,
        /// Character ceiling for the assembled episode section
        #[arg(long, default_value = "28000")]
        episode_char_budget: usize,
        /// Exclude episode search
        #[arg(long)]
        no_episodes: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Set a config value (e.g., `config set provider ollama`)
    Set {
        /// Config key (provider, model, api_base, llm.cli.*, ephemeral.max_entries)
        key: String,
        /// New value
        value: String,
    },
}

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
        /// Treat the file as externally authored (documents, tool output)
        /// instead of inferring authorship from conversation turn roles
        #[arg(long)]
        external: bool,
    },
    /// Scan conversations/ for un-ingested archives and ingest them all
    IngestAll {
        /// Treat every file as externally authored instead of inferring
        /// authorship from conversation turn roles
        #[arg(long)]
        external: bool,
    },
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
        /// Override provider (anthropic, openai, claude-code, gemini, grok, cli)
        #[arg(long)]
        provider: Option<String>,
        /// Milliseconds delay between archives (default: 100)
        #[arg(long, default_value = "100")]
        delay_ms: u64,
        /// Maximum tokens to spend, measured and estimated alike (0 = unlimited, default: 5000000)
        #[arg(long, default_value = "5000000")]
        max_tokens: u64,
    },
    /// Pipeline operations — sync, status, flow, stale detection
    Pipeline {
        #[command(subcommand)]
        command: PipelineCommands,
    },
    /// Garbage collection — prune stale/dead relationships, orphaned entities, spent episodes
    Gc {
        /// Actually delete (default is dry-run)
        #[arg(long)]
        execute: bool,
        /// Days before a low-confidence relationship is stale (default: 30)
        #[arg(long, default_value = "30")]
        stale_days: u64,
        /// Confidence threshold for stale relationships (default: 0.5)
        #[arg(long, default_value = "0.5")]
        stale_confidence: f64,
        /// Confidence threshold for dead relationships (default: 0.2)
        #[arg(long, default_value = "0.2")]
        dead_confidence: f64,
        /// Minimum age in days for dead relationship pruning (default: 14)
        #[arg(long, default_value = "14")]
        dead_min_age_days: u64,
        /// Also sweep episodes: old, never-retrieved, self-authored, cited by nothing
        #[arg(long)]
        episodes: bool,
        /// Days before a never-retrieved episode may be collected (default: 180)
        #[arg(long, default_value = "180")]
        episode_max_age_days: u64,
        /// Only show graph health stats, don't compute GC candidates
        #[arg(long)]
        stats_only: bool,
    },
    /// Apply a session outcome to the entities it touched (utility feedback)
    Feedback {
        /// Session identifier the archive was ingested under
        session_id: String,
        /// How the session went: success, partial, failed
        #[arg(long, default_value = "success")]
        outcome: String,
    },
    /// Sync vigil-pulse signals and outcomes into the graph
    VigilSync {
        /// Path to signals.json (defaults to {entity_root}/vigil/signals.json)
        #[arg(long)]
        signals_path: Option<PathBuf>,
        /// Path to outcomes.json (defaults to {entity_root}/caliber/outcomes.json)
        #[arg(long)]
        outcomes_path: Option<PathBuf>,
    },
    /// Inspect or stop the graph daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    /// Tell memory that something it learned is wrong
    ///
    /// `--wrong` records contradicting evidence at your authority, which is how
    /// confidence is supposed to move. `--forget` removes outright and leaves
    /// no trace — prefer `--wrong` unless the memory should never have existed.
    Correct {
        /// Entity name, or the source entity of a relationship
        subject: String,
        /// Relationship type — with a target, corrects that one relationship
        rel_type: Option<String>,
        /// Target entity of the relationship
        object: Option<String>,
        /// Record that it is mistaken: confidence falls with real evidence
        #[arg(long)]
        wrong: bool,
        /// Remove it and its relationships outright
        #[arg(long)]
        forget: bool,
        /// With --wrong on an entity: contradict every one of its relationships
        #[arg(long)]
        all_edges: bool,
        /// Skip the confirmation --forget otherwise requires
        #[arg(long)]
        yes: bool,
    },
    /// Show relationship decay report — stored vs effective confidence
    DecayReport {
        /// Show only relationships for a specific entity
        #[arg(long)]
        entity: Option<String>,
        /// Show all relationships including those with no decay
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Show the daemon's socket, pid, version and uptime
    Status,
    /// Stop the daemon serving this graph
    Stop,
}

#[derive(Subcommand)]
enum PipelineCommands {
    /// Sync pipeline documents (LEARNING, THOUGHTS, CURIOSITY, REFLECTIONS, PRAXIS) into the graph
    Sync {
        /// Directory containing pipeline documents (overrides config)
        #[arg(long)]
        docs_dir: Option<PathBuf>,
    },
    /// Show pipeline health — counts by stage/status, stale entities
    Status {
        /// Days before a thought is considered stale (default: 7)
        #[arg(long, default_value = "7")]
        days: u32,
    },
    /// Trace an entity's lineage through the pipeline
    Flow {
        /// Entity name to trace
        entity: String,
    },
    /// List stale pipeline entities
    Stale {
        /// Days threshold (default: 7)
        #[arg(long, default_value = "7")]
        days: u32,
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
        Some(Commands::ArchiveSession { entity_root }) => {
            archive::run_from_hook(entity_root.as_deref())
        }
        Some(Commands::Archive { all_unarchived }) => {
            if all_unarchived {
                archive::archive_all_unarchived()
            } else {
                Err("Use --all-unarchived to archive all unarchived JSONL transcripts.".into())
            }
        }
        Some(Commands::Ingest { from, all, dir }) => run_ingest(&from, all, dir),
        Some(Commands::Checkpoint {
            trigger,
            entity_root,
        }) => checkpoint::run_from_hook(&trigger, entity_root.as_deref()),
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
        #[cfg(feature = "bench")]
        Some(Commands::Bench { command }) => run_bench(command),
        Some(Commands::Serve { dir, foreground }) => {
            let memory_dir = dir.unwrap_or_else(|| resolve_entity_root(None).join("memory"));
            run_serve(&memory_dir, foreground)
        }
        Some(Commands::Mcp { entity_root }) => {
            let memory_dir = resolve_entity_root(entity_root).join("memory");
            client_runtime()
                .map_err(recall_echo::error::RecallError::from)
                .and_then(|rt| rt.block_on(recall_echo::mcp::run(&memory_dir)))
        }
        Some(Commands::WhatDoYouKnow {
            about,
            limit,
            entity_root,
        }) => {
            let memory_dir = resolve_entity_root(entity_root).join("memory");
            client_runtime()
                .map_err(recall_echo::error::RecallError::from)
                .and_then(|rt| {
                    rt.block_on(recall_echo::inspect_cli::run(
                        &memory_dir,
                        about.as_deref(),
                        limit,
                    ))
                })
        }
        Some(Commands::Graph {
            command,
            entity_root,
        }) => {
            let root = resolve_entity_root(entity_root);
            let memory_dir = root.join("memory");
            client_runtime()
                .map_err(recall_echo::error::RecallError::from)
                .and_then(|rt| {
                    rt.block_on(async {
                        match command {
                            GraphCommands::Init => graph_cli::init(&memory_dir).await,
                            GraphCommands::Status => graph_cli::graph_status(&memory_dir).await,
                            GraphCommands::AddEntity {
                                name,
                                r#type,
                                r#abstract,
                                overview,
                                source,
                            } => {
                                graph_cli::add_entity(
                                    &memory_dir,
                                    &name,
                                    &r#type,
                                    &r#abstract,
                                    overview.as_deref(),
                                    source.as_deref(),
                                )
                                .await
                            }
                            GraphCommands::Relate {
                                from,
                                rel,
                                target,
                                description,
                                source,
                            } => {
                                graph_cli::relate(
                                    &memory_dir,
                                    &from,
                                    &rel,
                                    &target,
                                    description.as_deref(),
                                    source.as_deref(),
                                )
                                .await
                            }
                            GraphCommands::Search {
                                query,
                                limit,
                                r#type,
                                keyword,
                            } => {
                                graph_cli::search(
                                    &memory_dir,
                                    &query,
                                    limit,
                                    r#type.as_deref(),
                                    keyword.as_deref(),
                                )
                                .await
                            }
                            GraphCommands::Traverse {
                                entity,
                                depth,
                                type_filter,
                            } => {
                                graph_cli::traverse(
                                    &memory_dir,
                                    &entity,
                                    depth,
                                    type_filter.as_deref(),
                                )
                                .await
                            }
                            GraphCommands::Query {
                                query,
                                limit,
                                r#type,
                                keyword,
                                depth,
                                episodes,
                            } => {
                                graph_cli::hybrid_query(
                                    &memory_dir,
                                    &query,
                                    limit,
                                    r#type.as_deref(),
                                    keyword.as_deref(),
                                    depth,
                                    episodes,
                                )
                                .await
                            }
                            GraphCommands::Ingest { archive, external } => {
                                graph_cli::ingest(
                                    &memory_dir,
                                    &archive,
                                    external_provenance(external),
                                )
                                .await
                            }
                            GraphCommands::IngestAll { external } => {
                                graph_cli::ingest_all(&memory_dir, external_provenance(external))
                                    .await
                            }
                            #[cfg(feature = "llm")]
                            GraphCommands::Extract {
                                log,
                                all,
                                dry_run,
                                model,
                                provider,
                                delay_ms,
                                max_tokens,
                            } => {
                                graph_cli::extract(
                                    &memory_dir,
                                    log,
                                    all,
                                    dry_run,
                                    model,
                                    provider,
                                    delay_ms,
                                    max_tokens,
                                )
                                .await
                            }
                            GraphCommands::VigilSync {
                                signals_path,
                                outcomes_path,
                            } => {
                                graph_cli::vigil_sync(
                                    &memory_dir,
                                    signals_path.as_deref(),
                                    outcomes_path.as_deref(),
                                )
                                .await
                            }
                            GraphCommands::Gc {
                                execute,
                                stale_days,
                                stale_confidence,
                                dead_confidence,
                                dead_min_age_days,
                                episodes,
                                episode_max_age_days,
                                stats_only,
                            } => {
                                let options = graph_cli::GcOptions {
                                    execute,
                                    stale_days,
                                    stale_confidence,
                                    dead_confidence,
                                    dead_min_age_days,
                                    episodes,
                                    episode_max_age_days,
                                    stats_only,
                                };
                                graph_cli::gc(&memory_dir, &options).await
                            }
                            GraphCommands::Feedback {
                                session_id,
                                outcome,
                            } => graph_cli::feedback(&memory_dir, &session_id, &outcome).await,
                            GraphCommands::Daemon { command } => match command {
                                DaemonCommands::Status => {
                                    graph_cli::daemon_status(&memory_dir).await
                                }
                                DaemonCommands::Stop => graph_cli::daemon_stop(&memory_dir).await,
                            },
                            GraphCommands::Correct {
                                subject,
                                rel_type,
                                object,
                                wrong,
                                forget,
                                all_edges,
                                yes,
                            } => {
                                let options = graph_cli::CorrectOptions {
                                    subject,
                                    rel_type,
                                    object,
                                    wrong,
                                    forget,
                                    all_edges,
                                    yes,
                                };
                                graph_cli::correct(&memory_dir, &options).await
                            }
                            GraphCommands::DecayReport { entity, all } => {
                                graph_cli::decay_report(&memory_dir, entity.as_deref(), all).await
                            }
                            GraphCommands::Pipeline { command } => match command {
                                PipelineCommands::Sync { docs_dir } => {
                                    graph_cli::pipeline_sync(&memory_dir, docs_dir.as_deref()).await
                                }
                                PipelineCommands::Status { days } => {
                                    graph_cli::pipeline_status(&memory_dir, days).await
                                }
                                PipelineCommands::Flow { entity } => {
                                    graph_cli::pipeline_flow(&memory_dir, &entity).await
                                }
                                PipelineCommands::Stale { days } => {
                                    graph_cli::pipeline_stale(&memory_dir, days).await
                                }
                            },
                        }
                    })
                })
        }
    };

    if let Err(e) = result {
        eprintln!("\x1b[31m\u{2717}\x1b[0m {e}");
        std::process::exit(1);
    }
}

/// A runtime for a command that talks to the graph daemon: mostly socket
/// round-trips, so one thread is enough. Only the daemon itself, which serves
/// concurrent connections, needs the multi-thread runtime.
fn client_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Run the graph daemon for a memory directory until it stops.
fn run_serve(
    memory_dir: &std::path::Path,
    foreground: bool,
) -> Result<(), recall_echo::error::RecallError> {
    use recall_echo::serve::{run, ServeOptions};

    let options = ServeOptions::from_config(memory_dir, foreground)?;
    tokio::runtime::Runtime::new()
        .map_err(recall_echo::error::RecallError::from)
        .and_then(|rt| rt.block_on(run(options)))
}

/// Import sessions from the agent CLIs the user runs.
fn run_ingest(
    from: &[String],
    all: bool,
    dir: Option<PathBuf>,
) -> Result<(), recall_echo::error::RecallError> {
    use recall_echo::capture;
    use recall_echo::config;
    use recall_echo::error::RecallError;
    use recall_echo::transcript::Source;

    if from.is_empty() && !all {
        return Err(RecallError::Config(
            "name a CLI with --from <codex|grok|claude-code>, or use --all".into(),
        ));
    }

    let memory_dir = match dir {
        Some(dir) => dir,
        None => capture_memory_dir()?,
    };

    let sources = if from.is_empty() {
        capture::configured_sources(&config::load_from_dir(&memory_dir).capture)
    } else {
        from.iter()
            .map(|name| Source::from_str_loose(name))
            .collect::<Result<Vec<_>, _>>()?
    };

    capture::ingest(&memory_dir, &sources)
}

/// The memory directory `ingest` writes into when none is given.
///
/// The same one the hooks write into: an entity's `memory/` when
/// `RECALL_ECHO_HOME` names one, and `~/.claude` otherwise — which is where a
/// standalone install already keeps `conversations/`, `ARCHIVE.md` and
/// `EPHEMERAL.md`, whatever CLI the sessions came from.
fn capture_memory_dir() -> Result<PathBuf, recall_echo::error::RecallError> {
    match std::env::var("RECALL_ECHO_HOME") {
        Ok(root) => Ok(PathBuf::from(root).join("memory")),
        Err(_) => paths::claude_dir(),
    }
}

fn resolve_entity_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| paths::entity_root().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The provenance override a `--external` flag asks for. Without the flag the
/// class is inferred per chunk from conversation turn roles.
fn external_provenance(external: bool) -> Option<recall_echo::graph::Provenance> {
    external.then_some(recall_echo::graph::Provenance::External)
}

#[cfg(feature = "bench")]
fn run_bench(command: BenchCommands) -> Result<(), recall_echo::error::RecallError> {
    use recall_echo::bench::{answer_question, ingest_conversation, AnswerOpts, BenchConversation};
    use recall_echo::config::Provider;

    let rt = tokio::runtime::Runtime::new().map_err(recall_echo::error::RecallError::from)?;

    match command {
        BenchCommands::Ingest {
            entity_root,
            conv_json,
            provider,
            model,
            no_llm,
        } => {
            let raw = read_json_input(conv_json.as_deref())?;
            let conv: BenchConversation = serde_json::from_str(&raw)?;

            rt.block_on(async {
                let memory_dir = entity_root.join("memory");
                let llm_pair = if no_llm {
                    None
                } else {
                    Some(recall_echo::llm_provider::create_provider(
                        &memory_dir,
                        provider.as_deref(),
                        model.as_deref(),
                    )?)
                };
                let llm_ref: Option<&dyn recall_echo::graph::llm::LlmProvider> = llm_pair
                    .as_ref()
                    .map(|(p, _)| p.as_ref() as &dyn recall_echo::graph::llm::LlmProvider);

                let stats = ingest_conversation(&entity_root, &conv, llm_ref).await?;
                println!("{}", serde_json::to_string(&stats)?);
                Ok::<_, recall_echo::error::RecallError>(())
            })
        }
        BenchCommands::Answer {
            entity_root,
            question,
            question_stdin,
            provider,
            model,
            graph_depth,
            graph_limit,
            archive_top_k,
            episode_top_k,
            episode_char_budget,
            no_episodes,
        } => {
            let question_text = match (question, question_stdin) {
                (Some(q), false) => q,
                (None, true) => read_stdin_to_string()?,
                _ => {
                    return Err(recall_echo::error::RecallError::Config(
                        "exactly one of --question or --question-stdin is required".into(),
                    ))
                }
            };

            let provider_override = match provider.as_deref() {
                Some(p) => Some(Provider::from_str_loose(p)?),
                None => None,
            };

            let opts = AnswerOpts {
                graph_depth,
                graph_limit,
                archive_top_k,
                episode_top_k,
                episode_char_budget,
                include_episodes: !no_episodes,
                provider_override,
                model_override: model,
                ..AnswerOpts::default()
            };

            rt.block_on(async {
                let answer = answer_question(&entity_root, &question_text, opts).await?;
                println!("{}", serde_json::to_string(&answer)?);
                Ok::<_, recall_echo::error::RecallError>(())
            })
        }
    }
}

#[cfg(feature = "bench")]
fn read_json_input(
    path: Option<&std::path::Path>,
) -> Result<String, recall_echo::error::RecallError> {
    use std::io::Read;
    let stdin = || -> Result<String, recall_echo::error::RecallError> {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    };
    match path {
        Some(p) if p.as_os_str() == "-" => stdin(),
        Some(p) => Ok(std::fs::read_to_string(p)?),
        None => stdin(),
    }
}

#[cfg(feature = "bench")]
fn read_stdin_to_string() -> Result<String, recall_echo::error::RecallError> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf.trim().to_string())
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
