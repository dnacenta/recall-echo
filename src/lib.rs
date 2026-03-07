//! recall-echo — Persistent three-layer memory system for AI coding agents.
//!
//! Provides a three-layer memory architecture: curated memory (MEMORY.md),
//! FIFO rolling window of recent sessions (EPHEMERAL.md), and full conversation
//! archives from JSONL transcripts. Designed for integration with echo-system
//! as a core plugin.
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
pub mod dashboard;
pub mod distill;
pub mod ephemeral;
pub mod frontmatter;
pub mod init;
pub mod jsonl;
pub mod paths;
pub mod search;
pub mod status;
pub mod tags;

use std::fs;
use std::path::PathBuf;

use echo_system_types::{HealthStatus, SetupPrompt};

/// The recall-echo plugin. Manages persistent three-layer memory.
pub struct RecallEcho {
    base_dir: PathBuf,
}

impl RecallEcho {
    /// Create a new RecallEcho instance with a specific base directory.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Create a RecallEcho using the default path resolution
    /// (~/.claude or RECALL_ECHO_HOME env var).
    pub fn from_default() -> Result<Self, String> {
        Ok(Self::new(paths::claude_dir()?))
    }

    /// Base directory for all memory files.
    pub fn base_dir(&self) -> &PathBuf {
        &self.base_dir
    }

    /// Path to the conversations directory (full archives).
    pub fn conversations_dir(&self) -> PathBuf {
        self.base_dir.join("conversations")
    }

    /// Memory directory: {base_dir}/memory/
    pub fn memory_dir(&self) -> PathBuf {
        self.base_dir.join("memory")
    }

    /// Path to MEMORY.md.
    pub fn memory_file(&self) -> PathBuf {
        self.memory_dir().join("MEMORY.md")
    }

    /// Path to EPHEMERAL.md.
    pub fn ephemeral_file(&self) -> PathBuf {
        self.base_dir.join("EPHEMERAL.md")
    }

    /// Path to ARCHIVE.md index.
    pub fn archive_index(&self) -> PathBuf {
        self.base_dir.join("ARCHIVE.md")
    }

    // ── Core operations ──────────────────────────────────────────────

    /// Read EPHEMERAL.md content without clearing it.
    /// Returns None if the file doesn't exist or is empty.
    pub fn consume_content(&self) -> Result<Option<String>, String> {
        let ephemeral = self.ephemeral_file();
        if !ephemeral.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&ephemeral)
            .map_err(|e| format!("Failed to read EPHEMERAL.md: {e}"))?;
        if content.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(content))
    }

    /// Create an archive checkpoint. Returns the path to the new conversation file.
    pub fn checkpoint(&self, trigger: &str, _context: &str) -> Result<PathBuf, String> {
        let conversations = self.conversations_dir();
        let archive_idx = self.archive_index();

        if !conversations.exists() {
            return Err("conversations/ directory not found. Run init first.".to_string());
        }

        let next_num = archive::highest_conversation_number(&conversations) + 1;
        let date = jsonl::utc_now();
        let date_short = date.split('T').next().unwrap_or(&date);

        let fm = frontmatter::Frontmatter {
            log: next_num,
            date: date.clone(),
            session_id: String::new(),
            message_count: 0,
            duration: String::new(),
            source: trigger.to_string(),
            topics: vec![],
        };

        let body = checkpoint::log_body(next_num);
        let content = format!("{}\n{}", fm.render(), body);
        let log_path = conversations.join(format!("conversation-{next_num:03}.md"));

        fs::write(&log_path, &content)
            .map_err(|e| format!("Failed to create conversation file: {e}"))?;

        archive::append_index(&archive_idx, next_num, date_short, "", &[], 0, "")?;

        Ok(log_path)
    }

    // ── Plugin interface ─────────────────────────────────────────────

    /// Report health status.
    pub fn health(&self) -> HealthStatus {
        if !self.base_dir.exists() {
            return HealthStatus::Down("base directory not found".into());
        }
        if !self.memory_file().exists() {
            return HealthStatus::Degraded("MEMORY.md not found".into());
        }
        if !self.conversations_dir().exists() {
            return HealthStatus::Degraded("conversations directory not found".into());
        }
        HealthStatus::Healthy
    }

    /// Configuration prompts for the echo-system init wizard.
    pub fn setup_prompts() -> Vec<SetupPrompt> {
        vec![SetupPrompt {
            key: "base_dir".into(),
            question: "Memory system base directory:".into(),
            required: true,
            secret: false,
            default: Some("~/.claude".into()),
        }]
    }
}
