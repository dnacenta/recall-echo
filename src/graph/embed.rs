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

/// The error for an offline run whose cache in `dir` lacks a model or
/// tokenizer file; `None` when the embedding crate may be called.
///
/// The hub client does not honour `HF_HUB_OFFLINE`: handed an empty cache,
/// it downloads the model anyway.
pub fn offline_cache_missing(dir: &Path, offline: bool) -> Option<String> {
    if !offline {
        return None;
    }
    let repo = hf_hub::Cache::new(dir.to_path_buf()).model(MODEL_REPO.to_string());
    if MODEL_FILES.iter().all(|file| repo.get(file).is_some()) {
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
        let effective_dir = std::env::var("HF_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| cache_dir.to_path_buf());
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

    #[test]
    fn offline_cache_missing_is_none_for_a_complete_cache_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("models--Xenova--bge-small-en-v1.5");
        let commit = "0123456789abcdef";
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("refs").join("main"), commit).unwrap();
        for file in MODEL_FILES {
            let path = repo.join("snapshots").join(commit).join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        assert_eq!(offline_cache_missing(tmp.path(), true), None);
    }
}
