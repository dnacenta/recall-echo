// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Extract one archive, for a host that embeds recall-echo.
//!
//! The daemon extracts in the background, but only for an embedded store:
//! in `[graph] mode = "server"` there is no daemon, and `graph extract` is a
//! command somebody has to remember to run. A host that archives its own
//! conversations — pulse-null — knows the moment an archive lands, so it can
//! ask for exactly that archive here.
//!
//! The host never hands over a model. The provider is the one
//! `.recall-echo.toml` configures, built and located the same way
//! `graph extract` builds it. The host may only say how the CLI is spawned —
//! which binary, with which environment, in which directory — when it knows
//! better (see [`CliOverrides`]).
//!
//! ```text
//! extract_archive(memory_dir, log, overrides)
//!   ├─ provider from [llm]            ── none usable ─▶ ProviderUnavailable
//!   ├─ archive file for `log`         ── missing ────▶ Recall
//!   └─ store (exclusive)
//!        ├─ nothing of `log` pending  ─────────────▶ Ok(NothingPending)
//!        ├─ every chunk failed        ─────────────▶ AllChunksFailed (left pending)
//!        └─ extracted, then marked    ─────────────▶ Ok(Extracted)
//! ```
//!
//! One call is one attempt: no retry, no quarantine. Scheduling, budgets and
//! back-off are the host's, because only the host knows what else it is
//! spending on.

use std::path::{Path, PathBuf};

pub use crate::cli_provider::CliOverrides;
use crate::error::RecallError;
use crate::graph::llm::LlmProvider;
use crate::graph::types::IngestionReport;
use crate::graph::{GraphMemory, IngestContext};

/// What one successful extraction added to the graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveExtraction {
    pub log_number: u32,
    pub entities_created: u32,
    pub entities_merged: u32,
    pub relationships_created: u32,
    /// Tokens the provider reported spending.
    pub measured_tokens: u64,
    /// Tokens estimated for calls whose provider reported nothing.
    pub estimated_tokens: u64,
    /// Steps that failed inside an archive that still yielded something.
    pub warnings: Vec<String>,
    /// The CLI binary that ran; `None` for an HTTP provider.
    pub binary: Option<PathBuf>,
}

impl ArchiveExtraction {
    /// Every token this extraction is believed to have spent — the number to
    /// budget against.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.measured_tokens + self.estimated_tokens
    }
}

/// How a call that did not fail ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractOutcome {
    /// The archive was extracted and marked; the graph grew by this much.
    Extracted(ArchiveExtraction),
    /// No episode of this archive awaits extraction — already extracted, or
    /// never ingested. Nothing was spent.
    NothingPending,
}

/// Why an extraction did not happen.
#[derive(Debug, thiserror::Error)]
pub enum ExtractArchiveError {
    /// No provider could be built: the CLI binary is not found, an API key is
    /// missing, or `[llm]` is unusable. Nothing was spent, and trying again
    /// changes nothing until the configuration or the machine does.
    #[error("no usable extraction provider: {0}")]
    ProviderUnavailable(String),
    /// The provider ran and every chunk's call failed. The archive is left
    /// pending rather than recorded as empty.
    #[error("every extraction chunk failed ({chunks}), archive left pending; first: {first}")]
    AllChunksFailed {
        chunks: u32,
        first: String,
        /// What the failed calls are believed to have cost.
        tokens: u64,
    },
    /// Anything else: the store, the archive file, the graph.
    #[error(transparent)]
    Recall(#[from] RecallError),
}

impl ExtractArchiveError {
    /// Tokens spent on the way to this error — nonzero only when the provider
    /// was called.
    #[must_use]
    pub fn tokens_spent(&self) -> u64 {
        match self {
            Self::AllChunksFailed { tokens, .. } => *tokens,
            Self::ProviderUnavailable(_) | Self::Recall(_) => 0,
        }
    }
}

/// Extract archive `log_number` of the memory at `memory_dir` into its graph,
/// with the provider `.recall-echo.toml` configures.
///
/// `overrides` say how the CLI is spawned — binary, environment, working
/// directory; [`CliOverrides::default`] spawns it exactly as `graph extract`
/// would. HTTP providers ignore them.
///
/// Works in both graph modes. In `server` mode the store is opened directly;
/// in `embedded` mode this takes the store exclusively, as `graph extract`
/// does, so a running daemon is stopped for the duration of the call.
pub async fn extract_archive(
    memory_dir: &Path,
    log_number: u32,
    overrides: &CliOverrides,
) -> Result<ExtractOutcome, ExtractArchiveError> {
    if !memory_dir.join("graph").exists() {
        return Err(RecallError::NotInitialized(
            "graph/ not initialized \u{2014} run `recall-echo graph init` first".into(),
        )
        .into());
    }

    let handle = crate::llm_provider::create_provider_with_overrides(memory_dir, overrides)
        .map_err(|err| ExtractArchiveError::ProviderUnavailable(err.to_string()))?;

    let archive = ArchiveFile::read(memory_dir, log_number)?;
    let llm = handle.llm;
    let binary = handle.binary;

    crate::serve_client::exclusive(memory_dir, |graph| async move {
        Ok(extract_into(&graph, llm.as_ref(), &archive, binary).await)
    })
    .await?
}

/// One archive on disk, read once, before the store is taken.
#[derive(Debug, Clone)]
pub(crate) struct ArchiveFile {
    pub(crate) log_number: u32,
    pub(crate) session_id: String,
    pub(crate) content: String,
}

impl ArchiveFile {
    fn read(memory_dir: &Path, log_number: u32) -> Result<Self, RecallError> {
        let conversations = crate::graph_cli::find_conversations_dir(memory_dir)?;
        let path = crate::graph_cli::find_archive_file(&conversations, log_number)?;
        let content = std::fs::read_to_string(&path)?;
        let (session_id, _) = crate::graph_cli::extract_archive_metadata(&content, &path);
        Ok(Self {
            log_number,
            session_id,
            content,
        })
    }
}

/// Extract `archive` into an open store: skip it when nothing awaits, mark it
/// only when the provider actually answered.
pub(crate) async fn extract_into(
    graph: &GraphMemory,
    llm: &dyn LlmProvider,
    archive: &ArchiveFile,
    binary: Option<PathBuf>,
) -> Result<ExtractOutcome, ExtractArchiveError> {
    let log_number = archive.log_number;
    if !graph
        .log_awaits_extraction(log_number)
        .await
        .map_err(RecallError::from)?
    {
        return Ok(ExtractOutcome::NothingPending);
    }

    let context = IngestContext::new(archive.session_id.clone(), Some(log_number));
    let report = graph
        .extract_from_archive(&archive.content, &context, llm)
        .await
        .map_err(RecallError::from)?;
    let extraction = classify(log_number, report, binary)?;

    graph
        .mark_extracted(log_number)
        .await
        .map_err(RecallError::from)?;
    Ok(ExtractOutcome::Extracted(extraction))
}

/// A report whose every chunk failed is the provider's failure, not an empty
/// archive: it must not be marked, or nothing would ever retry it.
fn classify(
    log_number: u32,
    report: IngestionReport,
    binary: Option<PathBuf>,
) -> Result<ArchiveExtraction, ExtractArchiveError> {
    if report.is_total_failure() {
        return Err(ExtractArchiveError::AllChunksFailed {
            chunks: report.chunks_failed,
            first: report.errors.first().cloned().unwrap_or_default(),
            tokens: report.total_tokens(),
        });
    }
    Ok(ArchiveExtraction {
        log_number,
        entities_created: report.entities_created,
        entities_merged: report.entities_merged,
        relationships_created: report.relationships_created,
        measured_tokens: report.measured_tokens,
        estimated_tokens: report.estimated_tokens,
        warnings: report.errors,
        binary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::error::GraphError;

    /// Answers every extraction call with an empty but well-formed result.
    struct EmptyModel;

    #[async_trait::async_trait]
    impl LlmProvider for EmptyModel {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Ok(r#"{"entities": [], "relationships": []}"#.into())
        }
    }

    /// Fails every call, as a CLI that cannot authenticate does.
    struct BrokenModel;

    #[async_trait::async_trait]
    impl LlmProvider for BrokenModel {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Err(GraphError::Llm("grok exited 1: not logged in".into()))
        }
    }

    /// Answers every call with prose: nothing to extract, but every call ran.
    struct ProseModel;

    #[async_trait::async_trait]
    impl LlmProvider for ProseModel {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Ok("I'd be happy to help with that transcript.".into())
        }
    }

    /// Answers the first call with invalid JSON and every later one cleanly.
    struct GlitchOnceModel {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for GlitchOnceModel {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if call == 0 {
                r#"{"entities": [], "relationships": [[]].length ? null : null}"#.into()
            } else {
                r#"{"entities": [], "relationships": []}"#.into()
            })
        }
    }

    /// Panics if called: the assertion that nothing was spent.
    struct NoModel;

    #[async_trait::async_trait]
    impl LlmProvider for NoModel {
        async fn complete(&self, _: &str, user: &str, _: u32) -> Result<String, GraphError> {
            panic!("extraction called a model it should not have:\n{user}");
        }
    }

    fn archive(log_number: u32) -> ArchiveFile {
        ArchiveFile {
            log_number,
            session_id: "s-1".into(),
            content: "---\nlog: 7\n---\n\n### User\n\nD moved Echo to a new home \
                      directory today.\n\n### Assistant\n\nNoted — Echo now lives under \
                      pulse-null/echo.\n"
                .into(),
        }
    }

    async fn store_with_pending(log_number: u32) -> (tempfile::TempDir, GraphMemory) {
        let tmp = tempfile::tempdir().unwrap();
        let graph = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        graph
            .db()
            .query("CREATE episode SET session_id = 's-1', abstract = 'a', log_number = $ln")
            .bind(("ln", i64::from(log_number)))
            .await
            .unwrap()
            .check()
            .unwrap();
        (tmp, graph)
    }

    #[tokio::test]
    async fn an_extracted_archive_is_marked_and_reports_what_it_spent() {
        let (_tmp, graph) = store_with_pending(7).await;
        let binary = Some(PathBuf::from("/opt/grok"));

        let outcome = extract_into(&graph, &EmptyModel, &archive(7), binary.clone())
            .await
            .unwrap();

        let ExtractOutcome::Extracted(extraction) = outcome else {
            panic!("expected an extraction, got {outcome:?}");
        };
        assert_eq!(extraction.log_number, 7);
        assert_eq!(extraction.binary, binary);
        assert!(
            extraction.total_tokens() > 0,
            "an unmeasured call is estimated"
        );
        assert!(!graph.log_awaits_extraction(7).await.unwrap());
    }

    #[tokio::test]
    async fn an_archive_with_nothing_pending_costs_nothing() {
        let (_tmp, graph) = store_with_pending(7).await;
        graph.mark_extracted(7).await.unwrap();

        let outcome = extract_into(&graph, &NoModel, &archive(7), None)
            .await
            .unwrap();
        assert_eq!(outcome, ExtractOutcome::NothingPending);

        let never_ingested = extract_into(&graph, &NoModel, &archive(8), None)
            .await
            .unwrap();
        assert_eq!(never_ingested, ExtractOutcome::NothingPending);
    }

    #[tokio::test]
    async fn a_provider_that_fails_every_chunk_leaves_the_archive_pending() {
        let (_tmp, graph) = store_with_pending(7).await;

        let err = extract_into(&graph, &BrokenModel, &archive(7), None)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, ExtractArchiveError::AllChunksFailed { first, .. } if first.contains("not logged in")),
            "{err}"
        );
        assert!(graph.log_awaits_extraction(7).await.unwrap());
    }

    /// Calls that ran and yielded nothing were still paid for, and the error
    /// says what the answers were rather than quoting them.
    #[tokio::test]
    async fn unusable_answers_are_billed_and_classified() {
        let (_tmp, graph) = store_with_pending(7).await;

        let err = extract_into(&graph, &ProseModel, &archive(7), None)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, ExtractArchiveError::AllChunksFailed { first, .. } if first.contains("no JSON")),
            "{err}"
        );
        assert_eq!(err.tokens_spent(), 5_000, "the call and its one retry");
        assert!(graph.log_awaits_extraction(7).await.unwrap());
    }

    /// A chunk recovered by its retry is an extraction, with the recovery
    /// named among the warnings.
    #[tokio::test]
    async fn a_recovered_chunk_is_extracted_with_a_warning() {
        let (_tmp, graph) = store_with_pending(7).await;
        let model = GlitchOnceModel {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let outcome = extract_into(&graph, &model, &archive(7), None)
            .await
            .unwrap();

        let ExtractOutcome::Extracted(extraction) = outcome else {
            panic!("expected an extraction, got {outcome:?}");
        };
        assert!(
            extraction
                .warnings
                .iter()
                .any(|w| w.contains("recovered") && w.contains("invalid JSON")),
            "{:?}",
            extraction.warnings
        );
        assert!(!graph.log_awaits_extraction(7).await.unwrap());
    }

    #[tokio::test]
    async fn an_uninitialised_graph_is_refused_before_any_provider_is_built() {
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_archive(tmp.path(), 1, &CliOverrides::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ExtractArchiveError::Recall(RecallError::NotInitialized(_))
        ));
        assert_eq!(err.tokens_spent(), 0);
    }

    /// A binary that is not there is a provider problem, reported as one —
    /// before the store is touched and before anything is spent.
    #[tokio::test]
    async fn a_missing_binary_is_provider_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("graph")).unwrap();
        std::fs::write(
            tmp.path().join(".recall-echo.toml"),
            "[llm]\nprovider = \"grok\"\n",
        )
        .unwrap();
        let absent = tmp.path().join("no-such-dir").join("grok");

        let overrides = CliOverrides {
            command: Some(absent),
            ..CliOverrides::default()
        };
        let err = extract_archive(tmp.path(), 1, &overrides)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, ExtractArchiveError::ProviderUnavailable(reason) if reason.contains("not an executable")),
            "{err}"
        );
        assert_eq!(err.tokens_spent(), 0);
    }

    #[test]
    fn a_total_failure_carries_its_cost() {
        let report = IngestionReport {
            chunks_total: 2,
            chunks_failed: 2,
            estimated_tokens: 5_000,
            errors: vec!["first".into(), "second".into()],
            ..IngestionReport::default()
        };
        let err = classify(3, report, None).unwrap_err();
        assert_eq!(err.tokens_spent(), 5_000);
        assert!(err.to_string().contains("first"), "{err}");
    }

    #[test]
    fn a_partial_failure_is_an_extraction_with_warnings() {
        let report = IngestionReport {
            chunks_total: 2,
            chunks_failed: 1,
            entities_created: 4,
            relationships_created: 2,
            measured_tokens: 1_000,
            errors: vec!["chunk 2 failed".into()],
            ..IngestionReport::default()
        };
        let extraction = classify(3, report, None).unwrap();
        assert_eq!(extraction.entities_created, 4);
        assert_eq!(extraction.relationships_created, 2);
        assert_eq!(extraction.warnings, vec!["chunk 2 failed".to_string()]);
        assert_eq!(extraction.total_tokens(), 1_000);
    }

    /// A host spawns this onto its own runtime; it must be `Send`.
    #[test]
    fn the_extraction_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let memory_dir = PathBuf::from("/nonexistent");
        let overrides = CliOverrides::default();
        let future = extract_archive(&memory_dir, 1, &overrides);
        assert_send(&future);
    }
}
