use std::path::Path;

use anyhow::Result;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

/// all-MiniLM-L6-v2's trained max sequence length. Release/show titles are
/// always far shorter than this, but truncate defensively rather than let a
/// pathological input blow up attention memory.
const MAX_SEQ_LEN: usize = 256;

pub struct OrtEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    dim: usize,
}

impl OrtEmbedder {
    /// CPU-only — no execution-provider selection. A GPU on a typical media
    /// server is usually already busy with transcoding, and a 90MB
    /// MiniLM-class model is cheap enough on CPU that a multi-backend GPU
    /// setup isn't worth the added complexity for a model this small.
    pub fn load(model_path: &Path, tokenizer_path: &Path, dim: usize) -> Result<Self> {
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create ort session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("failed to set optimization level: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| {
                anyhow::anyhow!("failed to load model from {}: {e}", model_path.display())
            })?;

        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        Ok(Self {
            session,
            tokenizer,
            dim,
        })
    }

    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let mut ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let mut mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let mut type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&x| x as i64).collect();

        ids.truncate(MAX_SEQ_LEN);
        mask.truncate(MAX_SEQ_LEN);
        type_ids.truncate(MAX_SEQ_LEN);

        let seq_len = ids.len() as i64;
        let id_tensor = Tensor::<i64>::from_array((vec![1i64, seq_len], ids))
            .map_err(|e| anyhow::anyhow!("failed to build input_ids tensor: {e}"))?;
        let mask_tensor = Tensor::<i64>::from_array((vec![1i64, seq_len], mask.clone()))
            .map_err(|e| anyhow::anyhow!("failed to build attention_mask tensor: {e}"))?;
        let type_tensor = Tensor::<i64>::from_array((vec![1i64, seq_len], type_ids))
            .map_err(|e| anyhow::anyhow!("failed to build token_type_ids tensor: {e}"))?;

        let outputs = self
            .session
            .run(ort::inputs! {
                "input_ids" => id_tensor,
                "attention_mask" => mask_tensor,
                "token_type_ids" => type_tensor,
            })
            .map_err(|e| anyhow::anyhow!("ort inference failed: {e}"))?;

        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract last_hidden_state: {e}"))?;

        let actual_seq = shape[1] as usize;
        let actual_dim = shape[2] as usize;

        // Mean-pool over non-padded positions only.
        let mut result = vec![0.0f32; actual_dim];
        let mut count = 0usize;
        for t in 0..actual_seq.min(mask.len()) {
            if mask[t] > 0 {
                for d in 0..actual_dim {
                    result[d] += data[t * actual_dim + d];
                }
                count += 1;
            }
        }
        if count > 0 {
            for x in &mut result {
                *x /= count as f32;
            }
        }

        l2_normalize(&mut result);
        result.truncate(self.dim);
        while result.len() < self.dim {
            result.push(0.0);
        }

        Ok(result)
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-10 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_produces_unit_vector() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_leaves_zero_vector_untouched() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn cosine_similarity_of_identical_unit_vectors_is_one() {
        let mut v = vec![1.0, 2.0, 3.0];
        l2_normalize(&mut v);
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
