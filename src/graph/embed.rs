// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text embedding via fastembed (BGE-Small-EN-v1.5, 384 dimensions).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::error::GraphError;

/// Points every store at one shared models dir instead of `<store>/models`.
const MODELS_DIR_ENV: &str = "RECALL_ECHO_MODELS_DIR";

/// Hub repo and files the embedding crate fetches for BGE-Small-EN-v1.5.
const MODEL_REPO: &str = "Xenova/bge-small-en-v1.5";
const MODEL_FILES: [&str; 5] = [
    "onnx/model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

/// The models dir for `store`: `override_dir` when it is non-empty, else
/// `<store>/models`.
pub fn resolve_models_dir(store: &Path, override_dir: Option<&OsStr>) -> PathBuf {
    match override_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => store.join("models"),
    }
}

/// The models dir for `store`, honouring `RECALL_ECHO_MODELS_DIR`.
///
/// `HF_HOME`, when set, still wins: the embedding crate reads it in place of
/// the dir it is given.
pub fn models_dir(store: &Path) -> PathBuf {
    resolve_models_dir(store, std::env::var_os(MODELS_DIR_ENV).as_deref())
}

/// The cache the embedding crate reads for `cache_dir`: `HF_HOME` when it is
/// set, else `cache_dir`.
pub(crate) fn effective_cache_dir(cache_dir: &Path) -> PathBuf {
    resolve_effective_cache_dir(cache_dir, std::env::var("HF_HOME").ok().as_deref())
}

/// [`effective_cache_dir`] with the `HF_HOME` value passed in.
fn resolve_effective_cache_dir(cache_dir: &Path, hf_home: Option<&str>) -> PathBuf {
    hf_home.map_or_else(|| cache_dir.to_path_buf(), PathBuf::from)
}

/// Whether every model and tokenizer file is in the hub cache at `dir`.
pub(crate) fn model_cached(dir: &Path) -> bool {
    let repo = hf_hub::Cache::new(dir.to_path_buf()).model(MODEL_REPO.to_string());
    MODEL_FILES.iter().all(|file| repo.get(file).is_some())
}

/// The error for an offline run whose cache in `dir` lacks a model or
/// tokenizer file; `None` when the embedding crate may be called.
///
/// The hub client does not honour `HF_HUB_OFFLINE`: handed an empty cache,
/// it downloads the model anyway.
pub fn offline_cache_missing(dir: &Path, offline: bool) -> Option<String> {
    if !offline || model_cached(dir) {
        return None;
    }
    Some(format!(
        "HF_HUB_OFFLINE is set and the embedding model is not cached in {}",
        dir.display()
    ))
}

/// Whether `HF_HUB_OFFLINE` is `1`, `true`, `yes` or `on` (any case).
fn hub_offline() -> bool {
    std::env::var("HF_HUB_OFFLINE").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Trait for embedding text into vectors.
pub trait Embedder: Send + Sync {
    fn embed(&self, texts: Vec<&str>) -> Result<Vec<Vec<f32>>, GraphError>;
    fn embed_single(&self, text: &str) -> Result<Vec<f32>, GraphError>;
    fn dimensions(&self) -> usize;
}

/// Local embedding using fastembed (BGE-Small-EN-v1.5, 384 dimensions).
pub struct FastEmbedder {
    model: TextEmbedding,
}

impl FastEmbedder {
    pub fn new(cache_dir: &Path) -> Result<Self, GraphError> {
        // The embedding crate reads HF_HOME in place of `cache_dir`, so that
        // is the cache to check.
        let effective_dir = effective_cache_dir(cache_dir);
        if let Some(message) = offline_cache_missing(&effective_dir, hub_offline()) {
            return Err(GraphError::Embed(message));
        }

        let options = InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true);

        let model =
            TextEmbedding::try_new(options).map_err(|e| GraphError::Embed(e.to_string()))?;
        Ok(Self { model })
    }
}

/// Lazily-initialized [`FastEmbedder`].
///
/// Construction is free — the ONNX model is loaded (and downloaded on first
/// ever use) only when an operation actually needs an embedding. Operations
/// that never embed (schema init, CRUD reads, GC, status) never touch the
/// network or pay the model-load cost. This also keeps unit tests that open
/// a graph store fully offline.
pub struct LazyEmbedder {
    cache_dir: std::path::PathBuf,
    cell: std::sync::OnceLock<FastEmbedder>,
    init_lock: std::sync::Mutex<()>,
}

impl LazyEmbedder {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
            cell: std::sync::OnceLock::new(),
            init_lock: std::sync::Mutex::new(()),
        }
    }

    /// Get the embedder, initializing it on first use.
    pub fn get(&self) -> Result<&FastEmbedder, GraphError> {
        if let Some(e) = self.cell.get() {
            return Ok(e);
        }
        // Serialize initialization; losers of the race find the cell filled.
        let _guard = self
            .init_lock
            .lock()
            .map_err(|_| GraphError::Embed("embedder init lock poisoned".into()))?;
        if self.cell.get().is_none() {
            let embedder = FastEmbedder::new(&self.cache_dir)?;
            let _ = self.cell.set(embedder);
        }
        self.cell
            .get()
            .ok_or_else(|| GraphError::Embed("embedder cell empty after init".into()))
    }
}

impl Embedder for FastEmbedder {
    fn embed(&self, texts: Vec<&str>) -> Result<Vec<Vec<f32>>, GraphError> {
        let docs: Vec<String> = texts.into_iter().map(|t| t.to_string()).collect();
        let embeddings = self
            .model
            .embed(docs, None)
            .map_err(|e| GraphError::Embed(e.to_string()))?;
        Ok(embeddings)
    }

    fn embed_single(&self, text: &str) -> Result<Vec<f32>, GraphError> {
        let embeddings = self
            .model
            .embed(vec![text.to_string()], None)
            .map_err(|e| GraphError::Embed(e.to_string()))?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| GraphError::Embed("no embedding returned".into()))
    }

    fn dimensions(&self) -> usize {
        384
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_models_dir_without_an_override_is_the_store_models_dir() {
        let store = Path::new("/srv/memory/graph");
        assert_eq!(resolve_models_dir(store, None), store.join("models"));
    }

    #[test]
    fn resolve_models_dir_treats_an_empty_override_as_unset() {
        let store = Path::new("/srv/memory/graph");
        assert_eq!(
            resolve_models_dir(store, Some(OsStr::new(""))),
            store.join("models")
        );
    }

    #[test]
    fn resolve_models_dir_uses_a_non_empty_override() {
        let store = Path::new("/srv/memory/graph");
        assert_eq!(
            resolve_models_dir(store, Some(OsStr::new("/var/cache/models"))),
            PathBuf::from("/var/cache/models")
        );
    }

    #[test]
    fn offline_cache_missing_is_none_when_not_offline() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(offline_cache_missing(tmp.path(), false), None);
    }

    #[test]
    fn offline_cache_missing_names_an_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            offline_cache_missing(tmp.path(), true),
            Some(format!(
                "HF_HUB_OFFLINE is set and the embedding model is not cached in {}",
                tmp.path().display()
            ))
        );
    }

    /// A hub cache layout under `dir` with every model file present and empty.
    fn write_complete_cache_layout(dir: &Path) {
        let repo = dir.join("models--Xenova--bge-small-en-v1.5");
        let commit = "0123456789abcdef";
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("refs").join("main"), commit).unwrap();
        for file in MODEL_FILES {
            let path = repo.join("snapshots").join(commit).join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }

    #[test]
    fn offline_cache_missing_is_none_for_a_complete_cache_layout() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_cache_layout(tmp.path());
        assert_eq!(offline_cache_missing(tmp.path(), true), None);
    }

    #[test]
    fn model_cached_is_false_for_an_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!model_cached(tmp.path()));
    }

    #[test]
    fn model_cached_is_false_for_a_dir_with_an_unrelated_entry() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cache.bin"), b"model").unwrap();
        assert!(!model_cached(tmp.path()));
    }

    #[test]
    fn model_cached_is_true_for_a_complete_cache_layout() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_cache_layout(tmp.path());
        assert!(model_cached(tmp.path()));
    }

    #[test]
    fn effective_cache_dir_prefers_hf_home_when_it_is_set() {
        let cache_dir = Path::new("/srv/memory/graph/models");
        assert_eq!(
            resolve_effective_cache_dir(cache_dir, None),
            cache_dir.to_path_buf()
        );
        assert_eq!(
            resolve_effective_cache_dir(cache_dir, Some("/var/cache/hf")),
            PathBuf::from("/var/cache/hf")
        );
    }
}
