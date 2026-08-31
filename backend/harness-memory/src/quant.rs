//! int8 storage format and the query types that score against it.

use serde::{Deserialize, Serialize};

use crate::simd;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuantizedVector {
    pub bytes: Vec<i8>,
    pub scale: f32,
    pub norm: f32,
}

impl QuantizedVector {
    /// Symmetric max-abs quantization.
    pub fn quantize(v: &[f32]) -> Self {
        let max_abs = v.iter().fold(0.0f32, |acc, &x| acc.max(x.abs()));
        Self::quantize_with_bound(v, max_abs)
    }

    /// Clips at a quantile of |v| before scaling.
    ///
    /// Transformer embeddings reliably carry a few high-magnitude dimensions.
    /// Under max-abs scaling those compress every other dimension into a handful
    /// of levels; clipping trades exactness on the outliers for resolution
    /// everywhere else.
    pub fn quantize_clipped(v: &[f32], quantile: f32) -> Self {
        if v.is_empty() {
            return Self::quantize(v);
        }
        let mut mags: Vec<f32> = v.iter().map(|x| x.abs()).collect();
        mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (((mags.len() - 1) as f32) * quantile.clamp(0.0, 1.0)).round() as usize;
        Self::quantize_with_bound(v, mags[idx])
    }

    fn quantize_with_bound(v: &[f32], bound: f32) -> Self {
        let scale = if bound > 0.0 { bound / 127.0 } else { 1.0 };
        let bytes: Vec<i8> = v
            .iter()
            .map(|&x| (x / scale).round().clamp(-127.0, 127.0) as i8)
            .collect();

        let int_sum_sq: i64 = bytes.iter().map(|&b| (b as i64) * (b as i64)).sum();
        let norm = (int_sum_sq as f32).sqrt() * scale;

        Self { bytes, scale, norm }
    }

    pub fn dim(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_view(&self) -> VectorView<'_> {
        VectorView {
            scale: self.scale,
            norm: self.norm,
            bytes: &self.bytes,
        }
    }

    /// Serialized record size in the on-disk format: scale + norm + payload.
    pub fn record_size(dim: usize) -> usize {
        8 + dim
    }
}

/// Borrowed view over one record, typically pointing straight into mmap pages.
#[derive(Clone, Copy, Debug)]
pub struct VectorView<'a> {
    pub scale: f32,
    pub norm: f32,
    pub bytes: &'a [i8],
}

impl<'a> VectorView<'a> {
    #[inline]
    pub fn dot(&self, other: &VectorView<'_>) -> f32 {
        (simd::dot_i8(self.bytes, other.bytes) as f32) * self.scale * other.scale
    }

    #[inline]
    pub fn cosine(&self, other: &VectorView<'_>) -> f32 {
        if self.norm == 0.0 || other.norm == 0.0 {
            return 0.0;
        }
        self.dot(other) / (self.norm * other.norm)
    }

    pub fn to_owned(&self) -> QuantizedVector {
        QuantizedVector {
            bytes: self.bytes.to_vec(),
            scale: self.scale,
            norm: self.norm,
        }
    }
}

/// How a search key scores against stored records.
///
/// Graph construction compares stored record to stored record (int8 x int8);
/// user queries compare full-precision input to stored records (f32 x int8).
/// Both are expressed as cosine *distance* so lower is better.
pub trait Query {
    fn distance(&self, doc: &VectorView<'_>) -> f32;
    fn dim(&self) -> usize;
}

/// Query key used during graph construction.
pub struct Int8Query(pub QuantizedVector);

impl Query for Int8Query {
    #[inline]
    fn distance(&self, doc: &VectorView<'_>) -> f32 {
        1.0 - self.0.as_view().cosine(doc)
    }
    fn dim(&self) -> usize {
        self.0.dim()
    }
}

/// Query key used at search time. Never quantized.
pub struct F32Query {
    data: Vec<f32>,
    norm: f32,
}

impl F32Query {
    pub fn new(v: impl Into<Vec<f32>>) -> Self {
        let data = v.into();
        let norm = data.iter().map(|x| x * x).sum::<f32>().sqrt();
        Self { data, norm }
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.data
    }
}

impl Query for F32Query {
    #[inline]
    fn distance(&self, doc: &VectorView<'_>) -> f32 {
        if self.norm == 0.0 || doc.norm == 0.0 {
            return 1.0;
        }
        let dot = simd::dot_f32_i8(&self.data, doc.bytes) * doc.scale;
        1.0 - dot / (self.norm * doc.norm)
    }
    fn dim(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos_f32(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    }

    /// Quantization error can cancel on any single pair, so a one-pair check
    /// proves nothing in either direction. Measure the mean over a population.
    #[test]
    fn asymmetric_beats_symmetric_on_average() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);

        let mut sym_total = 0.0f64;
        let mut asym_total = 0.0f64;
        let pairs = 400;

        for _ in 0..pairs {
            let a: Vec<f32> = (0..384).map(|_| rng.random_range(-1.0f32..1.0)).collect();
            let b: Vec<f32> = (0..384).map(|_| rng.random_range(-1.0f32..1.0)).collect();
            let truth = cos_f32(&a, &b);

            let qa = QuantizedVector::quantize(&a);
            let qb = QuantizedVector::quantize(&b);

            let symmetric = qa.as_view().cosine(&qb.as_view());
            let asymmetric = 1.0 - F32Query::new(a.clone()).distance(&qb.as_view());

            sym_total += (truth - symmetric).abs() as f64;
            asym_total += (truth - asymmetric).abs() as f64;
        }

        let sym_mean = sym_total / pairs as f64;
        let asym_mean = asym_total / pairs as f64;

        assert!(
            asym_mean < sym_mean,
            "asymmetric mean error {:.3e} should beat symmetric {:.3e}",
            asym_mean,
            sym_mean
        );
        assert!(
            asym_mean < 1e-3,
            "asymmetric error too large: {:.3e}",
            asym_mean
        );
    }

    #[test]
    fn clipping_helps_when_one_dimension_dominates() {
        // One outlier dimension 50x the rest: the classic transformer case.
        let mut v: Vec<f32> = (0..384).map(|i| (i as f32 * 0.01).sin() * 0.02).collect();
        v[7] = 1.0;

        let plain = QuantizedVector::quantize(&v);
        let clipped = QuantizedVector::quantize_clipped(&v, 0.99);

        // Reconstruction error over the non-outlier dimensions.
        let err = |q: &QuantizedVector| -> f32 {
            v.iter()
                .zip(q.bytes.iter())
                .enumerate()
                .filter(|(i, _)| *i != 7)
                .map(|(_, (orig, &b))| (orig - b as f32 * q.scale).abs())
                .sum()
        };

        assert!(err(&clipped) < err(&plain), "clipping did not reduce error");
    }

    #[test]
    fn zero_vector_is_not_nan() {
        let z = QuantizedVector::quantize(&[0.0f32; 16]);
        assert_eq!(z.norm, 0.0);
        assert_eq!(z.as_view().cosine(&z.as_view()), 0.0);
        assert!(F32Query::new(vec![0.0f32; 16])
            .distance(&z.as_view())
            .is_finite());
    }
}
