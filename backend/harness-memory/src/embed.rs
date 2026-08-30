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

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn every_vector_has_the_requested_width() {
        let embedder = HashEmbedder::new(384);
        assert_eq!(embedder.dimension(), 384);
        assert_eq!(embedder.embed("fn main() {}").unwrap().len(), 384);
    }

    #[test]
    fn the_same_text_always_embeds_the_same_way() {
        let embedder = HashEmbedder::new(64);
        assert_eq!(
            embedder.embed("pub fn compact_transcript()").unwrap(),
            embedder.embed("pub fn compact_transcript()").unwrap()
        );
    }

    #[test]
    fn a_vector_is_normalised_so_cosine_is_a_dot_product() {
        let embedder = HashEmbedder::new(128);
        let vector = embedder.embed("the harness compacts a transcript").unwrap();
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[test]
    fn shared_tokens_score_higher_than_unrelated_text() {
        let embedder = HashEmbedder::new(512);
        let query = embedder.embed("compact the transcript budget").unwrap();
        let related = embedder
            .embed("fn compact transcript within a budget")
            .unwrap();
        let unrelated = embedder.embed("quantise vectors into int8 rows").unwrap();

        assert!(
            cosine(&query, &related) > cosine(&query, &unrelated),
            "related {} should beat unrelated {}",
            cosine(&query, &related),
            cosine(&query, &unrelated)
        );
    }

    #[test]
    fn an_empty_text_embeds_to_zeroes_rather_than_failing() {
        let embedder = HashEmbedder::new(32);
        let vector = embedder.embed("   ").unwrap();
        assert_eq!(vector.len(), 32);
        assert!(vector.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn a_batch_is_the_same_as_embedding_each_text_on_its_own() {
        let embedder = HashEmbedder::new(64);
        let texts = vec!["first text".to_string(), "second text".to_string()];
        let batch = embedder.embed_batch(&texts).unwrap();

        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], embedder.embed("first text").unwrap());
        assert_eq!(batch[1], embedder.embed("second text").unwrap());
    }

    #[test]
    fn the_input_budget_matches_a_minilm_context() {
        assert_eq!(HashEmbedder::new(8).max_input_chars(), 2000);
    }
}
