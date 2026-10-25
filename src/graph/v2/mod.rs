// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Second extraction pipeline.
//!
//! v1 stays the default. v2 reads a session, sends one pass per window, and
//! writes edges and slots through a closed schema. The two never share a store:
//! v2 stamps `meta:pipeline` and refuses a store it did not build.

pub(crate) mod input;
pub(crate) mod pass1;
pub(crate) mod resolve;
pub(crate) mod schema;
mod write;

use super::error::GraphError;
use super::ingest::IngestContext;
use super::llm::LlmProvider;
use super::types::IngestionReport;
use super::GraphMemory;
pub(crate) use write::V2_REFUSAL;

/// Which extraction pipeline a run uses.
///
/// Absent configuration means [`PipelineVersion::V1`]. A flag overrides the
/// config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineVersion {
    /// The extraction that already ships: chunk, extract, dedup.
    V1,
    /// One structured pass per window, closed predicates, literal slots.
    V2,
}

impl PipelineVersion {
    /// `v1` or `v2`. Anything else is not a pipeline.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "v1" => Some(Self::V1),
            "v2" => Some(Self::V2),
            _ => None,
        }
    }

    /// The config and stamp spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }

    /// A flag wins. Otherwise the configured value, or v1 when nothing is set.
    ///
    /// # Errors
    ///
    /// The configured value is present and is neither `v1` nor `v2`.
    pub fn resolve(flag: Option<Self>, configured: Option<&str>) -> Result<Self, String> {
        if let Some(flag) = flag {
            return Ok(flag);
        }
        match configured {
            None => Ok(Self::V1),
            Some(raw) => Self::parse(raw)
                .ok_or_else(|| format!("[graph] pipeline must be \"v1\" or \"v2\", not {raw}")),
        }
    }
}

/// Pipeline for a CLI command: the flag, then `[graph] pipeline`, then v1.
pub(crate) fn chosen_pipeline(
    memory_dir: &std::path::Path,
    flag: Option<PipelineVersion>,
) -> Result<PipelineVersion, crate::error::RecallError> {
    let configured = crate::config::load_from_dir(memory_dir)
        .graph
        .and_then(|section| section.pipeline);
    PipelineVersion::resolve(flag, configured.as_deref()).map_err(crate::error::RecallError::Config)
}

/// Pipeline for a hook or the daemon. A bad config value is logged and v1 runs,
/// so a typo cannot stop the archive from being ingested.
pub(crate) fn pipeline_for_hook(memory_dir: &std::path::Path) -> PipelineVersion {
    match chosen_pipeline(memory_dir, None) {
        Ok(pipeline) => pipeline,
        Err(err) => {
            eprintln!("recall-echo: {err}; staying on pipeline v1");
            PipelineVersion::V1
        }
    }
}

/// The store refused this pipeline. The message is the rebuild line.
#[must_use]
pub(crate) fn is_stamp_refusal(err: &GraphError) -> bool {
    err.to_string().contains(V2_REFUSAL)
}

/// Extract one archive with the pipeline this run selected.
///
/// v1 calls the extractor that runs today. v2 reads the session from
/// `memory_dir` and ignores `archive_text` except as the caller's record of
/// which file it opened.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn extract_log(
    gm: &GraphMemory,
    memory_dir: &std::path::Path,
    log: u32,
    archive_text: &str,
    ctx: &IngestContext,
    llm: &dyn LlmProvider,
    pipeline: PipelineVersion,
) -> Result<IngestionReport, GraphError> {
    match pipeline {
        PipelineVersion::V1 => {
            super::ingest::extract_from_archive(gm, archive_text, ctx, llm).await
        }
        PipelineVersion::V2 => pass1::extract_session(gm, memory_dir, log, ctx, llm).await,
    }
}
