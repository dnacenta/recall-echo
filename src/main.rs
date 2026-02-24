mod init;

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
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Init) | None => {
            if let Err(e) = init::run() {
                eprintln!("\x1b[31m✗\x1b[0m {e}");
                std::process::exit(1);
            }
        }
    }
}
