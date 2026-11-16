// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use recall_echo::graph_cli;
use recall_echo::theme::{BAD, RESET};
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
    /// Initialize the memory system for a pulse
    Init {
        /// Pulse root directory (defaults to ~/.claude when Claude Code is installed, else the current directory)
        pulse_root: Option<PathBuf>,
    },
    /// Memory system health check
    Status {
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        pulse_root: Option<PathBuf>,
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
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        pulse_root: Option<PathBuf>,
    },
    /// Output EPHEMERAL.md content
    Consume {
        /// Pulse root directory (defaults to the initialised cwd, the root `init` persisted, then ~/.claude)
        pulse_root: Option<PathBuf>,
    },
    /// Memory dashboard with health, stats, and recent sessions
    Dashboard {
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        pulse_root: Option<PathBuf>,
    },
    /// Archive a Claude Code session from JSONL transcript (SessionEnd hook)
    ArchiveSession {
        /// Pulse root directory (defaults to the initialised cwd, the root `init` persisted, then ~/.claude)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
    },
    /// Archive JSONL transcripts
    Archive {
        /// Archive all unarchived JSONL transcripts under ~/.claude/projects/
        #[arg(long)]
        all_unarchived: bool,
        /// Pulse root to archive into (defaults to RECALL_ECHO_HOME, then the root `init` persisted, then ~/.claude — never the current directory)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
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
        /// Pulse root directory (defaults to the initialised cwd, the root `init` persisted, then ~/.claude)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
    },
    /// View or modify configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
    },
    /// Run the graph daemon (started automatically by graph commands and hooks)
    Serve {
        /// Memory directory to serve (defaults to {pulse_root}/memory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Stay in the foreground: log to stderr and never idle-shut-down
        /// (for systemd units and debugging)
        #[arg(long)]
        foreground: bool,
    },
    /// Run the MCP server (stdio) so an agent can query its own memory
    Mcp {
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
    },
    /// Show what memory holds — everything, or one subject
    WhatDoYouKnow {
        /// Narrow it to one subject
        #[arg(long = "about", value_name = "TOPIC")]
        about: Option<String>,
        /// Entities listed per type (or results, with --about)
        #[arg(long, default_value = "3")]
        limit: usize,
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
    },
    /// Knowledge graph operations
    Graph {
        #[command(subcommand)]
        command: GraphCommands,
        /// Pulse root directory (defaults to the initialised cwd, else the root `init` persisted)
        #[arg(long, alias = "entity-root")]
        pulse_root: Option<PathBuf>,
    },
    /// LoCoMo benchmark harness (ingest a conversation, answer a question)
    #[cfg(feature = "bench")]
    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },
    /// Update recall-echo to the latest GitHub release
    #[cfg(feature = "self-update")]
    Update {
        /// Only report current vs latest; exit 10 when an update is available
        #[arg(long)]
        check: bool,
        /// Install a specific release tag (e.g. v4.3.0) instead of the latest
        #[arg(long)]
        version: Option<String>,
        /// Reinstall even when already on the requested version
        #[arg(long)]
        force: bool,
    },
}

#[cfg(feature = "bench")]
#[derive(Subcommand)]
enum BenchCommands {
    /// Ingest a single LoCoMo conversation JSON
    Ingest {
        /// Pulse root directory
        #[arg(long, alias = "entity-root")]
        pulse_root: PathBuf,
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
    /// Answer a question against an already-ingested pulse root
    Answer {
        /// Pulse root directory
        #[arg(long, alias = "entity-root")]
        pulse_root: PathBuf,
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
    /// Grade graph writes against a fact script
    Writes {
        /// Fact script path
        #[arg(long)]
        script: PathBuf,
        /// New or empty directory to ingest into. Omit for a fresh temporary directory.
        #[arg(long)]
        work_dir: Option<PathBuf>,
        /// Provider to extract with. Omit to replay the script's own claims.
        #[arg(long)]
        provider: Option<String>,
        /// Provider model override
        #[arg(long)]
        model: Option<String>,
        /// Fresh scratch stores to grade. One prints the single report.
        /// More than one prints each run plus the median and the range.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Extraction pipeline. Overrides `[graph] pipeline`. Absent means v1
        /// unless the memory directory's config says otherwise.
        #[arg(long, value_parser = parse_pipeline)]
        pipeline: Option<recall_echo::graph::PipelineVersion>,
        /// Show stored facts to pass 1. On unless `--no-known-facts-context`.
        #[arg(long)]
        known_facts_context: bool,
        /// Match kept facts to stored rows after pass 1. Off unless set.
        #[arg(long)]
        known_match: bool,
        /// Close drifted copies after the session write. On unless `--no-consolidate`.
        #[arg(long)]
        consolidate: bool,
        /// Hold a conflicting write the retirement guard would skip.
        /// Implies `--consolidate`. On unless `--no-consolidate-hold`. Pending
        /// rows stay out of live reads if this is later turned off; see
        /// docs/grounding.md.
        #[arg(long)]
        consolidate_hold: bool,
        /// Record derived-from links that feed consolidation.
        /// Implies `--consolidate`. On unless `--no-consolidate-links`.
        /// Independent of `--consolidate-hold` and `--drop-restatements`.
        #[arg(long)]
        consolidate_links: bool,
        /// Close a sibling or derived-from partner after the judge.
        /// Implies `--consolidate`. On unless `--no-consolidate-components`.
        /// Independent of `--consolidate-hold`, `--consolidate-links`, and
        /// `--drop-restatements`.
        #[arg(long)]
        consolidate_components: bool,
        /// Keep a judged rename, a "becomes" replacement, or a dropped-entity rule.
        /// Implies `--consolidate`. On unless `--no-consolidate-key-rename`.
        /// Independent of `--consolidate-hold`, `--consolidate-links`,
        /// `--consolidate-components`, and `--drop-restatements`.
        #[arg(long)]
        consolidate_key_rename: bool,
        /// Count a unique pass-1 replaces match as the departure cue.
        /// Implies `--consolidate`. On unless `--no-consolidate-replaces-cue`.
        /// Independent of `--consolidate-hold`, `--consolidate-links`,
        /// `--consolidate-components`, `--consolidate-key-rename`, and
        /// `--drop-restatements`.
        #[arg(long)]
        consolidate_replaces_cue: bool,
        /// Keep a judged supersede when an exact rule replaces and a departure
        /// word supply the cue. Implies `--consolidate`. On unless
        /// `--no-consolidate-replaces-departure`. Independent of
        /// `--consolidate-replaces-cue`.
        #[arg(long)]
        consolidate_replaces_departure: bool,
        /// Close orphan copies of a rejected value after the session write.
        /// Implies `--consolidate`. On unless `--no-consolidate-orphan-sweep`.
        /// Does not turn on `--ground-component-edges`.
        #[arg(long)]
        consolidate_orphan_sweep: bool,
        /// Close a same-turn rule fragment after its rule is closed.
        /// Implies `--consolidate`. On unless `--no-consolidate-rule-fragments`.
        /// Does not turn on `--ground-component-edges`.
        #[arg(long)]
        consolidate_rule_fragments: bool,
        /// Retire a dropped component's settings after the session write.
        /// Implies `--consolidate`. On unless `--no-consolidate-retire-component`.
        /// Does not turn on `--ground-component-edges`.
        #[arg(long)]
        consolidate_retire_component: bool,
        /// Bind a decided setting key from a shown known row. On unless
        /// `--no-known-key-binding`. Does not turn on `--known-facts-context`.
        #[arg(long)]
        known_key_binding: bool,
        /// Drop a decided rule that only restates a same-session setting.
        /// On unless `--no-drop-restatements`.
        #[arg(long)]
        drop_restatements: bool,
        /// Accept a decided component part_of the session project when the
        /// turn names the component. On unless `--no-ground-component-edges`.
        /// Implies nothing.
        #[arg(long)]
        ground_component_edges: bool,
        /// List raw-turn values missing from the store. Off unless set.
        /// Implies nothing. The list is not stored.
        #[arg(long)]
        recover_turns: bool,
        /// Turn the known-facts block off for this run.
        #[arg(long, conflicts_with = "known_facts_context")]
        no_known_facts_context: bool,
        /// Turn consolidation off for this run.
        #[arg(
            long,
            conflicts_with_all = [
                "consolidate",
                "consolidate_hold",
                "consolidate_links",
                "consolidate_components",
                "consolidate_key_rename",
                "consolidate_replaces_cue",
                "consolidate_replaces_departure",
                "consolidate_orphan_sweep",
                "consolidate_rule_fragments",
                "consolidate_retire_component",
            ]
        )]
        no_consolidate: bool,
        /// Turn the contradiction hold off for this run.
        #[arg(long, conflicts_with = "consolidate_hold")]
        no_consolidate_hold: bool,
        /// Turn derived-from links off for this run.
        #[arg(long, conflicts_with = "consolidate_links")]
        no_consolidate_links: bool,
        /// Turn sibling and derived-from cues off for this run.
        #[arg(long, conflicts_with = "consolidate_components")]
        no_consolidate_components: bool,
        /// Turn judged renames off for this run.
        #[arg(long, conflicts_with = "consolidate_key_rename")]
        no_consolidate_key_rename: bool,
        /// Turn the replaces departure cue off for this run.
        #[arg(long, conflicts_with = "consolidate_replaces_cue")]
        no_consolidate_replaces_cue: bool,
        /// Turn the exact-replaces departure cue off for this run.
        #[arg(long, conflicts_with = "consolidate_replaces_departure")]
        no_consolidate_replaces_departure: bool,
        /// Turn the orphan sweep off for this run.
        #[arg(long, conflicts_with = "consolidate_orphan_sweep")]
        no_consolidate_orphan_sweep: bool,
        /// Turn same-turn rule-fragment closes off for this run.
        #[arg(long, conflicts_with = "consolidate_rule_fragments")]
        no_consolidate_rule_fragments: bool,
        /// Turn dropped-component retirement off for this run.
        #[arg(long, conflicts_with = "consolidate_retire_component")]
        no_consolidate_retire_component: bool,
        /// Turn known-row key binding off for this run.
        #[arg(long, conflicts_with = "known_key_binding")]
        no_known_key_binding: bool,
        /// Turn restatement dropping off for this run.
        #[arg(long, conflicts_with = "drop_restatements")]
        no_drop_restatements: bool,
        /// Turn component-edge grounding off for this run.
        #[arg(long, conflicts_with = "ground_component_edges")]
        no_ground_component_edges: bool,
    },
    /// List raw-turn values missing from a store. Writes nothing.
    RecoverReplay {
        /// Memory directory whose archives and graph are read.
        #[arg(long)]
        memory_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Set a config value (e.g., `config set provider claude-code`)
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
        /// Extraction pipeline. Overrides `[graph] pipeline`. Absent follows
        /// the config key, which defaults to v1 (episodes only).
        #[arg(long, value_parser = parse_pipeline)]
        pipeline: Option<recall_echo::graph::PipelineVersion>,
    },
    /// Re-run schema migrations against the graph store
    ///
    /// Opening the store already migrates it, so this normally reports that
    /// there is nothing to do. It repairs the one state opening cannot:
    /// a version marker claiming a migration that did not finish, which
    /// leaves rows the current read paths cannot see.
    Migrate {
        /// Clear the version marker first, so every migration runs again
        #[arg(long)]
        force: bool,
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
        /// Extraction pipeline. Overrides `[graph] pipeline`. Absent follows
        /// the config key, which defaults to v1.
        #[arg(long, value_parser = parse_pipeline)]
        pipeline: Option<recall_echo::graph::PipelineVersion>,
        /// Rebuild a fresh pipeline v2 store beside the current one and swap it into place
        #[arg(long)]
        rebuild: bool,
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
        /// Path to signals.json (defaults to {pulse_root}/vigil/signals.json)
        #[arg(long)]
        signals_path: Option<PathBuf>,
        /// Path to outcomes.json (defaults to {pulse_root}/caliber/outcomes.json)
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
        Some(Commands::Init { pulse_root }) => {
            let root = resolve_init_root(pulse_root);
            init::run(&root)
        }
        Some(Commands::Status {
            pulse_root: Some(root),
        }) => status::run_with_base(&root),
        Some(Commands::Status { pulse_root: None }) => status::run(),
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
        Some(Commands::Distill { pulse_root }) => {
            let root = resolve_pulse_root(pulse_root);
            distill::run_with_base(&root)
        }
        Some(Commands::Consume { pulse_root }) => {
            // Same layout resolution as archive-session and checkpoint: the
            // file consume reads must be the file the SessionEnd hook wrote,
            // on both the pulse layout and a claude-style root.
            paths::resolved_hook_base_dir(pulse_root.as_deref())
                .and_then(|base| recall_echo::consume::run(&base.join("EPHEMERAL.md")))
        }
        Some(Commands::Dashboard { pulse_root }) => {
            let root = resolve_pulse_root(pulse_root);
            let recall = RecallEcho::new(root);
            let version = env!("CARGO_PKG_VERSION");
            dashboard::render(&recall, "echo", version, 200);
            Ok(())
        }
        // JSONL commands (ported from recall-claude)
        Some(Commands::ArchiveSession { pulse_root }) => {
            archive::run_from_hook(pulse_root.as_deref())
        }
        Some(Commands::Archive {
            all_unarchived,
            pulse_root,
        }) => {
            if all_unarchived {
                archive::archive_all_unarchived_at(pulse_root.as_deref())
            } else {
                Err("Use --all-unarchived to archive all unarchived JSONL transcripts.".into())
            }
        }
        Some(Commands::Ingest { from, all, dir }) => run_ingest(&from, all, dir),
        Some(Commands::Checkpoint {
            trigger,
            pulse_root,
        }) => checkpoint::run_from_hook(&trigger, pulse_root.as_deref()),
        Some(Commands::Config {
            command,
            pulse_root,
        }) => {
            let root = resolve_pulse_root(pulse_root);
            let memory_dir = root.join("memory");
            match command {
                ConfigCommands::Show => config_cli::show(&memory_dir),
                ConfigCommands::Set { key, value } => config_cli::set(&memory_dir, &key, &value),
            }
        }
        #[cfg(feature = "bench")]
        Some(Commands::Bench { command }) => run_bench(command),
        #[cfg(feature = "self-update")]
        Some(Commands::Update {
            check,
            version,
            force,
        }) => client_runtime()
            .map_err(recall_echo::error::RecallError::from)
            .and_then(|rt| {
                rt.block_on(recall_echo::update::run(recall_echo::update::UpdateOpts {
                    check,
                    version,
                    force,
                }))
            })
            .map(|code| {
                if code != 0 {
                    std::process::exit(code);
                }
            }),
        Some(Commands::Serve { dir, foreground }) => {
            let memory_dir = dir.unwrap_or_else(|| resolve_pulse_root(None).join("memory"));
            run_serve(&memory_dir, foreground)
        }
        Some(Commands::Mcp { pulse_root }) => {
            let memory_dir = resolve_pulse_root(pulse_root).join("memory");
            client_runtime()
                .map_err(recall_echo::error::RecallError::from)
                .and_then(|rt| rt.block_on(recall_echo::mcp::run(&memory_dir)))
        }
        Some(Commands::WhatDoYouKnow {
            about,
            limit,
            pulse_root,
        }) => {
            let memory_dir = resolve_pulse_root(pulse_root).join("memory");
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
            pulse_root,
        }) => {
            let root = resolve_pulse_root(pulse_root);
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
                            GraphCommands::Ingest {
                                archive,
                                external,
                                pipeline,
                            } => {
                                graph_cli::ingest(
                                    &memory_dir,
                                    &archive,
                                    external_provenance(external),
                                    pipeline,
                                )
                                .await
                            }
                            GraphCommands::IngestAll { external } => {
                                graph_cli::ingest_all(&memory_dir, external_provenance(external))
                                    .await
                            }
                            GraphCommands::Migrate { force } => {
                                graph_cli::migrate(&memory_dir, force).await
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
                                pipeline,
                                rebuild,
                            } => {
                                graph_cli::extract(
                                    &memory_dir,
                                    graph_cli::ExtractOptions {
                                        log,
                                        all,
                                        dry_run,
                                        model,
                                        provider,
                                        delay_ms,
                                        max_tokens,
                                        pipeline,
                                        rebuild,
                                    },
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
        eprintln!("{BAD}\u{2717}{RESET} {e}");
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
/// The same one the hooks write into, via the same resolution
/// (`RECALL_ECHO_HOME`, an initialized cwd, then the root `init` persisted),
/// with `~/.claude` as the legacy fallback — which is where a standalone
/// install already keeps `conversations/`, `ARCHIVE.md` and `EPHEMERAL.md`,
/// whatever CLI the sessions came from.
fn capture_memory_dir() -> Result<PathBuf, recall_echo::error::RecallError> {
    paths::resolved_hook_base_dir(None)
}

fn resolve_pulse_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| paths::pulse_root().unwrap_or_else(|_| PathBuf::from(".")))
}

fn parse_pipeline(raw: &str) -> Result<recall_echo::graph::PipelineVersion, String> {
    recall_echo::graph::PipelineVersion::parse(raw)
        .ok_or_else(|| format!("pipeline must be v1 or v2, not {raw}"))
}

/// The provenance override a `--external` flag asks for. Without the flag the
/// class is inferred per chunk from conversation turn roles.
fn external_provenance(external: bool) -> Option<recall_echo::graph::Provenance> {
    external.then_some(recall_echo::graph::Provenance::External)
}

#[cfg(feature = "bench")]
fn run_bench(command: BenchCommands) -> Result<(), recall_echo::error::RecallError> {
    let rt = tokio::runtime::Runtime::new().map_err(recall_echo::error::RecallError::from)?;

    match command {
        BenchCommands::Ingest {
            pulse_root,
            conv_json,
            provider,
            model,
            no_llm,
        } => bench_ingest(&rt, pulse_root, conv_json, provider, model, no_llm),
        command @ BenchCommands::Answer { .. } => {
            let (root, question, opts) = answer_request(command)?;
            bench_answer(&rt, &root, &question, opts)
        }
        BenchCommands::Writes {
            script,
            work_dir,
            provider,
            model,
            repeat,
            pipeline,
            known_facts_context,
            known_match,
            consolidate,
            consolidate_hold,
            consolidate_links,
            consolidate_components,
            consolidate_key_rename,
            consolidate_replaces_cue,
            consolidate_replaces_departure,
            consolidate_orphan_sweep,
            consolidate_rule_fragments,
            consolidate_retire_component,
            known_key_binding,
            drop_restatements,
            ground_component_edges,
            recover_turns,
            no_known_facts_context,
            no_consolidate,
            no_consolidate_hold,
            no_consolidate_links,
            no_consolidate_components,
            no_consolidate_key_rename,
            no_consolidate_replaces_cue,
            no_consolidate_replaces_departure,
            no_consolidate_orphan_sweep,
            no_consolidate_rule_fragments,
            no_consolidate_retire_component,
            no_known_key_binding,
            no_drop_restatements,
            no_ground_component_edges,
        } => run_writes(
            script,
            work_dir,
            provider,
            model,
            repeat,
            pipeline,
            on_off(
                known_facts_context,
                recall_echo::bench::writes::KnownFactsContext::On,
                recall_echo::bench::writes::KnownFactsContext::Off,
            ),
            on_off(
                known_match,
                recall_echo::bench::writes::KnownMatch::On,
                recall_echo::bench::writes::KnownMatch::Off,
            ),
            consolidate_mode(consolidate_hold, consolidate),
            on_off(
                drop_restatements,
                recall_echo::bench::writes::DropRestatements::On,
                recall_echo::bench::writes::DropRestatements::Off,
            ),
            on_off(
                consolidate_links,
                recall_echo::bench::writes::ConsolidateLinks::On,
                recall_echo::bench::writes::ConsolidateLinks::Off,
            ),
            on_off(
                ground_component_edges,
                recall_echo::bench::writes::GroundComponentEdges::On,
                recall_echo::bench::writes::GroundComponentEdges::Off,
            ),
            on_off(
                consolidate_components,
                recall_echo::bench::writes::ConsolidateComponents::On,
                recall_echo::bench::writes::ConsolidateComponents::Off,
            ),
            on_off(
                recover_turns,
                recall_echo::bench::writes::RecoverTurns::On,
                recall_echo::bench::writes::RecoverTurns::Off,
            ),
            on_off(
                consolidate_key_rename,
                recall_echo::bench::writes::ConsolidateKeyRename::On,
                recall_echo::bench::writes::ConsolidateKeyRename::Off,
            ),
            on_off(
                consolidate_replaces_cue,
                recall_echo::bench::writes::ConsolidateReplacesCue::On,
                recall_echo::bench::writes::ConsolidateReplacesCue::Off,
            ),
            on_off(
                consolidate_replaces_departure,
                recall_echo::bench::writes::ConsolidateReplacesDeparture::On,
                recall_echo::bench::writes::ConsolidateReplacesDeparture::Off,
            ),
            on_off(
                consolidate_orphan_sweep,
                recall_echo::bench::writes::ConsolidateOrphanSweep::On,
                recall_echo::bench::writes::ConsolidateOrphanSweep::Off,
            ),
            on_off(
                consolidate_rule_fragments,
                recall_echo::bench::writes::ConsolidateRuleFragments::On,
                recall_echo::bench::writes::ConsolidateRuleFragments::Off,
            ),
            on_off(
                consolidate_retire_component,
                recall_echo::bench::writes::ConsolidateRetireComponent::On,
                recall_echo::bench::writes::ConsolidateRetireComponent::Off,
            ),
            on_off(
                known_key_binding,
                recall_echo::bench::writes::KnownKeyBinding::On,
                recall_echo::bench::writes::KnownKeyBinding::Off,
            ),
            recall_echo::bench::writes::OptOuts {
                known_facts_context: no_known_facts_context,
                consolidate: no_consolidate,
                consolidate_hold: no_consolidate_hold,
                drop_restatements: no_drop_restatements,
                consolidate_links: no_consolidate_links,
                consolidate_components: no_consolidate_components,
                ground_component_edges: no_ground_component_edges,
                consolidate_key_rename: no_consolidate_key_rename,
                consolidate_replaces_cue: no_consolidate_replaces_cue,
                consolidate_replaces_departure: no_consolidate_replaces_departure,
                known_key_binding: no_known_key_binding,
                consolidate_orphan_sweep: no_consolidate_orphan_sweep,
                consolidate_rule_fragments: no_consolidate_rule_fragments,
                consolidate_retire_component: no_consolidate_retire_component,
            },
        ),
        BenchCommands::RecoverReplay { memory_dir } => bench_recover_replay(memory_dir),
    }
}

#[cfg(feature = "bench")]
fn bench_ingest(
    rt: &tokio::runtime::Runtime,
    pulse_root: PathBuf,
    conv_json: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
    no_llm: bool,
) -> Result<(), recall_echo::error::RecallError> {
    use recall_echo::bench::{ingest_conversation, BenchConversation};

    let raw = read_json_input(conv_json.as_deref())?;
    let conv: BenchConversation = serde_json::from_str(&raw)?;

    rt.block_on(async {
        let memory_dir = pulse_root.join("memory");
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

        let stats = ingest_conversation(&pulse_root, &conv, llm_ref).await?;
        println!("{}", serde_json::to_string(&stats)?);
        Ok::<_, recall_echo::error::RecallError>(())
    })
}

#[cfg(feature = "bench")]
fn answer_request(
    command: BenchCommands,
) -> Result<(PathBuf, String, recall_echo::bench::AnswerOpts), recall_echo::error::RecallError> {
    use recall_echo::bench::AnswerOpts;

    let BenchCommands::Answer {
        pulse_root,
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
    } = command
    else {
        return Err(recall_echo::error::RecallError::Config(
            "expected bench answer".into(),
        ));
    };
    let question_text = bench_question(question, question_stdin)?;
    let provider_override = bench_provider(provider.as_deref())?;
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
    Ok((pulse_root, question_text, opts))
}

#[cfg(feature = "bench")]
fn bench_answer(
    rt: &tokio::runtime::Runtime,
    root: &std::path::Path,
    question: &str,
    opts: recall_echo::bench::AnswerOpts,
) -> Result<(), recall_echo::error::RecallError> {
    use recall_echo::bench::answer_question;

    rt.block_on(async {
        let answer = answer_question(root, question, opts).await?;
        println!("{}", serde_json::to_string(&answer)?);
        Ok::<_, recall_echo::error::RecallError>(())
    })
}

#[cfg(feature = "bench")]
fn bench_question(
    question: Option<String>,
    question_stdin: bool,
) -> Result<String, recall_echo::error::RecallError> {
    bench_question_with(question, question_stdin, read_stdin_to_string)
}

#[cfg(feature = "bench")]
fn bench_question_with(
    question: Option<String>,
    stdin: bool,
    read: impl FnOnce() -> Result<String, recall_echo::error::RecallError>,
) -> Result<String, recall_echo::error::RecallError> {
    match (question, stdin) {
        (Some(text), false) => Ok(text),
        (None, true) => read(),
        _ => Err(recall_echo::error::RecallError::Config(
            "exactly one of --question or --question-stdin is required".into(),
        )),
    }
}

#[cfg(feature = "bench")]
fn bench_provider(
    provider: Option<&str>,
) -> Result<Option<recall_echo::config::Provider>, recall_echo::error::RecallError> {
    match provider {
        Some(name) => Ok(Some(recall_echo::config::Provider::from_str_loose(name)?)),
        None => Ok(None),
    }
}

#[cfg(feature = "bench")]
fn bench_recover_replay(memory_dir: PathBuf) -> Result<(), recall_echo::error::RecallError> {
    let rt = tokio::runtime::Runtime::new().map_err(recall_echo::error::RecallError::from)?;
    let lines = rt.block_on(recall_echo::bench::writes::recover_replay(&memory_dir))?;
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

#[cfg(feature = "bench")]
fn on_off<T>(flag: bool, on: T, off: T) -> T {
    if flag {
        on
    } else {
        off
    }
}

#[cfg(feature = "bench")]
fn consolidate_mode(hold: bool, consolidate: bool) -> recall_echo::bench::writes::Consolidate {
    if hold {
        recall_echo::bench::writes::Consolidate::Hold
    } else if consolidate {
        recall_echo::bench::writes::Consolidate::On
    } else {
        recall_echo::bench::writes::Consolidate::Off
    }
}

#[cfg(feature = "bench")]
#[allow(clippy::too_many_arguments)]
fn run_writes(
    script: PathBuf,
    work_dir: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
    repeat: usize,
    pipeline: Option<recall_echo::graph::PipelineVersion>,
    known_facts_context: recall_echo::bench::writes::KnownFactsContext,
    known_match: recall_echo::bench::writes::KnownMatch,
    consolidate: recall_echo::bench::writes::Consolidate,
    drop_restatements: recall_echo::bench::writes::DropRestatements,
    consolidate_links: recall_echo::bench::writes::ConsolidateLinks,
    ground_component_edges: recall_echo::bench::writes::GroundComponentEdges,
    consolidate_components: recall_echo::bench::writes::ConsolidateComponents,
    recover_turns: recall_echo::bench::writes::RecoverTurns,
    consolidate_key_rename: recall_echo::bench::writes::ConsolidateKeyRename,
    consolidate_replaces_cue: recall_echo::bench::writes::ConsolidateReplacesCue,
    consolidate_replaces_departure: recall_echo::bench::writes::ConsolidateReplacesDeparture,
    consolidate_orphan_sweep: recall_echo::bench::writes::ConsolidateOrphanSweep,
    consolidate_rule_fragments: recall_echo::bench::writes::ConsolidateRuleFragments,
    consolidate_retire_component: recall_echo::bench::writes::ConsolidateRetireComponent,
    known_key_binding: recall_echo::bench::writes::KnownKeyBinding,
    opt_outs: recall_echo::bench::writes::OptOuts,
) -> Result<(), recall_echo::error::RecallError> {
    use recall_echo::bench::writes;

    if repeat == 0 {
        return Err(recall_echo::error::RecallError::Config(
            "repeat must be at least 1".into(),
        ));
    }
    let script = writes::load_script(&script)?;
    let scratch = match work_dir {
        Some(dir) => {
            writes::require_scratch(&dir)?;
            Scratch::Kept(dir)
        }
        None => Scratch::Temporary(temporary_work_dir()?),
    };
    let root = scratch.path().to_path_buf();
    let configured = recall_echo::config::load_from_dir(&root.join("memory"))
        .graph
        .and_then(|section| section.pipeline);
    let pipeline = recall_echo::graph::PipelineVersion::resolve(pipeline, configured.as_deref())
        .map_err(recall_echo::error::RecallError::Config)?;
    let rt = tokio::runtime::Runtime::new().map_err(recall_echo::error::RecallError::from)?;
    let json = if repeat == 1 {
        let report = rt.block_on(grade_once(
            &script,
            &root,
            provider,
            model.as_deref(),
            pipeline,
            known_facts_context,
            known_match,
            consolidate,
            drop_restatements,
            consolidate_links,
            ground_component_edges,
            consolidate_components,
            recover_turns,
            consolidate_key_rename,
            consolidate_replaces_cue,
            consolidate_replaces_departure,
            consolidate_orphan_sweep,
            consolidate_rule_fragments,
            consolidate_retire_component,
            known_key_binding,
            opt_outs,
        ))?;
        serde_json::to_string_pretty(&report)?
    } else {
        let report = rt.block_on(grade_repeat(
            &script,
            &root,
            repeat,
            provider,
            model.as_deref(),
            pipeline,
            known_facts_context,
            known_match,
            consolidate,
            drop_restatements,
            consolidate_links,
            ground_component_edges,
            consolidate_components,
            recover_turns,
            consolidate_key_rename,
            consolidate_replaces_cue,
            consolidate_replaces_departure,
            consolidate_orphan_sweep,
            consolidate_rule_fragments,
            consolidate_retire_component,
            known_key_binding,
            opt_outs,
        ))?;
        serde_json::to_string_pretty(&report)?
    };
    println!("{json}");
    Ok(())
}

#[cfg(feature = "bench")]
#[allow(clippy::too_many_arguments)]
async fn grade_once(
    script: &recall_echo::bench::writes::FactScript,
    root: &std::path::Path,
    provider: Option<String>,
    model: Option<&str>,
    pipeline: recall_echo::graph::PipelineVersion,
    known_facts_context: recall_echo::bench::writes::KnownFactsContext,
    known_match: recall_echo::bench::writes::KnownMatch,
    consolidate: recall_echo::bench::writes::Consolidate,
    drop_restatements: recall_echo::bench::writes::DropRestatements,
    consolidate_links: recall_echo::bench::writes::ConsolidateLinks,
    ground_component_edges: recall_echo::bench::writes::GroundComponentEdges,
    consolidate_components: recall_echo::bench::writes::ConsolidateComponents,
    recover_turns: recall_echo::bench::writes::RecoverTurns,
    consolidate_key_rename: recall_echo::bench::writes::ConsolidateKeyRename,
    consolidate_replaces_cue: recall_echo::bench::writes::ConsolidateReplacesCue,
    consolidate_replaces_departure: recall_echo::bench::writes::ConsolidateReplacesDeparture,
    consolidate_orphan_sweep: recall_echo::bench::writes::ConsolidateOrphanSweep,
    consolidate_rule_fragments: recall_echo::bench::writes::ConsolidateRuleFragments,
    consolidate_retire_component: recall_echo::bench::writes::ConsolidateRetireComponent,
    known_key_binding: recall_echo::bench::writes::KnownKeyBinding,
    opt_outs: recall_echo::bench::writes::OptOuts,
) -> Result<recall_echo::bench::writes::WriteReport, recall_echo::error::RecallError> {
    use recall_echo::bench::writes::{self, Extractor};

    match provider {
        Some(name) => {
            let memory_dir = root.join("memory");
            let (llm, _) =
                recall_echo::llm_provider::create_provider(&memory_dir, Some(&name), model)?;
            writes::run(
                script,
                root,
                Extractor::Live(llm.as_ref()),
                pipeline,
                known_facts_context,
                known_match,
                consolidate,
                drop_restatements,
                consolidate_links,
                ground_component_edges,
                consolidate_components,
                recover_turns,
                consolidate_key_rename,
                consolidate_replaces_cue,
                consolidate_replaces_departure,
                consolidate_orphan_sweep,
                consolidate_rule_fragments,
                consolidate_retire_component,
                known_key_binding,
                opt_outs,
            )
            .await
        }
        None => {
            writes::run(
                script,
                root,
                Extractor::Scripted,
                pipeline,
                known_facts_context,
                known_match,
                consolidate,
                drop_restatements,
                consolidate_links,
                ground_component_edges,
                consolidate_components,
                recover_turns,
                consolidate_key_rename,
                consolidate_replaces_cue,
                consolidate_replaces_departure,
                consolidate_orphan_sweep,
                consolidate_rule_fragments,
                consolidate_retire_component,
                known_key_binding,
                opt_outs,
            )
            .await
        }
    }
}

#[cfg(feature = "bench")]
#[allow(clippy::too_many_arguments)]
async fn grade_repeat(
    script: &recall_echo::bench::writes::FactScript,
    root: &std::path::Path,
    repeat: usize,
    provider: Option<String>,
    model: Option<&str>,
    pipeline: recall_echo::graph::PipelineVersion,
    known_facts_context: recall_echo::bench::writes::KnownFactsContext,
    known_match: recall_echo::bench::writes::KnownMatch,
    consolidate: recall_echo::bench::writes::Consolidate,
    drop_restatements: recall_echo::bench::writes::DropRestatements,
    consolidate_links: recall_echo::bench::writes::ConsolidateLinks,
    ground_component_edges: recall_echo::bench::writes::GroundComponentEdges,
    consolidate_components: recall_echo::bench::writes::ConsolidateComponents,
    recover_turns: recall_echo::bench::writes::RecoverTurns,
    consolidate_key_rename: recall_echo::bench::writes::ConsolidateKeyRename,
    consolidate_replaces_cue: recall_echo::bench::writes::ConsolidateReplacesCue,
    consolidate_replaces_departure: recall_echo::bench::writes::ConsolidateReplacesDeparture,
    consolidate_orphan_sweep: recall_echo::bench::writes::ConsolidateOrphanSweep,
    consolidate_rule_fragments: recall_echo::bench::writes::ConsolidateRuleFragments,
    consolidate_retire_component: recall_echo::bench::writes::ConsolidateRetireComponent,
    known_key_binding: recall_echo::bench::writes::KnownKeyBinding,
    opt_outs: recall_echo::bench::writes::OptOuts,
) -> Result<recall_echo::bench::writes::RepeatReport, recall_echo::error::RecallError> {
    use recall_echo::bench::writes::{self, Extractor};

    match provider {
        Some(name) => {
            let memory_dir = root.join("memory");
            let (llm, _) =
                recall_echo::llm_provider::create_provider(&memory_dir, Some(&name), model)?;
            writes::run_repeat(
                script,
                root,
                repeat,
                Extractor::Live(llm.as_ref()),
                pipeline,
                known_facts_context,
                known_match,
                consolidate,
                drop_restatements,
                consolidate_links,
                ground_component_edges,
                consolidate_components,
                recover_turns,
                consolidate_key_rename,
                consolidate_replaces_cue,
                consolidate_replaces_departure,
                consolidate_orphan_sweep,
                consolidate_rule_fragments,
                consolidate_retire_component,
                known_key_binding,
                opt_outs,
            )
            .await
        }
        None => {
            writes::run_repeat(
                script,
                root,
                repeat,
                Extractor::Scripted,
                pipeline,
                known_facts_context,
                known_match,
                consolidate,
                drop_restatements,
                consolidate_links,
                ground_component_edges,
                consolidate_components,
                recover_turns,
                consolidate_key_rename,
                consolidate_replaces_cue,
                consolidate_replaces_departure,
                consolidate_orphan_sweep,
                consolidate_rule_fragments,
                consolidate_retire_component,
                known_key_binding,
                opt_outs,
            )
            .await
        }
    }
}

#[cfg(feature = "bench")]
enum Scratch {
    Kept(PathBuf),
    Temporary(PathBuf),
}

#[cfg(feature = "bench")]
impl Scratch {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::Kept(path) | Self::Temporary(path) => path,
        }
    }
}

#[cfg(feature = "bench")]
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Self::Temporary(path) = self {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(feature = "bench")]
fn temporary_work_dir() -> Result<PathBuf, recall_echo::error::RecallError> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path =
        std::env::temp_dir().join(format!("recall-echo-writes-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&path)?;
    Ok(path)
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

/// Resolve pulse root for init, preferring Claude Code directory.
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Every subcommand that takes the root as a flag, split around where the
    /// flag goes: `(before, after)`.
    const FLAGGED: [(&[&str], &[&str]); 7] = [
        (&["archive-session"], &[]),
        (&["archive", "--all-unarchived"], &[]),
        (&["checkpoint", "--trigger", "precompact"], &[]),
        (&["config"], &["show"]),
        (&["mcp"], &[]),
        (&["what-do-you-know"], &[]),
        (&["graph"], &["status"]),
    ];

    fn parse_root(flag: &str, before: &[&str], after: &[&str]) -> Option<PathBuf> {
        let argv = std::iter::once("recall-echo")
            .chain(before.iter().copied())
            .chain([flag, "/srv/pulse"])
            .chain(after.iter().copied());
        let cli =
            Cli::try_parse_from(argv).unwrap_or_else(|err| panic!("{flag} with {before:?}: {err}"));
        match cli.command.expect("a subcommand") {
            Commands::ArchiveSession { pulse_root }
            | Commands::Archive { pulse_root, .. }
            | Commands::Checkpoint { pulse_root, .. }
            | Commands::Config { pulse_root, .. }
            | Commands::Mcp { pulse_root }
            | Commands::WhatDoYouKnow { pulse_root, .. }
            | Commands::Graph { pulse_root, .. } => pulse_root,
            _ => panic!("{before:?} is not a command with a root flag"),
        }
    }

    #[test]
    fn every_root_flag_is_spelled_pulse_root() {
        for (before, after) in FLAGGED {
            assert_eq!(
                parse_root("--pulse-root", before, after),
                Some(PathBuf::from("/srv/pulse")),
                "{before:?}"
            );
        }
    }

    /// Hooks and MCP registrations written before 4.6.0 carry `--entity-root`
    /// and must keep parsing to the same root.
    #[test]
    fn the_pre_4_6_entity_root_flag_still_parses() {
        for (before, after) in FLAGGED {
            assert_eq!(
                parse_root("--entity-root", before, after),
                Some(PathBuf::from("/srv/pulse")),
                "{before:?}"
            );
        }
    }

    /// The old spelling is accepted, not advertised.
    #[test]
    fn help_shows_pulse_root_and_hides_entity_root() {
        let mut cli = Cli::command();
        for name in ["archive-session", "checkpoint", "config", "mcp", "graph"] {
            let help = cli
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("{name} subcommand"))
                .render_long_help()
                .to_string();
            assert!(help.contains("--pulse-root"), "{name}: {help}");
            assert!(!help.contains("entity-root"), "{name}: {help}");
        }
    }

    #[test]
    fn the_command_line_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_links_is_its_own_bench_flag() {
        let links_only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-links",
        ])
        .expect("parse links");
        match links_only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        ..
                    },
            } => {
                assert!(consolidate_links);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
                assert!(!ground_component_edges);
            }
            _ => panic!("bench writes"),
        }

        let both = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-hold",
            "--consolidate-links",
        ])
        .expect("parse hold and links");
        match both.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        drop_restatements,
                        ..
                    },
            } => {
                assert!(consolidate_hold);
                assert!(consolidate_links);
                assert!(!consolidate);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
            }
            _ => panic!("bench writes"),
        }
    }

    #[cfg(feature = "bench")]
    #[test]
    fn ground_component_edges_is_its_own_bench_flag() {
        let parsed = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--ground-component-edges",
        ])
        .expect("parse ground");
        match parsed.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        ..
                    },
            } => {
                assert!(ground_component_edges);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--ground-component-edges"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_components_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-components",
        ])
        .expect("parse components");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        ..
                    },
            } => {
                assert!(consolidate_components);
                assert!(!ground_component_edges);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-components"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn recover_turns_is_its_own_bench_flag() {
        let parsed = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--recover-turns",
        ])
        .expect("parse recover");
        match parsed.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        consolidate_key_rename,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        recover_turns,
                        ..
                    },
            } => {
                assert!(recover_turns);
                assert!(!consolidate_key_rename);
                assert!(!ground_component_edges);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--recover-turns"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_key_rename_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-key-rename",
        ])
        .expect("parse key rename");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        consolidate_key_rename,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        recover_turns,
                        ..
                    },
            } => {
                assert!(consolidate_key_rename);
                assert!(!recover_turns);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
                assert!(!ground_component_edges);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-key-rename"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_replaces_cue_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-replaces-cue",
        ])
        .expect("parse replaces cue");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        consolidate_key_rename,
                        consolidate_replaces_cue,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        recover_turns,
                        ..
                    },
            } => {
                assert!(consolidate_replaces_cue);
                assert!(!consolidate_key_rename);
                assert!(!recover_turns);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
                assert!(!ground_component_edges);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-replaces-cue"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_replaces_departure_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-replaces-departure",
        ])
        .expect("parse replaces departure");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        consolidate_key_rename,
                        consolidate_replaces_cue,
                        consolidate_replaces_departure,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        recover_turns,
                        ..
                    },
            } => {
                assert!(consolidate_replaces_departure);
                assert!(!consolidate_replaces_cue);
                assert!(!consolidate_key_rename);
                assert!(!recover_turns);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!consolidate_components);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
                assert!(!ground_component_edges);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-replaces-departure"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_orphan_sweep_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-orphan-sweep",
        ])
        .expect("parse orphan sweep");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_hold,
                        consolidate_links,
                        consolidate_components,
                        consolidate_key_rename,
                        consolidate_replaces_cue,
                        consolidate_replaces_departure,
                        consolidate_orphan_sweep,
                        drop_restatements,
                        known_match,
                        known_facts_context,
                        ground_component_edges,
                        recover_turns,
                        ..
                    },
            } => {
                assert!(consolidate_orphan_sweep);
                assert!(!consolidate_replaces_departure);
                assert!(!consolidate_replaces_cue);
                assert!(!consolidate);
                assert!(!consolidate_hold);
                assert!(!consolidate_links);
                assert!(!consolidate_components);
                assert!(!consolidate_key_rename);
                assert!(!drop_restatements);
                assert!(!known_match);
                assert!(!known_facts_context);
                assert!(!ground_component_edges);
                assert!(!recover_turns);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-orphan-sweep"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_rule_fragments_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-rule-fragments",
        ])
        .expect("parse rule fragments");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_orphan_sweep,
                        consolidate_rule_fragments,
                        consolidate_replaces_cue,
                        ground_component_edges,
                        known_facts_context,
                        ..
                    },
            } => {
                assert!(consolidate_rule_fragments);
                assert!(!consolidate_orphan_sweep);
                assert!(!consolidate);
                assert!(!consolidate_replaces_cue);
                assert!(!ground_component_edges);
                assert!(!known_facts_context);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-rule-fragments"), "{help}");
        assert!(help.contains("--consolidate-orphan-sweep"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_retire_component_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate-retire-component",
        ])
        .expect("parse retire component");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        consolidate_orphan_sweep,
                        consolidate_rule_fragments,
                        consolidate_retire_component,
                        ground_component_edges,
                        known_facts_context,
                        ..
                    },
            } => {
                assert!(consolidate_retire_component);
                assert!(!consolidate_rule_fragments);
                assert!(!consolidate_orphan_sweep);
                assert!(!consolidate);
                assert!(!ground_component_edges);
                assert!(!known_facts_context);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--consolidate-retire-component"), "{help}");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn known_key_binding_is_its_own_bench_flag() {
        let only = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--known-key-binding",
        ])
        .expect("parse known key binding");
        match only.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        known_facts_context,
                        known_key_binding,
                        known_match,
                        consolidate_replaces_departure,
                        ..
                    },
            } => {
                assert!(known_key_binding);
                assert!(!known_facts_context);
                assert!(!known_match);
                assert!(!consolidate);
                assert!(!consolidate_replaces_departure);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let bench = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes");
        let help = bench.render_long_help().to_string();
        assert!(help.contains("--known-key-binding"), "{help}");
        assert!(help.contains("--consolidate-replaces-departure"), "{help}");
    }
    #[cfg(feature = "bench")]
    const QUESTION_REQUIRED: &str =
        "config: exactly one of --question or --question-stdin is required";

    #[cfg(feature = "bench")]
    const UNKNOWN_PROVIDER: &str = concat!(
        "config: unknown provider: nonesuch (use 'anthropic', 'openai', 'claude-code', ",
        "'gemini', 'grok', 'codex', or 'cli' with a [llm.cli] section)"
    );

    #[cfg(feature = "bench")]
    #[test]
    fn bench_answer_rejects_neither_and_both_question_sources() {
        let neither = run_bench(answer_command(None, false, None)).expect_err("neither");
        assert_eq!(neither.to_string(), QUESTION_REQUIRED);
        let both = run_bench(answer_command(Some("alpha"), true, None)).expect_err("both");
        assert_eq!(both.to_string(), QUESTION_REQUIRED);
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_answer_rejects_an_unknown_provider() {
        let err = run_bench(answer_command(Some("alpha"), false, Some("nonesuch")))
            .expect_err("provider");
        assert_eq!(err.to_string(), UNKNOWN_PROVIDER);
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_answer_reports_the_question_error_before_the_provider() {
        let err = run_bench(answer_command(None, false, Some("nonesuch"))).expect_err("order");
        assert_eq!(err.to_string(), QUESTION_REQUIRED);
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_writes_rejects_repeat_zero() {
        let err = run_bench(writes_command(
            history_script().as_path(),
            None,
            0,
            BenchSwitch::none(),
        ))
        .expect_err("repeat");
        assert_eq!(err.to_string(), "config: repeat must be at least 1");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_ingest_missing_conversation_is_an_io_error() {
        let err = run_bench(BenchCommands::Ingest {
            pulse_root: PathBuf::from("/x"),
            conv_json: Some(PathBuf::from("/x/nonesuch.json")),
            provider: None,
            model: None,
            no_llm: true,
        })
        .expect_err("missing conversation");
        assert_eq!(
            err.to_string(),
            "io: No such file or directory (os error 2)"
        );
    }

    /// Pins the scratch config each bench `writes` switch produces.
    ///
    /// `RECALL_ECHO_BLESS=1` rewrites the fixture from the current code. The
    /// fixture is generated once and then compared byte for byte.
    #[cfg(feature = "bench")]
    #[test]
    fn bench_writes_switches_match_the_golden() {
        let cases = switch_cases();
        assert_eq!(cases.len(), 34);
        let rendered = render_switch_cases(&cases);
        let path = switch_golden_path();
        if std::env::var_os("RECALL_ECHO_BLESS").is_some() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture directory");
            }
            std::fs::write(&path, &rendered).expect("write golden");
        }
        let expected = std::fs::read_to_string(&path).expect("writes switch golden");
        assert_eq!(rendered, expected);
    }

    #[cfg(feature = "bench")]
    fn answer_command(
        question: Option<&str>,
        question_stdin: bool,
        provider: Option<&str>,
    ) -> BenchCommands {
        BenchCommands::Answer {
            pulse_root: PathBuf::from("/x"),
            question: question.map(str::to_string),
            question_stdin,
            provider: provider.map(str::to_string),
            model: None,
            graph_depth: 2,
            graph_limit: 20,
            archive_top_k: 5,
            episode_top_k: 20,
            episode_char_budget: 28000,
            no_episodes: false,
        }
    }

    #[cfg(feature = "bench")]
    fn history_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/writes/history.toml")
    }

    #[cfg(feature = "bench")]
    fn switch_golden_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bench/writes_switches.golden")
    }

    /// Opt-outs for the measured stack, in the order the run config writes them.
    #[cfg(feature = "bench")]
    const OPT_OUT_NAMES: [&str; 14] = [
        "no-known-facts-context",
        "no-consolidate",
        "no-consolidate-hold",
        "no-drop-restatements",
        "no-consolidate-links",
        "no-consolidate-components",
        "no-ground-component-edges",
        "no-consolidate-key-rename",
        "no-consolidate-replaces-cue",
        "no-consolidate-replaces-departure",
        "no-known-key-binding",
        "no-consolidate-orphan-sweep",
        "no-consolidate-rule-fragments",
        "no-consolidate-retire-component",
    ];

    /// Sixteen switches, in the order `run_bench` forwards them.
    #[cfg(feature = "bench")]
    const SWITCH_NAMES: [&str; 16] = [
        "known_facts_context",
        "known_match",
        "consolidate",
        "consolidate_hold",
        "consolidate_links",
        "consolidate_components",
        "consolidate_key_rename",
        "consolidate_replaces_cue",
        "consolidate_replaces_departure",
        "consolidate_orphan_sweep",
        "consolidate_rule_fragments",
        "consolidate_retire_component",
        "known_key_binding",
        "drop_restatements",
        "ground_component_edges",
        "recover_turns",
    ];

    #[cfg(feature = "bench")]
    #[derive(Clone, Copy)]
    struct BenchSwitch {
        flags: [bool; 16],
        opt_outs: [bool; 14],
    }

    #[cfg(feature = "bench")]
    impl BenchSwitch {
        fn none() -> Self {
            Self {
                flags: [false; 16],
                opt_outs: [false; 14],
            }
        }

        fn only(index: usize) -> Self {
            let mut flags = [false; 16];
            flags[index] = true;
            Self {
                flags,
                opt_outs: [false; 14],
            }
        }

        fn all() -> Self {
            Self {
                flags: [true; 16],
                opt_outs: [false; 14],
            }
        }

        fn opt_out(index: usize) -> Self {
            let mut opt_outs = [false; 14];
            opt_outs[index] = true;
            Self {
                flags: [false; 16],
                opt_outs,
            }
        }

        fn all_opt_out() -> Self {
            Self {
                flags: [false; 16],
                opt_outs: [true; 14],
            }
        }
    }

    #[cfg(feature = "bench")]
    fn switch_cases() -> Vec<(&'static str, BenchSwitch)> {
        let mut cases = vec![("none", BenchSwitch::none())];
        for (index, name) in SWITCH_NAMES.iter().enumerate() {
            cases.push((name, BenchSwitch::only(index)));
        }
        let mut both = BenchSwitch::none();
        both.flags[2] = true;
        both.flags[3] = true;
        cases.push(("consolidate+consolidate_hold", both));
        cases.push(("all", BenchSwitch::all()));
        for (index, name) in OPT_OUT_NAMES.iter().enumerate() {
            cases.push((name, BenchSwitch::opt_out(index)));
        }
        cases.push(("all-off", BenchSwitch::all_opt_out()));
        cases
    }

    #[cfg(feature = "bench")]
    fn render_switch_cases(cases: &[(&str, BenchSwitch)]) -> String {
        let script = history_script();
        let mut out = String::new();
        for (name, flags) in cases {
            out.push_str(&render_switch_case(&script, name, *flags));
            out.push('\n');
        }
        out
    }

    #[cfg(feature = "bench")]
    fn render_switch_case(script: &std::path::Path, name: &str, flags: BenchSwitch) -> String {
        let work = tempfile::tempdir().expect("work dir");
        run_bench(writes_command(script, Some(work.path()), 1, flags)).expect(name);
        let config = work.path().join("memory").join(".recall-echo.toml");
        let body = match std::fs::read_to_string(&config) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => "absent\n".to_string(),
            Err(err) => panic!("read {name} config: {err}"),
        };
        format!("{name}\n{body}")
    }

    #[cfg(feature = "bench")]
    fn writes_command(
        script: &std::path::Path,
        work_dir: Option<&std::path::Path>,
        repeat: usize,
        flags: BenchSwitch,
    ) -> BenchCommands {
        let f = flags.flags;
        let o = flags.opt_outs;
        BenchCommands::Writes {
            script: script.to_path_buf(),
            work_dir: work_dir.map(std::path::Path::to_path_buf),
            provider: None,
            model: None,
            repeat,
            pipeline: None,
            known_facts_context: f[0],
            known_match: f[1],
            consolidate: f[2],
            consolidate_hold: f[3],
            consolidate_links: f[4],
            consolidate_components: f[5],
            consolidate_key_rename: f[6],
            consolidate_replaces_cue: f[7],
            consolidate_replaces_departure: f[8],
            consolidate_orphan_sweep: f[9],
            consolidate_rule_fragments: f[10],
            consolidate_retire_component: f[11],
            known_key_binding: f[12],
            drop_restatements: f[13],
            ground_component_edges: f[14],
            recover_turns: f[15],
            no_known_facts_context: o[0],
            no_consolidate: o[1],
            no_consolidate_hold: o[2],
            no_drop_restatements: o[3],
            no_consolidate_links: o[4],
            no_consolidate_components: o[5],
            no_ground_component_edges: o[6],
            no_consolidate_key_rename: o[7],
            no_consolidate_replaces_cue: o[8],
            no_consolidate_replaces_departure: o[9],
            no_known_key_binding: o[10],
            no_consolidate_orphan_sweep: o[11],
            no_consolidate_rule_fragments: o[12],
            no_consolidate_retire_component: o[13],
        }
    }

    #[cfg(feature = "bench")]
    #[test]
    fn an_opt_out_is_its_own_flag_and_rejects_the_matching_on_flag() {
        let err = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--consolidate",
            "--no-consolidate",
        ]);
        assert!(err.is_err(), "on and opt-out together should be rejected");

        let parsed = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "writes",
            "--script",
            "facts.toml",
            "--no-consolidate",
            "--no-drop-restatements",
        ])
        .expect("parse opt-outs");
        match parsed.command.expect("command") {
            Commands::Bench {
                command:
                    BenchCommands::Writes {
                        consolidate,
                        no_consolidate,
                        drop_restatements,
                        no_drop_restatements,
                        no_known_facts_context,
                        ..
                    },
            } => {
                assert!(!consolidate);
                assert!(no_consolidate);
                assert!(!drop_restatements);
                assert!(no_drop_restatements);
                assert!(!no_known_facts_context);
            }
            _ => panic!("bench writes"),
        }

        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("bench")
            .expect("bench")
            .find_subcommand_mut("writes")
            .expect("writes")
            .render_long_help()
            .to_string();
        for flag in OPT_OUT_NAMES {
            assert!(
                help.contains(&format!("--{flag}")),
                "{flag} missing from {help}"
            );
        }
    }

    #[cfg(feature = "bench")]
    #[test]
    fn on_off_selects_each_switch_enum() {
        use recall_echo::bench::writes::{
            ConsolidateComponents, ConsolidateKeyRename, ConsolidateLinks, ConsolidateOrphanSweep,
            ConsolidateReplacesCue, ConsolidateReplacesDeparture, ConsolidateRetireComponent,
            ConsolidateRuleFragments, DropRestatements, GroundComponentEdges, KnownFactsContext,
            KnownKeyBinding, KnownMatch, RecoverTurns,
        };
        assert_on_off(KnownFactsContext::On, KnownFactsContext::Off);
        assert_on_off(KnownMatch::On, KnownMatch::Off);
        assert_on_off(DropRestatements::On, DropRestatements::Off);
        assert_on_off(ConsolidateLinks::On, ConsolidateLinks::Off);
        assert_on_off(GroundComponentEdges::On, GroundComponentEdges::Off);
        assert_on_off(ConsolidateComponents::On, ConsolidateComponents::Off);
        assert_on_off(RecoverTurns::On, RecoverTurns::Off);
        assert_on_off(ConsolidateKeyRename::On, ConsolidateKeyRename::Off);
        assert_on_off(ConsolidateReplacesCue::On, ConsolidateReplacesCue::Off);
        assert_on_off(
            ConsolidateReplacesDeparture::On,
            ConsolidateReplacesDeparture::Off,
        );
        assert_on_off(ConsolidateOrphanSweep::On, ConsolidateOrphanSweep::Off);
        assert_on_off(ConsolidateRuleFragments::On, ConsolidateRuleFragments::Off);
        assert_on_off(
            ConsolidateRetireComponent::On,
            ConsolidateRetireComponent::Off,
        );
        assert_on_off(KnownKeyBinding::On, KnownKeyBinding::Off);
    }

    #[cfg(feature = "bench")]
    fn assert_on_off<T: Copy + PartialEq + std::fmt::Debug>(on: T, off: T) {
        assert_eq!(on_off(true, on, off), on);
        assert_eq!(on_off(false, on, off), off);
    }

    #[cfg(feature = "bench")]
    #[test]
    fn consolidate_mode_lets_hold_win() {
        use recall_echo::bench::writes::Consolidate;
        assert_eq!(consolidate_mode(true, false), Consolidate::Hold);
        assert_eq!(consolidate_mode(false, true), Consolidate::On);
        assert_eq!(consolidate_mode(true, true), Consolidate::Hold);
        assert_eq!(consolidate_mode(false, false), Consolidate::Off);
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_question_returns_the_given_question() {
        let text = bench_question(Some("alpha".to_string()), false).expect("question");
        assert_eq!(text, "alpha");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_question_reads_stdin_only_when_asked() {
        let from_stdin =
            bench_question_with(None, true, || Ok("alpha".to_string())).expect("stdin");
        assert_eq!(from_stdin, "alpha");
        let from_flag =
            bench_question_with(Some("alpha".to_string()), false, || panic!("read")).expect("flag");
        assert_eq!(from_flag, "alpha");
    }

    #[cfg(feature = "bench")]
    #[test]
    fn answer_request_maps_every_flag() -> Result<(), recall_echo::error::RecallError> {
        use recall_echo::bench::AnswerOpts;
        use recall_echo::config::Provider;

        let full = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "answer",
            "--pulse-root",
            "/x",
            "--question",
            "alpha",
            "--provider",
            "cli",
            "--model",
            "alpha",
            "--graph-depth",
            "1",
            "--graph-limit",
            "2",
            "--archive-top-k",
            "3",
            "--episode-top-k",
            "4",
            "--episode-char-budget",
            "5",
            "--no-episodes",
        ])
        .expect("parse full");
        let (root, question, opts) = answer_request(bench_subcommand(full))?;
        assert_eq!(root, PathBuf::from("/x"));
        assert_eq!(question, "alpha");
        assert_eq!(opts.graph_depth, 1);
        assert_eq!(opts.graph_limit, 2);
        assert_eq!(opts.archive_top_k, 3);
        assert_eq!(opts.episode_top_k, 4);
        assert_eq!(opts.episode_char_budget, 5);
        assert!(!opts.include_episodes);
        assert_eq!(
            opts.provider_override,
            Some(Provider::from_str_loose("cli")?)
        );
        assert_eq!(opts.model_override.as_deref(), Some("alpha"));
        assert_eq!(opts.max_tokens, AnswerOpts::default().max_tokens);

        let bare = Cli::try_parse_from([
            "recall-echo",
            "bench",
            "answer",
            "--pulse-root",
            "/x",
            "--question",
            "alpha",
        ])
        .expect("parse bare");
        let (_root, question, opts) = answer_request(bench_subcommand(bare))?;
        assert_eq!(question, "alpha");
        assert_eq!(opts.graph_depth, 2);
        assert_eq!(opts.graph_limit, 20);
        assert_eq!(opts.archive_top_k, 5);
        assert_eq!(opts.episode_top_k, 20);
        assert_eq!(opts.episode_char_budget, 28000);
        assert!(opts.include_episodes);
        assert!(opts.provider_override.is_none());
        assert!(opts.model_override.is_none());
        Ok(())
    }

    #[cfg(feature = "bench")]
    fn bench_subcommand(cli: Cli) -> BenchCommands {
        match cli.command.expect("command") {
            Commands::Bench { command } => command,
            _ => panic!("bench"),
        }
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_recover_replay_reports_an_unopenable_store() {
        let dir = tempfile::tempdir().expect("temp");
        let memory = dir.path().join("store");
        std::fs::write(&memory, "alpha").expect("file");
        assert!(bench_recover_replay(memory).is_err());
    }

    #[cfg(feature = "bench")]
    #[test]
    fn bench_ingest_builds_the_provider_unless_no_llm() {
        let dir = tempfile::tempdir().expect("temp");
        let conv = dir.path().join("conv.json");
        std::fs::write(
            &conv,
            r#"{"sample_id":"alpha","speaker_a":"alpha","speaker_b":"alpha","sessions":[]}"#,
        )
        .expect("conversation");
        let err = run_bench(BenchCommands::Ingest {
            pulse_root: dir.path().to_path_buf(),
            conv_json: Some(conv),
            provider: Some("nonesuch".into()),
            model: None,
            no_llm: false,
        })
        .expect_err("provider");
        assert_eq!(err.to_string(), UNKNOWN_PROVIDER);
    }
}
