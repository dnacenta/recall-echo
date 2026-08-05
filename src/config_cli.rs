// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI handlers for `recall-echo config show` and `recall-echo config set`.

use std::path::Path;

use crate::cli_provider::CliSpec;
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
        Provider::Openai => "openai (ollama)".to_string(),
        other => other.to_string(),
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
    if cfg.llm.provider.is_cli() {
        show_cli_section(&cfg.llm);
    } else {
        eprintln!(
            "  api_base = {} {DIM}({}){RESET}",
            cfg.llm.resolved_api_base(),
            if cfg.llm.api_base.is_empty() {
                "default"
            } else {
                "custom"
            }
        );
    }

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

/// Show the resolved agent-CLI call, so a misconfigured vendor is visible
/// before it is spawned rather than after it fails.
fn show_cli_section(llm: &config::LlmSection) {
    eprintln!("\n{BOLD}[llm.cli]{RESET}");
    match CliSpec::resolve(&llm.provider, &llm.cli) {
        Ok(spec) => {
            let preset = llm
                .cli
                .preset
                .or_else(|| llm.provider.default_cli_preset())
                .map_or_else(|| "custom".to_string(), |p| p.to_string());
            eprintln!("  preset   = {preset}");
            eprintln!("  command  = {}", spec.resolve_command());
            eprintln!(
                "  timeout  = {}",
                spec.timeout
                    .map_or_else(|| "none".to_string(), |t| format!("{}s", t.as_secs()))
            );
            eprintln!("  output   = {}", spec.output_mode);
            let result = if spec.result_json_paths.is_empty() {
                "raw stdout".to_string()
            } else {
                spec.result_json_paths.to_string()
            };
            eprintln!("  result   = {result}");
            if !spec.ndjson_match.is_empty() {
                eprintln!("  match    = {}", spec.ndjson_match);
            }
            eprintln!(
                "  {DIM}{}{RESET}",
                spec.argv_preview(&spec.resolve_model(&llm.model))
            );
        }
        Err(err) => eprintln!("  {err}"),
    }
}

/// Set a config key and save.
pub fn set(memory_dir: &Path, key: &str, value: &str) -> Result<(), RecallError> {
    let mut cfg = config::load(memory_dir);
    cfg.set_key(key, value)?;
    config::save(memory_dir, &cfg)?;

    eprintln!("{GREEN}✓{RESET} Set {BOLD}{key}{RESET} = {BOLD}{value}{RESET}");

    // Show what the new provider resolves to — a CLI provider has no API base,
    // and what matters instead is the call it will make.
    if key == "llm.provider" || key == "provider" {
        if cfg.llm.provider.is_cli() {
            match CliSpec::resolve(&cfg.llm.provider, &cfg.llm.cli) {
                Ok(spec) => eprintln!(
                    "  command  → {}",
                    spec.argv_preview(&spec.resolve_model(&cfg.llm.model))
                ),
                Err(err) => eprintln!("  {err}"),
            }
        } else {
            eprintln!("  model    → {}", cfg.llm.resolved_model());
            eprintln!("  api_base → {}", cfg.llm.resolved_api_base());
        }
    }

    Ok(())
}
