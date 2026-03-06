use std::path::PathBuf;

use clap::{Parser, Subcommand};

use recall_echo::{distill, init, paths, search, status};

#[derive(Parser)]
#[command(
    name = "recall-echo",
    about = "Persistent three-layer memory system for pulse-null entities",
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
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        None => {
            // Default: show status
            status::run()
        }
        Some(Commands::Init { entity_root }) => {
            let root = resolve_entity_root(entity_root);
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
    };

    if let Err(e) = result {
        eprintln!("\x1b[31m✗\x1b[0m {e}");
        std::process::exit(1);
    }
}

fn resolve_entity_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| paths::entity_root().unwrap_or_else(|_| PathBuf::from(".")))
}
