// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Second extraction pipeline.
//!
//! v1 stays the default. v2 reads a session, sends one pass per window, and
//! writes edges and slots through a closed schema. The two never share a store:
//! v2 stamps `meta:pipeline` and refuses a store it did not build, and a
//! v2 stamp refuses a v1 write the other way.

pub(crate) mod ground;
pub(crate) mod input;
pub(crate) mod pass1;
pub(crate) mod resolve;
pub(crate) mod schema;
mod write;

use std::path::Path;

use surrealdb::Surreal;

use super::error::GraphError;
use super::ingest::IngestContext;
use super::llm::LlmProvider;
use super::store::Db;
use super::types::IngestionReport;
use super::GraphMemory;
use crate::error::RecallError;
pub(crate) use resolve::RESOLVER_CALL_BOUND;
pub(crate) use write::{ensure_v2_store, read_stamp, V1_REFUSAL, V2_REFUSAL};

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

/// What a hook or a CLI run should do, and the one line to print when the
/// config and the store do not say the same thing.
struct PipelineChoice {
    version: PipelineVersion,
    /// Printed as `recall-echo: {notice}` when present.
    notice: Option<String>,
}

/// Pipeline for a CLI command.
///
/// A flag wins over the config, so a broken file is not read when the flag
/// is set: `load` would drop it and print a warning for a value this call
/// does not use. A v2 stamp then wins over the config. Explicit `--pipeline v1`
/// on that stamp is refused. `db` is the already-open store; without it the
/// stamp is read from `memory_dir/graph` and only when that directory is a
/// SurrealKV store.
///
/// # Errors
///
/// The configured value is present and is neither `v1` nor `v2`, and the
/// store is not stamped `v2`. Explicit `--pipeline v1` on a v2 stamp.
/// The stamp could not be read.
pub(crate) async fn chosen_pipeline(
    memory_dir: &Path,
    flag: Option<PipelineVersion>,
    db: Option<&Surreal<Db>>,
) -> Result<PipelineVersion, RecallError> {
    // `--pipeline v2` does not consult the store or the file.
    if flag == Some(PipelineVersion::V2) {
        return Ok(PipelineVersion::V2);
    }
    let configured = match flag {
        Some(_) => None,
        None => crate::config::load_from_dir(memory_dir)
            .graph
            .and_then(|section| section.pipeline),
    };
    let stamp = if stamp_can_change(flag, configured.as_deref()) {
        read_choice_stamp(memory_dir, flag, db).await?
    } else {
        None
    };
    let choice = decide_cli(flag, configured.as_deref(), stamp.as_deref())?;
    log_notice(choice.notice.as_deref());
    Ok(choice.version)
}

/// Pipeline for a hook or the daemon.
///
/// A bad config value is logged and does not stop ingestion. A stamped store
/// supplies the pipeline; an unstamped store stays on v1, which is what a
/// typo did before the stamp was consulted.
pub(crate) async fn pipeline_for_hook(memory_dir: &Path, db: &Surreal<Db>) -> PipelineVersion {
    let choice = hook_choice(memory_dir, db).await;
    log_notice(choice.notice.as_deref());
    choice.version
}

/// The decision [`pipeline_for_hook`] logs and returns.
async fn hook_choice(memory_dir: &Path, db: &Surreal<Db>) -> PipelineChoice {
    let configured = crate::config::load_from_dir(memory_dir)
        .graph
        .and_then(|section| section.pipeline);
    let stamp = match read_stamp(db).await {
        Ok(stamp) => stamp,
        Err(err) => {
            eprintln!("recall-echo: could not read the store pipeline stamp: {err}");
            None
        }
    };
    decide_hook(configured.as_deref(), stamp.as_deref())
}

fn log_notice(notice: Option<&str>) {
    if let Some(notice) = notice {
        eprintln!("recall-echo: {notice}");
    }
}

/// A v2 stamp changes the result when the flag is explicit v1, or when the
/// config is not already v2. An explicit v2 flag never needs the stamp.
fn stamp_can_change(flag: Option<PipelineVersion>, configured: Option<&str>) -> bool {
    match flag {
        Some(PipelineVersion::V2) => false,
        Some(PipelineVersion::V1) => true,
        None => configured != Some(PipelineVersion::V2.as_str()),
    }
}

async fn read_choice_stamp(
    memory_dir: &Path,
    flag: Option<PipelineVersion>,
    db: Option<&Surreal<Db>>,
) -> Result<Option<String>, RecallError> {
    if let Some(db) = db {
        return Ok(read_stamp(db).await?);
    }
    // An explicit flag must not load config. Asking a running daemon for the
    // stamp goes through `load`, so try the directory first and only then
    // the daemon, when the directory open is the one the daemon holds.
    if flag.is_some() {
        read_stamp_off_daemon(memory_dir).await
    } else {
        read_embedded_stamp(memory_dir).await
    }
}

/// The pipeline stamp on `memory_dir/graph`, if that directory is a store.
///
/// A running daemon owns the file lock, so the stamp is read from the daemon
/// when one is up. A directory with no SurrealKV manifest is not a store:
/// this returns [`None`] and does not create one.
pub(crate) async fn read_embedded_stamp(memory_dir: &Path) -> Result<Option<String>, RecallError> {
    let graph = memory_dir.join("graph");
    if !super::store::has_kv_manifest(&graph) {
        return Ok(None);
    }
    if let Some(info) = crate::serve_client::daemon_info(memory_dir).await? {
        return Ok(info.pipeline_stamp);
    }
    Ok(read_stamp(&super::store::open(&graph).await?).await?)
}

/// Like [`read_embedded_stamp`], without loading config unless the store is locked.
async fn read_stamp_off_daemon(memory_dir: &Path) -> Result<Option<String>, RecallError> {
    let graph = memory_dir.join("graph");
    if !super::store::has_kv_manifest(&graph) {
        return Ok(None);
    }
    match super::store::open(&graph).await {
        Ok(db) => Ok(read_stamp(&db).await?),
        Err(GraphError::Locked(message)) => {
            match crate::serve_client::daemon_info(memory_dir).await? {
                Some(info) => Ok(info.pipeline_stamp),
                None => Err(GraphError::Locked(message).into()),
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn decide_cli(
    flag: Option<PipelineVersion>,
    configured: Option<&str>,
    stamp: Option<&str>,
) -> Result<PipelineChoice, RecallError> {
    if let Some(flag) = flag {
        if flag == PipelineVersion::V1 && stamp == Some(PipelineVersion::V2.as_str()) {
            return Err(RecallError::Other(V1_REFUSAL.to_string()));
        }
        return Ok(PipelineChoice {
            version: flag,
            notice: None,
        });
    }
    // A v2 stamp overrides a typo. Anything else keeps the CLI error, so an
    // unstamped store still refuses a bad config instead of silently running v1.
    let choice = decide_hook(configured, stamp);
    if stamp != Some(PipelineVersion::V2.as_str()) {
        if let Err(err) = PipelineVersion::resolve(None, configured) {
            return Err(RecallError::Config(err));
        }
    }
    Ok(choice)
}

fn decide_hook(configured: Option<&str>, stamp: Option<&str>) -> PipelineChoice {
    let stamped = stamp.and_then(PipelineVersion::parse);
    match PipelineVersion::resolve(None, configured) {
        Ok(version) => {
            if stamped == Some(PipelineVersion::V2) && version != PipelineVersion::V2 {
                let label = configured.unwrap_or("unset");
                PipelineChoice {
                    version: PipelineVersion::V2,
                    notice: Some(format!(
                        "[graph] pipeline is {label} but the store is stamped v2; extracting with v2"
                    )),
                }
            } else {
                PipelineChoice {
                    version,
                    notice: None,
                }
            }
        }
        Err(err) => match stamped {
            Some(version) => PipelineChoice {
                version,
                notice: Some(format!(
                    "{err}; the store is stamped {}, so extraction uses {}",
                    version.as_str(),
                    version.as_str()
                )),
            },
            None => PipelineChoice {
                version: PipelineVersion::V1,
                notice: Some(format!("{err}; staying on pipeline v1")),
            },
        },
    }
}

/// The store refused this pipeline. The message is the rebuild line.
#[must_use]
pub(crate) fn is_stamp_refusal(err: &GraphError) -> bool {
    err.to_string().contains(V2_REFUSAL)
}

/// What a rebuild dry-run quotes for one archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionEstimate {
    /// `turns/conversation-NNN.json` is present.
    pub has_sidecar: bool,
    /// Windows the structured pass would send.
    pub windows: u32,
    /// `windows` plus [`RESOLVER_CALL_BOUND`].
    pub calls: u32,
}

/// Windows and provider calls for one archive, without opening a store.
///
/// # Errors
///
/// The archive cannot be loaded. The same failure extraction would hit.
pub(crate) fn estimate_session(memory_dir: &Path, log: u32) -> Result<SessionEstimate, GraphError> {
    let has_sidecar = input::turns_sidecar(memory_dir, log).is_file();
    let session = input::load_session(memory_dir, log)?;
    let windows = u32::try_from(pass1::window_count(&session)).unwrap_or(u32::MAX);
    Ok(SessionEstimate {
        has_sidecar,
        windows,
        calls: windows.saturating_add(RESOLVER_CALL_BOUND),
    })
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

#[cfg(all(test, feature = "embedded"))]
mod tests {
    use std::path::Path;

    use super::*;

    fn write_pipeline(memory: &Path, value: &str) {
        std::fs::write(
            memory.join(".recall-echo.toml"),
            format!("[graph]\npipeline = \"{value}\"\n"),
        )
        .unwrap();
    }

    async fn stamp_v2(memory: &Path) -> GraphMemory {
        let gm = GraphMemory::open_embedded(&memory.join("graph"))
            .await
            .unwrap();
        ensure_v2_store(&gm).await.unwrap();
        gm
    }

    fn list_files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                if path.is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        if root.exists() {
            walk(root, root, &mut out);
        }
        out
    }

    #[tokio::test]
    async fn v2_store_and_a_config_typo_runs_the_hook_as_v2() {
        let tmp = tempfile::tempdir().unwrap();
        write_pipeline(tmp.path(), "typo");
        let gm = stamp_v2(tmp.path()).await;
        let choice = hook_choice(tmp.path(), gm.db()).await;
        assert_eq!(choice.version, PipelineVersion::V2);
        let notice = choice.notice.expect("config error");
        assert!(notice.contains("not typo"), "{notice}");
        assert!(notice.contains("stamped v2"), "{notice}");
        assert_eq!(
            pipeline_for_hook(tmp.path(), gm.db()).await,
            PipelineVersion::V2
        );
        assert_eq!(
            chosen_pipeline(tmp.path(), None, Some(gm.db()))
                .await
                .unwrap(),
            PipelineVersion::V2
        );
    }

    #[tokio::test]
    async fn v2_store_and_config_v1_or_missing_runs_v2_and_notices() {
        let tmp = tempfile::tempdir().unwrap();
        let gm = stamp_v2(tmp.path()).await;

        let missing = hook_choice(tmp.path(), gm.db()).await;
        assert_eq!(missing.version, PipelineVersion::V2);
        let notice = missing.notice.expect("unset notice");
        assert!(notice.contains("unset"), "{notice}");
        assert!(notice.contains("stamped v2"), "{notice}");
        assert_eq!(notice.lines().count(), 1, "{notice}");

        write_pipeline(tmp.path(), "v1");
        let configured = hook_choice(tmp.path(), gm.db()).await;
        assert_eq!(configured.version, PipelineVersion::V2);
        let notice = configured.notice.expect("v1 notice");
        assert!(notice.contains("pipeline is v1"), "{notice}");
        assert!(notice.contains("stamped v2"), "{notice}");
        assert_eq!(notice.lines().count(), 1, "{notice}");

        let cli = chosen_pipeline(tmp.path(), None, Some(gm.db()))
            .await
            .unwrap();
        assert_eq!(cli, PipelineVersion::V2);
    }

    #[tokio::test]
    async fn explicit_v1_on_a_v2_store_is_refused_and_the_archive_is_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_dir = tmp.path().join("conversations");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let archive = archive_dir.join("conversation-001.md");
        let body = b"---\nlog: 1\n---\n\n### User\n\nhello\n";
        std::fs::write(&archive, body).unwrap();
        {
            let gm = stamp_v2(tmp.path()).await;
            assert_eq!(gm.stats().await.unwrap().entity_count, 0);
            let err = chosen_pipeline(tmp.path(), Some(PipelineVersion::V1), Some(gm.db()))
                .await
                .expect_err("refused");
            assert!(err.to_string().contains(V1_REFUSAL), "{err}");
            assert_eq!(gm.stats().await.unwrap().entity_count, 0);
            assert_eq!(read_stamp(gm.db()).await.unwrap().as_deref(), Some("v2"));
        }
        assert_eq!(std::fs::read(&archive).unwrap(), body);
        let err = chosen_pipeline(tmp.path(), Some(PipelineVersion::V1), None)
            .await
            .expect_err("refused from disk");
        assert!(err.to_string().contains(V1_REFUSAL), "{err}");
        assert_eq!(std::fs::read(&archive).unwrap(), body);
    }

    #[tokio::test]
    async fn an_unstamped_store_keeps_today_behaviour() {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        assert!(read_stamp(gm.db()).await.unwrap().is_none());

        let hook = hook_choice(tmp.path(), gm.db()).await;
        assert_eq!(hook.version, PipelineVersion::V1);
        assert!(hook.notice.is_none());
        assert_eq!(
            chosen_pipeline(tmp.path(), None, Some(gm.db()))
                .await
                .unwrap(),
            PipelineVersion::V1
        );

        write_pipeline(tmp.path(), "v2");
        assert_eq!(
            hook_choice(tmp.path(), gm.db()).await.version,
            PipelineVersion::V2
        );
        assert!(hook_choice(tmp.path(), gm.db()).await.notice.is_none());
        assert_eq!(
            chosen_pipeline(tmp.path(), None, Some(gm.db()))
                .await
                .unwrap(),
            PipelineVersion::V2
        );

        write_pipeline(tmp.path(), "typo");
        let hook = hook_choice(tmp.path(), gm.db()).await;
        assert_eq!(hook.version, PipelineVersion::V1);
        let notice = hook.notice.expect("typo");
        assert!(notice.contains("not typo"), "{notice}");
        assert!(notice.contains("staying on pipeline v1"), "{notice}");
        let err = chosen_pipeline(tmp.path(), None, Some(gm.db()))
            .await
            .expect_err("cli typo");
        assert!(err.to_string().contains("not typo"), "{err}");

        assert_eq!(
            chosen_pipeline(tmp.path(), Some(PipelineVersion::V2), Some(gm.db()))
                .await
                .unwrap(),
            PipelineVersion::V2
        );
        assert_eq!(
            chosen_pipeline(tmp.path(), Some(PipelineVersion::V1), Some(gm.db()))
                .await
                .unwrap(),
            PipelineVersion::V1
        );
    }

    #[tokio::test]
    async fn reading_the_stamp_does_not_create_a_store() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = tmp.path().join("graph");
        std::fs::create_dir_all(graph.join("surreal")).unwrap();
        let before = list_files(&graph);
        let version = chosen_pipeline(tmp.path(), None, None).await.unwrap();
        assert_eq!(version, PipelineVersion::V1);
        assert_eq!(list_files(&graph), before);
        assert!(read_embedded_stamp(tmp.path()).await.unwrap().is_none());
        assert_eq!(list_files(&graph), before);
    }
}
