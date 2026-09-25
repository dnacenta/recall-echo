// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! recall-echo — Persistent memory system with knowledge graph.
//!
//! A general-purpose persistent memory system for any LLM tool — Claude Code,
//! Ollama, or any provider. Features a four-layer memory architecture with
//! a knowledge graph (SurrealDB + fastembed) as Layer 0.
//!
//! # Architecture
//!
//! ```text
//! Transcript adapters (Claude Code, Codex, Grok, pulse-null Messages)
//!     → Conversation (universal internal format)
//!     → Archive pipeline (markdown + index + ephemeral + graph)
//! ```
//!
//! Sessions arrive either because the CLI told us (Claude Code's `SessionEnd`
//! hook) or because we read what it wrote ([`capture`], over [`transcript`]).
//!
//! # Features
//!
//! - `pulse-null` — Plugin integration for pulse-null pulses
//! - `llm` — HTTP-based LLM provider for entity extraction

pub mod agent_cli;
pub mod archive;
#[cfg(feature = "llm")]
pub mod archive_extract;
pub mod capture;
pub mod checkpoint;
pub mod cli_provider;
pub mod config;
pub mod config_cli;
pub mod consume;
pub mod conversation;
pub mod dashboard;
pub mod distill;
pub mod ephemeral;
pub mod error;
pub mod frontmatter;
pub mod init;
pub mod inspect_cli;
pub mod jsonl;
pub mod mcp;
pub mod paths;
pub mod search;
pub mod serve;
pub mod serve_capture;
pub mod serve_client;
#[cfg(feature = "llm")]
pub mod serve_extract;
mod serve_security;
pub mod status;
pub mod summarize;
pub mod tags;
pub mod theme;
pub mod transcript;

pub mod graph;
pub mod graph_bridge;
pub mod graph_cli;
#[cfg(feature = "llm")]
pub mod llm_provider;

#[cfg(feature = "pulse-null")]
pub mod pulse_null;

#[cfg(feature = "bench")]
pub mod bench;

#[cfg(feature = "self-update")]
pub mod update;

use std::fs;
use std::path::{Path, PathBuf};

pub use archive::SessionMetadata;
pub use summarize::ConversationSummary;

/// The recall-echo memory system.
///
/// All paths are derived from pulse_root:
/// ```text
/// {pulse_root}/memory/
/// ├── MEMORY.md
/// ├── EPHEMERAL.md
/// ├── ARCHIVE.md
/// ├── conversations/
/// └── graph/ (knowledge graph store)
/// ```
pub struct RecallEcho {
    pulse_root: PathBuf,
}

impl RecallEcho {
    /// Create a new RecallEcho instance with a specific pulse root directory.
    #[must_use]
    pub fn new(pulse_root: PathBuf) -> Self {
        Self { pulse_root }
    }

    /// Create a RecallEcho using the default path resolution
    /// (RECALL_ECHO_HOME, an initialised cwd, or the root `init` persisted).
    pub fn from_default() -> Result<Self, error::RecallError> {
        Ok(Self::new(paths::pulse_root()?))
    }

    /// Pulse root directory.
    #[must_use]
    pub fn pulse_root(&self) -> &Path {
        &self.pulse_root
    }

    /// Pulse root directory, under its pre-4.6.0 name.
    #[deprecated(since = "4.6.0", note = "renamed to `pulse_root`")]
    #[must_use]
    pub fn entity_root(&self) -> &Path {
        self.pulse_root()
    }

    /// Memory directory: {pulse_root}/memory/
    #[must_use]
    pub fn memory_dir(&self) -> PathBuf {
        self.pulse_root.join("memory")
    }

    /// Path to MEMORY.md.
    #[must_use]
    pub fn memory_file(&self) -> PathBuf {
        self.memory_dir().join("MEMORY.md")
    }

    /// Path to EPHEMERAL.md.
    #[must_use]
    pub fn ephemeral_file(&self) -> PathBuf {
        self.memory_dir().join("EPHEMERAL.md")
    }

    /// Path to conversations directory.
    #[must_use]
    pub fn conversations_dir(&self) -> PathBuf {
        self.memory_dir().join("conversations")
    }

    /// Path to ARCHIVE.md index.
    #[must_use]
    pub fn archive_index(&self) -> PathBuf {
        self.memory_dir().join("ARCHIVE.md")
    }

    // ── Core operations ──────────────────────────────────────────────

    /// Read EPHEMERAL.md content without clearing it.
    /// Returns None if the file doesn't exist or is empty.
    pub fn consume_content(&self) -> Result<Option<String>, error::RecallError> {
        consume::consume(&self.ephemeral_file())
    }

    /// Check if the memory system has been initialized.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.memory_dir().exists() && self.conversations_dir().exists()
    }

    /// Number of lines in MEMORY.md.
    #[must_use]
    pub fn memory_line_count(&self) -> usize {
        let path = self.memory_file();
        if !path.exists() {
            return 0;
        }
        fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .count()
    }
}

// ---------------------------------------------------------------------------
// Pulse-null plugin implementation — behind feature flag
// ---------------------------------------------------------------------------

#[cfg(feature = "pulse-null")]
mod plugin_impl {
    use super::*;
    use std::any::Any;
    use std::future::Future;
    use std::pin::Pin;

    use pulse_system_types::plugin::{Plugin, PluginContext, PluginResult, PluginRole};
    use pulse_system_types::{HealthStatus, PluginMeta, SetupPrompt};

    impl RecallEcho {
        fn health_check(&self) -> HealthStatus {
            if !self.memory_dir().exists() {
                return HealthStatus::Down("memory directory not found".into());
            }
            if !self.memory_file().exists() {
                return HealthStatus::Degraded("MEMORY.md not found".into());
            }
            if !self.conversations_dir().exists() {
                return HealthStatus::Degraded("conversations directory not found".into());
            }
            HealthStatus::Healthy
        }

        fn get_setup_prompts() -> Vec<SetupPrompt> {
            vec![SetupPrompt {
                key: "pulse_root".into(),
                question: "Pulse root directory:".into(),
                required: true,
                secret: false,
                default: None,
            }]
        }
    }

    /// Factory function — creates a fully initialized recall-echo plugin.
    pub async fn create(
        config: &serde_json::Value,
        ctx: &PluginContext,
    ) -> Result<Box<dyn Plugin>, Box<dyn std::error::Error + Send + Sync>> {
        let pulse_root = configured_pulse_root(config).unwrap_or_else(|| ctx.pulse_root.clone());

        Ok(Box::new(RecallEcho::new(pulse_root)))
    }

    /// The plugin config's pulse root: `pulse_root`, or the pre-4.6.0
    /// `entity_root` key a config written by an older setup wizard carries.
    fn configured_pulse_root(config: &serde_json::Value) -> Option<PathBuf> {
        ["pulse_root", "entity_root"]
            .iter()
            .find_map(|key| config.get(key).and_then(|v| v.as_str()))
            .map(PathBuf::from)
    }

    impl Plugin for RecallEcho {
        fn meta(&self) -> PluginMeta {
            PluginMeta {
                name: "recall-echo".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                description: "Persistent memory system with knowledge graph".into(),
            }
        }

        fn role(&self) -> PluginRole {
            PluginRole::Memory
        }

        fn start(&mut self) -> PluginResult<'_> {
            Box::pin(async { Ok(()) })
        }

        fn stop(&mut self) -> PluginResult<'_> {
            Box::pin(async { Ok(()) })
        }

        fn health(&self) -> Pin<Box<dyn Future<Output = HealthStatus> + Send + '_>> {
            Box::pin(async move { self.health_check() })
        }

        fn setup_prompts(&self) -> Vec<SetupPrompt> {
            Self::get_setup_prompts()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plugin_config_reads_the_pulse_root_key() {
            let config = serde_json::json!({ "pulse_root": "/srv/echo" });
            assert_eq!(
                configured_pulse_root(&config),
                Some(PathBuf::from("/srv/echo"))
            );
        }

        #[test]
        fn plugin_config_still_reads_the_legacy_entity_root_key() {
            let config = serde_json::json!({ "entity_root": "/srv/echo" });
            assert_eq!(
                configured_pulse_root(&config),
                Some(PathBuf::from("/srv/echo"))
            );
        }

        #[test]
        fn plugin_config_prefers_pulse_root_over_the_legacy_key() {
            let config = serde_json::json!({ "pulse_root": "/new", "entity_root": "/old" });
            assert_eq!(configured_pulse_root(&config), Some(PathBuf::from("/new")));
            assert_eq!(configured_pulse_root(&serde_json::json!({})), None);
        }
    }
}

#[cfg(feature = "pulse-null")]
pub use plugin_impl::create;
