use std::path::Path;

use anyhow::Result;
use bread_onnx::embedding::EmbeddingSession;
use bread_onnx::Provider;

/// all-MiniLM-L6-v2's trained max sequence length. Release/show titles are
/// always far shorter than this, but truncate defensively rather than let a
/// pathological input blow up attention memory.
const MAX_SEQ_LEN: usize = 256;

pub struct OrtEmbedder {
    inner: EmbeddingSession,
}

impl OrtEmbedder {
    /// CPU-only — no execution-provider selection. A GPU on a typical media
    /// server is usually already busy with transcoding, and a 90MB
    /// MiniLM-class model is cheap enough on CPU that a multi-backend GPU
    /// setup isn't worth the added complexity for a model this small.
    pub fn load(model_path: &Path, tokenizer_path: &Path, dim: usize) -> Result<Self> {
        let inner = EmbeddingSession::load(model_path, tokenizer_path, dim, MAX_SEQ_LEN, &[Provider::Cpu])?;
        Ok(Self { inner })
    }

    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        self.inner.embed(text)
    }
}

pub use bread_onnx::embedding::cosine_similarity;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_of_identical_unit_vectors_is_one() {
        let v = vec![0.6, 0.8]; // already unit length
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }
}
