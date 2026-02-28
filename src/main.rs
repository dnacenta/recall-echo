mod archive;
mod checkpoint;
mod consume;
mod frontmatter;
mod init;
mod paths;
mod promote;
mod status;

use clap::{Parser, Subcommand};

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
    /// Create an archive checkpoint
    Checkpoint {
        /// Trigger type: precompact or session-end
        #[arg(long, default_value = "precompact")]
        trigger: String,
        /// Brief session context
        #[arg(long, default_value = "")]
        context: String,
    },
    /// Promote EPHEMERAL.md into an archive log
    Promote {
        /// Override context field
        #[arg(long, default_value = "")]
        context: String,
    },
    /// Consume EPHEMERAL.md at session start (outputs content, clears file)
    Consume,
    /// Memory system health check
    Status,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Some(Commands::Init) | None => init::run(),
        Some(Commands::Checkpoint { trigger, context }) => checkpoint::run(&trigger, &context),
        Some(Commands::Promote { context }) => promote::run(&context),
        Some(Commands::Consume) => consume::run(),
        Some(Commands::Status) => status::run(),
    };

    if let Err(e) = result {
        eprintln!("\x1b[31m✗\x1b[0m {e}");
        std::process::exit(1);
    }
}
