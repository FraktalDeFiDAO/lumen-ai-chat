//! Vector memory for an agent coding harness: row-aligned storage, an HNSW
//! index, and a retrieval layer that can name what it found.

pub mod embed;
pub mod hnsw;
pub mod memory;
pub mod quant;
pub mod simd;
pub mod store;
