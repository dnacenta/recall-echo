//! CLI handlers for `recall-echo config show` and `recall-echo config set`.

use std::path::Path;

use crate::config::{self, Provider};
use crate::error::RecallError;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// Display current configuration.
pub fn show(memory_dir: &Path) -> Result<(), RecallError> {
    let cfg = config::load(memory_dir);
    let path = config::config_path(memory_dir);
    let exists = path.exists();

    eprintln!("{BOLD}recall-echo config{RESET}");
    if exists {
        eprintln!("{DIM}{}{RESET}\n", path.display());
    } else {
        eprintln!("{DIM}(no config file — using defaults){RESET}\n");
    }

    // Ephemeral
    eprintln!("{BOLD}[ephemeral]{RESET}");
    eprintln!("  max_entries = {}", cfg.ephemeral.max_entries);

    // LLM
    eprintln!("\n{BOLD}[llm]{RESET}");
    let provider_label = match &cfg.llm.provider {
        Provider::Anthropic => "anthropic",
        Provider::Openai => "openai (ollama)",
        Provider::ClaudeCode => "claude-code",
    };
    eprintln!("  provider = {provider_label}");
    eprintln!(
        "  model    = {} {DIM}({}){RESET}",
        cfg.llm.resolved_model(),
        if cfg.llm.model.is_empty() {
            "default"
        } else {
            "custom"
        }
    );
    eprintln!(
        "  api_base = {} {DIM}({}){RESET}",
        cfg.llm.resolved_api_base(),
        if cfg.llm.api_base.is_empty() {
            "default"
        } else {
            "custom"
        }
    );

    // Pipeline
    if let Some(ref pipeline) = cfg.pipeline {
        eprintln!("\n{BOLD}[pipeline]{RESET}");
        eprintln!(
            "  docs_dir  = {}",
            pipeline
                .docs_dir
                .as_deref()
                .unwrap_or("{DIM}(not set){RESET}")
        );
        eprintln!("  auto_sync = {}", pipeline.auto_sync.unwrap_or(false));
    }

    Ok(())
}

/// Set a config key and save.
pub fn set(memory_dir: &Path, key: &str, value: &str) -> Result<(), RecallError> {
    let mut cfg = config::load(memory_dir);
    cfg.set_key(key, value)?;
    config::save(memory_dir, &cfg)?;

    eprintln!("{GREEN}✓{RESET} Set {BOLD}{key}{RESET} = {BOLD}{value}{RESET}");

    // Show resolved values after setting provider
    if key == "llm.provider" || key == "provider" {
        eprintln!("  model    → {}", cfg.llm.resolved_model());
        eprintln!("  api_base → {}", cfg.llm.resolved_api_base());
    }

    Ok(())
}
