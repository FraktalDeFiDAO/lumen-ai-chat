//! Embedding provider interface.
//!
//! `embed_batch` is the primary method and `embed` defaults to it, rather than
//! the other way round. Indexing a repository one function at a time is the
//! single biggest throughput loss in a local-ONNX setup and one round trip per
//! symbol against a remote embedder.
//!
//! Everything here is synchronous on purpose. Embedding is CPU-bound; putting it
//! behind `async fn` invites callers to run it on a reactor thread. Callers on
//! tokio should wrap these in `spawn_blocking`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("embedding failed: {0}")]
    Failed(String),
    #[error("expected {expected} dimensions, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
}

pub trait Embedder: Send + Sync {
    fn dimension(&self) -> usize;

    /// Maximum input length in characters. Text beyond this is truncated by the
    /// caller before embedding — MiniLM-class models cut off around 512 tokens,
    /// so a long function body is mostly absent from its own embedding.
    fn max_input_chars(&self) -> usize {
        2000
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(std::slice::from_ref(&text.to_string()))?;
        out.pop()
            .ok_or_else(|| EmbedError::Failed("empty batch result".into()))
    }
}

/// Deterministic hashed bag-of-tokens embedder.
///
/// Not a semantic model, but it is stable, dependency-free, and genuinely
/// similarity-bearing: texts sharing tokens get higher cosine. That makes it
/// usable for recall benchmarking and integration tests, where a mock returning
/// a constant vector would make every test vacuous.
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn hash(token: &str, seed: u64) -> u64 {
        // FNV-1a with a seed, so we can take several independent projections.
        let mut h: u64 = 0xcbf29ce484222325 ^ seed;
        for b in token.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
}

impl Embedder for HashEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let mut v = vec![0.0f32; self.dim];
            for token in crate::memory::tokenize(text) {
                for seed in 0..3u64 {
                    let h = Self::hash(&token, seed);
                    let idx = (h % self.dim as u64) as usize;
                    let sign = if (h >> 63) & 1 == 1 { -1.0 } else { 1.0 };
                    v[idx] += sign;
                }
            }
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            out.push(v);
        }
        Ok(out)
    }
}
