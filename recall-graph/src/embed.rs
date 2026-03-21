//! Text embedding via fastembed (BGE-Small-EN-v1.5, 384 dimensions).

use std::path::Path;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::error::GraphError;

/// Trait for embedding text into vectors.
pub trait Embedder {
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
