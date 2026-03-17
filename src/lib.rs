//! recall-echo — Persistent memory system with knowledge graph.
//!
//! A general-purpose persistent memory system for any LLM tool — Claude Code,
//! Ollama, or any provider. Features a three-layer memory architecture with
//! an optional knowledge graph powered by SurrealDB + fastembed.
//!
//! # Architecture
//!
//! ```text
//! Input adapters (JSONL transcripts, pulse-null Messages)
//!     → Conversation (universal internal format)
//!     → Archive pipeline (markdown + index + ephemeral + graph)
//! ```
//!
//! # Features
//!
//! - `graph` (default) — Knowledge graph with SurrealDB + fastembed
//! - `pulse-null` — Plugin integration for pulse-null entities

pub mod archive;
pub mod checkpoint;
pub mod config;
pub mod config_cli;
pub mod consume;
pub mod conversation;
pub mod dashboard;
pub mod distill;
pub mod ephemeral;
pub mod frontmatter;
pub mod init;
pub mod jsonl;
pub mod paths;
pub mod search;
pub mod status;
pub mod summarize;
pub mod tags;

#[cfg(feature = "graph")]
pub mod graph_bridge;
#[cfg(feature = "graph")]
pub mod graph_cli;
#[cfg(feature = "llm")]
pub mod llm_provider;

#[cfg(feature = "pulse-null")]
pub mod pulse_null;

use std::fs;
use std::path::{Path, PathBuf};

pub use archive::SessionMetadata;
pub use summarize::ConversationSummary;

/// The recall-echo memory system.
///
/// All paths are derived from entity_root:
/// ```text
/// {entity_root}/memory/
/// ├── MEMORY.md
/// ├── EPHEMERAL.md
/// ├── ARCHIVE.md
/// ├── conversations/
/// └── graph/ (when graph feature is enabled)
/// ```
pub struct RecallEcho {
    entity_root: PathBuf,
}

impl RecallEcho {
    /// Create a new RecallEcho instance with a specific entity root directory.
    pub fn new(entity_root: PathBuf) -> Self {
        Self { entity_root }
    }

    /// Create a RecallEcho using the default path resolution
    /// (RECALL_ECHO_HOME env var or current working directory).
    pub fn from_default() -> Result<Self, String> {
        Ok(Self::new(paths::entity_root()?))
    }

    /// Entity root directory.
    pub fn entity_root(&self) -> &Path {
        &self.entity_root
    }

    /// Memory directory: {entity_root}/memory/
    pub fn memory_dir(&self) -> PathBuf {
        self.entity_root.join("memory")
    }

    /// Path to MEMORY.md.
    pub fn memory_file(&self) -> PathBuf {
        self.memory_dir().join("MEMORY.md")
    }

    /// Path to EPHEMERAL.md.
    pub fn ephemeral_file(&self) -> PathBuf {
        self.memory_dir().join("EPHEMERAL.md")
    }

    /// Path to conversations directory.
    pub fn conversations_dir(&self) -> PathBuf {
        self.memory_dir().join("conversations")
    }

    /// Path to ARCHIVE.md index.
    pub fn archive_index(&self) -> PathBuf {
        self.memory_dir().join("ARCHIVE.md")
    }

    // ── Core operations ──────────────────────────────────────────────

    /// Read EPHEMERAL.md content without clearing it.
    /// Returns None if the file doesn't exist or is empty.
    pub fn consume_content(&self) -> Result<Option<String>, String> {
        consume::consume(&self.ephemeral_file())
    }

    /// Check if the memory system has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.memory_dir().exists() && self.conversations_dir().exists()
    }

    /// Number of lines in MEMORY.md.
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

    use echo_system_types::plugin::{Plugin, PluginContext, PluginResult, PluginRole};
    use echo_system_types::{HealthStatus, PluginMeta, SetupPrompt};

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
                key: "entity_root".into(),
                question: "Entity root directory:".into(),
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
        let entity_root = config
            .get("entity_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.entity_root.clone());

        Ok(Box::new(RecallEcho::new(entity_root)))
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
}

#[cfg(feature = "pulse-null")]
pub use plugin_impl::create;
