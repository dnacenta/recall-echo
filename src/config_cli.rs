// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI handlers for `recall-echo config show` and `recall-echo config set`.

use std::path::Path;

use crate::cli_provider::CliSpec;
use crate::config::{self, Provider};
use crate::error::RecallError;
use crate::theme::{BOLD, DIM, GOOD, RESET};

/// Display current configuration.
pub fn show(memory_dir: &Path) -> Result<(), RecallError> {
    let cfg = config::try_load(memory_dir)?;
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
    eprintln!("  provider = {}", cfg.llm.provider);
    let note = |empty: bool, required: bool| {
        if !empty {
            "custom"
        } else if required {
            "required"
        } else {
            "default"
        }
    };
    let openai = cfg.llm.provider == Provider::Openai;
    eprintln!(
        "  model    = {} {DIM}({}){RESET}",
        cfg.llm.resolved_model(),
        note(cfg.llm.model.is_empty(), openai)
    );
    if cfg.llm.provider.is_cli() {
        show_cli_section(&cfg.llm);
    } else {
        eprintln!(
            "  api_base = {} {DIM}({}){RESET}",
            cfg.llm.resolved_api_base(),
            note(cfg.llm.api_base.is_empty(), openai)
        );
    }

    show_capture_section(&cfg.capture);
    show_extraction_section(&cfg.extraction);
    show_serve_section(&cfg.serve);

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

/// Which agent CLIs the daemon sweeps transcripts from.
///
/// Shown even at its defaults: "sessions are being imported from every CLI on
/// this machine" is a thing a user debugging their setup needs to know, and it
/// is invisible in a config file that never mentions it.
fn show_capture_section(capture: &config::CaptureSection) {
    eprintln!("\n{BOLD}[capture]{RESET}");
    eprintln!("  enabled     = {}", capture.enabled);
    let sources = match &capture.sources {
        Some(sources) if !sources.is_empty() => sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        _ => "(every CLI with sessions on this machine)".to_string(),
    };
    eprintln!("  sources     = {sources}");
    eprintln!(
        "  settle_secs = {} {DIM}(a transcript must be this quiet to count as finished){RESET}",
        capture.settle_secs
    );
}

/// Background entity extraction inside the graph daemon.
fn show_extraction_section(extraction: &config::ExtractionSection) {
    eprintln!("\n{BOLD}[extraction]{RESET}");
    eprintln!("  background_enabled = {}", extraction.background_enabled);
    eprintln!(
        "  idle_after_secs    = {} {DIM}(quiet period before a batch starts){RESET}",
        extraction.idle_after_secs
    );
    eprintln!(
        "  batch_size         = {} {DIM}(archives per batch){RESET}",
        extraction.batch_size
    );
}

/// Where the graph daemon listens and how long it lives.
fn show_serve_section(serve: &config::ServeSection) {
    eprintln!("\n{BOLD}[serve]{RESET}");
    let socket = serve
        .socket_path
        .as_deref()
        .filter(|path| !path.trim().is_empty());
    match socket {
        Some(path) => eprintln!("  socket_path       = {path}"),
        None => eprintln!("  socket_path       = {DIM}(derived from the memory directory){RESET}"),
    }
    let idle = match serve.idle_timeout_secs {
        0 => "0 (never idle-shuts-down)".to_string(),
        secs => format!("{secs}"),
    };
    eprintln!("  idle_timeout_secs = {idle}");
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
            // Whether this CLI's token bill will be measured or estimated —
            // worth knowing before the number is read, not after.
            let usage = if spec.usage_input_paths.is_empty() && spec.usage_output_paths.is_empty() {
                format!("{DIM}estimated (this CLI reports no token counts){RESET}")
            } else {
                format!("{} / {}", spec.usage_input_paths, spec.usage_output_paths)
            };
            eprintln!("  usage    = {usage}");
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
    // A file `try_load` rejects is left untouched: saving defaults would erase it.
    let mut cfg = config::try_load(memory_dir)?;
    cfg.set_key(key, value)?;
    config::save(memory_dir, &cfg)?;
    eprintln!("{GOOD}✓{RESET} Set {BOLD}{key}{RESET} = {BOLD}{value}{RESET}");
    report_set_provider(&cfg, key);
    Ok(())
}

fn report_set_provider(cfg: &config::Config, key: &str) {
    if key != "llm.provider" && key != "provider" {
        return;
    }
    if cfg.llm.provider.is_cli() {
        report_cli_after_set(cfg);
        return;
    }
    eprintln!("  model    → {}", cfg.llm.resolved_model());
    eprintln!("  api_base → {}", cfg.llm.resolved_api_base());
}

fn report_cli_after_set(cfg: &config::Config) {
    match CliSpec::resolve(&cfg.llm.provider, &cfg.llm.cli) {
        Ok(spec) => eprintln!(
            "  command  → {}",
            spec.argv_preview(&spec.resolve_model(&cfg.llm.model))
        ),
        Err(err) => eprintln!("  {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_show_fails_on_a_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = config::config_path(dir.path());
        std::fs::write(&path, "this is not toml [[[\n").unwrap();
        let err = show(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn config_set_fails_and_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = config::config_path(dir.path());
        let body = "this is not toml [[[\n";
        std::fs::write(&path, body).unwrap();
        let err = set(dir.path(), "ephemeral.max_entries", "5").unwrap_err();
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }
}
