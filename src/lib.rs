//! recall-echo — Persistent three-layer memory system for AI coding agents.
//!
//! Provides a three-layer memory architecture: curated memory (MEMORY.md),
//! short-term session memory (EPHEMERAL.md), and long-term archive logs.
//! Designed for integration with echo-system as a core plugin.
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
pub mod consume;
pub mod frontmatter;
pub mod init;
pub mod paths;
pub mod promote;
pub mod status;

use std::fs;
use std::path::PathBuf;

use echo_system_types::{HealthStatus, SetupPrompt};

/// The recall-echo plugin. Manages persistent three-layer memory.
pub struct RecallEcho {
    base_dir: PathBuf,
}

impl RecallEcho {
    /// Create a new RecallEcho instance with a specific base directory.
    ///
    /// The base directory is where all memory files live (MEMORY.md,
    /// EPHEMERAL.md, ARCHIVE.md, memories/, etc).
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

    /// Path to the memories directory (archive logs).
    pub fn memories_dir(&self) -> PathBuf {
        self.base_dir.join("memories")
    }

    /// Path to MEMORY.md.
    pub fn memory_file(&self) -> PathBuf {
        self.base_dir.join("memory").join("MEMORY.md")
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

    /// Create an archive checkpoint. Returns the path to the new log file.
    pub fn checkpoint(&self, trigger: &str, context: &str) -> Result<PathBuf, String> {
        let memories = self.memories_dir();
        let archive_idx = self.archive_index();

        if !memories.exists() {
            return Err("memories/ directory not found. Run init first.".to_string());
        }

        let next_num = archive::highest_log_number(&memories) + 1;
        let date = checkpoint::utc_now();
        let date_short = date.split('T').next().unwrap_or(&date);

        let fm = frontmatter::Frontmatter {
            log: next_num,
            date: date.clone(),
            trigger: trigger.to_string(),
            context: context.to_string(),
            topics: vec![],
        };

        let body = checkpoint::log_body(next_num);
        let content = format!("{}\n{}", fm.render(), body);
        let log_path = memories.join(format!("archive-log-{next_num:03}.md"));

        fs::write(&log_path, &content).map_err(|e| format!("Failed to create archive log: {e}"))?;

        archive::append_index(&archive_idx, next_num, date_short, trigger)?;

        Ok(log_path)
    }

    /// Promote EPHEMERAL.md into an archive log.
    /// Returns the path to the new log file, or None if nothing to promote.
    /// Clears EPHEMERAL.md after promotion.
    pub fn promote(&self, context: &str) -> Result<Option<PathBuf>, String> {
        let ephemeral = self.ephemeral_file();
        let memories = self.memories_dir();
        let archive_idx = self.archive_index();

        if !memories.exists() {
            return Err("memories/ directory not found. Run init first.".to_string());
        }

        if !ephemeral.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&ephemeral)
            .map_err(|e| format!("Failed to read EPHEMERAL.md: {e}"))?;

        if content.trim().is_empty() {
            return Ok(None);
        }

        let next_num = archive::highest_log_number(&memories) + 1;
        let date = promote::utc_now();
        let date_short = date.split('T').next().unwrap_or(&date);

        let ctx = if context.is_empty() {
            content
                .lines()
                .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .or_else(|| content.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("")
                .trim_start_matches('#')
                .trim()
                .to_string()
        } else {
            context.to_string()
        };

        let fm = frontmatter::Frontmatter {
            log: next_num,
            date: date.clone(),
            trigger: "session-end".to_string(),
            context: ctx,
            topics: vec![],
        };

        let log_content = format!(
            "{}\n\n# Archive Log {next_num:03}\n\n{}",
            fm.render(),
            content
        );
        let log_path = memories.join(format!("archive-log-{next_num:03}.md"));

        fs::write(&log_path, &log_content)
            .map_err(|e| format!("Failed to create archive log: {e}"))?;

        archive::append_index(&archive_idx, next_num, date_short, "session-end")?;

        fs::write(&ephemeral, "").map_err(|e| format!("Failed to clear EPHEMERAL.md: {e}"))?;

        Ok(Some(log_path))
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
        if !self.memories_dir().exists() {
            return HealthStatus::Degraded("memories directory not found".into());
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
