//! LoCoMo benchmark harness — thin wrapper over recall-echo's retrieval primitives.
//!
//! Exposes two operations to the benchmark runner:
//!
//! 1. [`ingest_conversation`] — write a LoCoMo conversation as N entity sessions
//!    (one archive per session, dated correctly), then trigger graph extraction.
//! 2. [`answer_question`] — answer a question using hybrid retrieval
//!    (graph query + archive ranked search + MEMORY.md), call an LLM, return
//!    the predicted answer plus the retrieval trace as JSON.
//!
//! This module is intentionally a *thin* facade: every call ultimately drives
//! the same code paths an entity exercises at runtime. Score validity depends
//! on that, so do not reimplement archive format, retrieval, or extraction here.

use serde::{Deserialize, Serialize};

pub mod answer;
pub mod ingest;

#[cfg(test)]
mod tests;

pub use answer::{answer_question, AnswerOpts, BenchAnswer, RetrievedEpisode, RetrievedFact};
pub use ingest::{ingest_conversation, IngestStats};

// ── Dataset shape ────────────────────────────────────────────────────────

/// One speaker turn inside a LoCoMo session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchTurn {
    pub speaker: String,
    pub text: String,
    pub dia_id: String,
}

/// One LoCoMo session: a header date plus an ordered list of turns.
///
/// The `date_time` is the raw string from the dataset (e.g. `"22 May 2023"`)
/// so ingestion can normalize it however needed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchSession {
    pub date_time: String,
    pub turns: Vec<BenchTurn>,
}

/// A full LoCoMo conversation: identity of both speakers and N sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchConversation {
    pub sample_id: String,
    pub speaker_a: String,
    pub speaker_b: String,
    pub sessions: Vec<BenchSession>,
}
