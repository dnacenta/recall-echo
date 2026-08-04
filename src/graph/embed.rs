//! Text embedding via fastembed (BGE-Small-EN-v1.5, 384 dimensions).

use std::path::Path;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::error::GraphError;

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
