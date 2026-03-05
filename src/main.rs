use clap::{Parser, Subcommand};

use recall_echo::{archive, checkpoint, consume, distill, init, search, status};

#[derive(Parser)]
#[command(
    name = "recall-echo",
    about = "Persistent memory for AI coding agents",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the memory system
    Init,
    /// Archive a session from JSONL transcript (SessionEnd hook)
    ArchiveSession,
    /// Archive operations
    Archive {
        /// Archive all unarchived JSONL transcripts
        #[arg(long)]
        all_unarchived: bool,
    },
    /// Create an archive checkpoint (PreCompact hook)
    Checkpoint {
        /// Trigger type: precompact or manual
        #[arg(long, default_value = "precompact")]
        trigger: String,
        /// Brief session context
        #[arg(long, default_value = "")]
        context: String,
    },
    /// Output EPHEMERAL.md at session start (PreToolUse hook)
    Consume,
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
    Distill,
    /// Memory system health check
    Status,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Some(Commands::Init) | None => init::run(),
        Some(Commands::ArchiveSession) => archive::run_from_hook(),
        Some(Commands::Archive { all_unarchived }) => {
            if all_unarchived {
                archive::archive_all_unarchived()
            } else {
                eprintln!("Use --all-unarchived to batch archive missed sessions.");
                Ok(())
            }
        }
        Some(Commands::Checkpoint { trigger, context }) => checkpoint::run(&trigger, &context),
        Some(Commands::Consume) => consume::run(),
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
        Some(Commands::Distill) => distill::run(),
        Some(Commands::Status) => status::run(),
    };

    if let Err(e) = result {
        eprintln!("\x1b[31m✗\x1b[0m {e}");
        std::process::exit(1);
    }
}
