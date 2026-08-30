//! int8 dot-product kernels.
//!
//! Two fixes over the naive version:
//!   * kernels clamp to `min(a.len(), b.len())` so a length mismatch is a wrong
//!     answer, not an out-of-bounds read inside `unsafe`;
//!   * CPU feature detection is resolved once into a function pointer instead of
//!     branching on `is_x86_feature_detected!` in HNSW's innermost loop.

use std::sync::OnceLock;

pub type DotI8Fn = fn(&[i8], &[i8]) -> i32;

// ---------------------------------------------------------------- scalar

pub fn dot_i8_scalar(a: &[i8], b: &[i8]) -> i32 {
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as i32) * (y as i32))
        .sum()
}

/// Asymmetric kernel: full-precision query against a quantized document.
///
/// Keeping the query in f32 costs nothing (it is scored once per search, not
/// once per comparison) and avoids the precision loss of quantizing it.
pub fn dot_f32_i8(q: &[f32], d: &[i8]) -> f32 {
    let n = q.len().min(d.len());
    let (q, d) = (&q[..n], &d[..n]);

    // Written as chunks so LLVM emits packed FMAs.
    let mut acc = [0.0f32; 8];
    let mut i = 0;
    while i + 8 <= n {
        for k in 0..8 {
            acc[k] += q[i + k] * d[i + k] as f32;
        }
        i += 8;
    }
    let mut sum: f32 = acc.iter().sum();
    while i < n {
        sum += q[i] * d[i] as f32;
        i += 1;
    }
    sum
}

// ---------------------------------------------------------------- avx2

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_i8_avx2_inner(a: &[i8], b: &[i8]) -> i32 {
    use std::arch::x86_64::*;

    let len = a.len().min(b.len());
    let mut i = 0usize;
    let mut acc = _mm256_setzero_si256();

    // 32 int8 lanes per iteration. Sign-extend each 128-bit half to i16, then
    // madd_epi16 does the pairwise multiply-add into i32 lanes. For D<=8192 the
    // i32 accumulator cannot overflow (127*127*8192 < 2^31).
    while i + 32 <= len {
        let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
        let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);

        let va16_lo = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(va));
        let va16_hi = _mm256_cvtepi8_epi16(_mm256_extracti128_si256::<1>(va));
        let vb16_lo = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vb));
        let vb16_hi = _mm256_cvtepi8_epi16(_mm256_extracti128_si256::<1>(vb));

        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(va16_lo, vb16_lo));
        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(va16_hi, vb16_hi));
        i += 32;
    }

    let mut buf = [0i32; 8];
    _mm256_storeu_si256(buf.as_mut_ptr() as *mut __m256i, acc);
    let mut sum: i32 = buf.iter().sum();

    while i < len {
        sum += (*a.get_unchecked(i) as i32) * (*b.get_unchecked(i) as i32);
        i += 1;
    }
    sum
}

/// Safe wrapper. Only ever installed in the dispatch cell when AVX2 was detected.
#[cfg(target_arch = "x86_64")]
fn dot_i8_avx2(a: &[i8], b: &[i8]) -> i32 {
    unsafe { dot_i8_avx2_inner(a, b) }
}

// ---------------------------------------------------------------- neon

#[cfg(target_arch = "aarch64")]
unsafe fn dot_i8_neon_inner(a: &[i8], b: &[i8]) -> i32 {
    use std::arch::aarch64::*;

    let len = a.len().min(b.len());
    let mut i = 0usize;
    let mut acc = vdupq_n_s32(0);

    while i + 16 <= len {
        let va = vld1q_s8(a.as_ptr().add(i));
        let vb = vld1q_s8(b.as_ptr().add(i));
        acc = vpadalq_s16(acc, vmull_s8(vget_low_s8(va), vget_low_s8(vb)));
        acc = vpadalq_s16(acc, vmull_s8(vget_high_s8(va), vget_high_s8(vb)));
        i += 16;
    }

    let mut sum = vaddvq_s32(acc);
    while i < len {
        sum += (*a.get_unchecked(i) as i32) * (*b.get_unchecked(i) as i32);
        i += 1;
    }
    sum
}

#[cfg(target_arch = "aarch64")]
fn dot_i8_neon(a: &[i8], b: &[i8]) -> i32 {
    // NEON is baseline on aarch64, so no runtime detection is needed.
    unsafe { dot_i8_neon_inner(a, b) }
}

// ---------------------------------------------------------------- dispatch

static DOT_I8: OnceLock<DotI8Fn> = OnceLock::new();

fn resolve_dot_i8() -> DotI8Fn {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return dot_i8_avx2;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        return dot_i8_neon;
    }
    #[allow(unreachable_code)]
    dot_i8_scalar
}

/// Resolved once per process; call this at startup to keep the first query warm.
#[inline]
pub fn dot_i8_kernel() -> DotI8Fn {
    *DOT_I8.get_or_init(resolve_dot_i8)
}

/// Panics on a length mismatch rather than reading out of bounds.
#[inline]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    assert_eq!(a.len(), b.len(), "int8 dot product length mismatch");
    dot_i8_kernel()(a, b)
}

pub fn active_kernel_name() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return "avx2";
        }
        return "scalar";
    }
    #[cfg(target_arch = "aarch64")]
    {
        return "neon";
    }
    #[allow(unreachable_code)]
    "scalar"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo(n: usize, seed: i32) -> Vec<i8> {
        (0..n)
            .map(|i| (((i as i32 * 37 + seed * 17) % 255) - 127) as i8)
            .collect()
    }

    /// The tail paths never execute at D=384; exercise them explicitly.
    #[test]
    fn simd_matches_scalar_across_lengths() {
        for &n in &[
            0usize, 1, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 127, 384, 385,
        ] {
            let a = pseudo(n, 1);
            let b = pseudo(n, 2);
            assert_eq!(
                dot_i8_scalar(&a, &b),
                dot_i8(&a, &b),
                "mismatch at len {}",
                n
            );
        }
    }

    #[test]
    fn extremes_do_not_overflow() {
        let a = vec![-127i8; 8192];
        let b = vec![127i8; 8192];
        assert_eq!(dot_i8(&a, &b), -127 * 127 * 8192);
    }

    #[test]
    fn asymmetric_agrees_with_reference() {
        for &n in &[0usize, 1, 7, 8, 9, 384, 385] {
            let d = pseudo(n, 3);
            let q: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin()).collect();
            let want: f32 = q.iter().zip(d.iter()).map(|(&x, &y)| x * y as f32).sum();
            let got = dot_f32_i8(&q, &d);
            assert!((want - got).abs() < 1e-3, "len {}: {} vs {}", n, want, got);
        }
    }
}
