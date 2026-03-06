//! recall-echo — Persistent three-layer memory system for pulse-null entities.
//!
//! Provides a three-layer memory architecture: curated memory (MEMORY.md),
//! FIFO rolling window of recent sessions (EPHEMERAL.md), and full conversation
//! archives with LLM-enhanced summaries. Designed as a core crate for
//! pulse-null with standalone CLI support.
//!
//! # Usage as a library
//!
//! ```no_run
//! use recall_echo::RecallEcho;
//!
//! # fn run() {
//! let recall = RecallEcho::from_default().expect("memory system");
//! if let Some(content) = recall.consume_content().expect("consume") {
//!     println!("{content}");
//! }
//! # }
//! ```

pub mod archive;
pub mod checkpoint;
pub mod config;
pub mod consume;
pub mod conversation;
pub mod dashboard;
pub mod distill;
pub mod ephemeral;
pub mod frontmatter;
pub mod init;
pub mod paths;
pub mod search;
pub mod status;
pub mod summarize;
pub mod tags;

use std::fs;
use std::path::{Path, PathBuf};

use echo_system_types::{HealthStatus, SetupPrompt};

pub use archive::SessionMetadata;
pub use summarize::ConversationSummary;

/// The recall-echo plugin. Manages persistent three-layer memory.
///
/// All paths are derived from entity_root:
/// ```text
/// {entity_root}/memory/
/// ├── MEMORY.md
/// ├── EPHEMERAL.md
/// ├── ARCHIVE.md
/// └── conversations/
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

    // ── Plugin interface ─────────────────────────────────────────────

    /// Report health status.
    pub fn health(&self) -> HealthStatus {
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

    /// Configuration prompts for the pulse-null init wizard.
    pub fn setup_prompts() -> Vec<SetupPrompt> {
        vec![SetupPrompt {
            key: "entity_root".into(),
            question: "Entity root directory:".into(),
            required: true,
            secret: false,
            default: None,
        }]
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
