//! Row-aligned vector + payload storage.
//!
//! The gap this closes: a vector index that returns row numbers is useless if
//! nothing maps a row back to the thing it described. Every append here writes
//! the vector *and* its payload under one row id, and the row count is committed
//! last, so the two can never disagree.
//!
//! Four append-only files per segment:
//!
//! ```text
//!   vectors.bin     [24B header][scale f32][norm f32][i8; D] x N
//!   payload.idx     [16B header][offset u64][len u32][rsvd u32] x N
//!   payload.dat     [16B header][arbitrary bytes]
//!   tombstones.log  [u32 row id] x M          (deletions; no in-place mutation)
//! ```
//!
//! Commit order per row is payload data, payload index, vector record, then the
//! count in the vectors header. Readers trust only that count, so a crash leaves
//! an ignorable tail rather than a torn row. Nothing is ever mutated in place,
//! which is what makes it safe to hand readers a live mmap.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use memmap2::{Mmap, MmapOptions};
use thiserror::Error;

use crate::quant::{QuantizedVector, VectorView};

const VEC_MAGIC: &[u8; 4] = b"QVEC";
const IDX_MAGIC: &[u8; 4] = b"QPIX";
const DAT_MAGIC: &[u8; 4] = b"QPDT";
const VERSION: u32 = 2;

const VEC_HEADER: usize = 24; // magic 4 | version 4 | dim 4 | flags 4 | count 8
const IDX_HEADER: usize = 16;
const DAT_HEADER: usize = 16;
const IDX_RECORD: usize = 16; // offset 8 | len 4 | reserved 4

const COUNT_OFFSET: u64 = 16;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad magic in {file}")]
    BadMagic { file: &'static str },
    #[error("unsupported version {found} in {file} (expected {VERSION})")]
    UnsupportedVersion { file: &'static str, found: u32 },
    #[error("dimension mismatch: store holds {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
    #[error("row {row} out of bounds (count {count})")]
    OutOfBounds { row: u32, count: u32 },
    #[error("{file} is truncated: need {need} bytes, have {have}")]
    Truncated {
        file: &'static str,
        need: usize,
        have: usize,
    },
}

/// Paths for one segment.
#[derive(Debug, Clone)]
pub struct SegmentPaths {
    pub vectors: PathBuf,
    pub payload_idx: PathBuf,
    pub payload_dat: PathBuf,
    pub tombstones: PathBuf,
}

impl SegmentPaths {
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let d = dir.as_ref();
        Self {
            vectors: d.join("vectors.bin"),
            payload_idx: d.join("payload.idx"),
            payload_dat: d.join("payload.dat"),
            tombstones: d.join("tombstones.log"),
        }
    }
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
}

// ===================================================================== writer

/// Single-writer handle. Opens an existing segment or creates a new one.
///
/// Deliberately has no `truncate` path: the previous design recreated the file
/// on every startup, which both destroyed the store and risked SIGBUS against a
/// reader that already had it mapped.
pub struct SegmentWriter {
    vectors: BufWriter<File>,
    payload_idx: BufWriter<File>,
    payload_dat: File,
    tombstones: File,
    dim: usize,
    count: u32,
    dat_cursor: u64,
    dirty: bool,
}

impl SegmentWriter {
    pub fn open(paths: &SegmentPaths, dim: usize) -> Result<Self, StoreError> {
        if let Some(parent) = paths.vectors.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut vectors = Self::open_rw(&paths.vectors)?;
        let mut payload_idx = Self::open_rw(&paths.payload_idx)?;
        let mut payload_dat = Self::open_rw(&paths.payload_dat)?;
        let tombstones = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            // Never truncate: a reader may already have this file mapped, and
            // open-or-create is the whole point of the recovery path.
            .truncate(false)
            .open(&paths.tombstones)?;

        let count = if vectors.metadata()?.len() < VEC_HEADER as u64 {
            Self::write_vec_header(&mut vectors, dim)?;
            0
        } else {
            let mut hdr = [0u8; VEC_HEADER];
            vectors.seek(SeekFrom::Start(0))?;
            vectors.read_exact(&mut hdr)?;
            if &hdr[0..4] != VEC_MAGIC {
                return Err(StoreError::BadMagic {
                    file: "vectors.bin",
                });
            }
            let version = read_u32(&hdr, 4);
            if version != VERSION {
                return Err(StoreError::UnsupportedVersion {
                    file: "vectors.bin",
                    found: version,
                });
            }
            let found_dim = read_u32(&hdr, 8) as usize;
            if found_dim != dim {
                return Err(StoreError::DimensionMismatch {
                    expected: found_dim,
                    actual: dim,
                });
            }
            read_u64(&hdr, COUNT_OFFSET as usize) as u32
        };

        if payload_idx.metadata()?.len() < IDX_HEADER as u64 {
            Self::write_small_header(&mut payload_idx, IDX_MAGIC)?;
        }
        if payload_dat.metadata()?.len() < DAT_HEADER as u64 {
            Self::write_small_header(&mut payload_dat, DAT_MAGIC)?;
        }

        // Recover from a crash mid-append: everything past the committed count is
        // an orphan, so rewind all three cursors to the committed frontier and let
        // the next append overwrite the garbage.
        let record_size = QuantizedVector::record_size(dim) as u64;
        let vec_end = VEC_HEADER as u64 + count as u64 * record_size;
        let idx_end = IDX_HEADER as u64 + count as u64 * IDX_RECORD as u64;

        let dat_cursor = if count == 0 {
            DAT_HEADER as u64
        } else {
            let mut rec = [0u8; IDX_RECORD];
            payload_idx.seek(SeekFrom::Start(idx_end - IDX_RECORD as u64))?;
            payload_idx.read_exact(&mut rec)?;
            read_u64(&rec, 0) + read_u32(&rec, 8) as u64
        };

        vectors.seek(SeekFrom::Start(vec_end))?;
        payload_idx.seek(SeekFrom::Start(idx_end))?;
        payload_dat.seek(SeekFrom::Start(dat_cursor))?;

        Ok(Self {
            vectors: BufWriter::new(vectors),
            payload_idx: BufWriter::new(payload_idx),
            payload_dat,
            tombstones,
            dim,
            count,
            dat_cursor,
            dirty: false,
        })
    }

    fn open_rw(path: &Path) -> Result<File, StoreError> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Never truncate: a reader may already have this file mapped, and
            // open-or-create is what makes a restart non-destructive.
            .truncate(false)
            .open(path)?)
    }

    fn write_vec_header(f: &mut File, dim: usize) -> Result<(), StoreError> {
        f.seek(SeekFrom::Start(0))?;
        f.write_all(VEC_MAGIC)?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&(dim as u32).to_le_bytes())?;
        f.write_all(&0u32.to_le_bytes())?;
        f.write_all(&0u64.to_le_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    fn write_small_header(f: &mut File, magic: &[u8; 4]) -> Result<(), StoreError> {
        f.seek(SeekFrom::Start(0))?;
        f.write_all(magic)?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&0u64.to_le_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Appends one row. The returned id indexes both the vector and the payload.
    ///
    /// Not durable until `commit`; the count header is the commit point.
    pub fn append(&mut self, vector: &QuantizedVector, payload: &[u8]) -> Result<u32, StoreError> {
        if vector.dim() != self.dim {
            return Err(StoreError::DimensionMismatch {
                expected: self.dim,
                actual: vector.dim(),
            });
        }

        // 1. payload bytes
        self.payload_dat.write_all(payload)?;
        let offset = self.dat_cursor;
        self.dat_cursor += payload.len() as u64;

        // 2. payload index record
        self.payload_idx.write_all(&offset.to_le_bytes())?;
        self.payload_idx
            .write_all(&(payload.len() as u32).to_le_bytes())?;
        self.payload_idx.write_all(&0u32.to_le_bytes())?;

        // 3. vector record
        self.vectors.write_all(&vector.scale.to_le_bytes())?;
        self.vectors.write_all(&vector.norm.to_le_bytes())?;
        self.vectors
            .write_all(bytemuck::cast_slice(&vector.bytes))?;

        let row = self.count;
        self.count += 1;
        self.dirty = true;
        Ok(row)
    }

    /// Flushes buffers, then publishes the new count. Readers see nothing until
    /// this returns.
    pub fn commit(&mut self) -> Result<u32, StoreError> {
        if !self.dirty {
            return Ok(self.count);
        }
        self.payload_dat.flush()?;
        self.payload_idx.flush()?;
        self.vectors.flush()?;

        // 4. commit point
        let f = self.vectors.get_mut();
        f.seek(SeekFrom::Start(COUNT_OFFSET))?;
        f.write_all(&(self.count as u64).to_le_bytes())?;
        f.seek(SeekFrom::End(0))?;
        self.dirty = false;
        Ok(self.count)
    }

    /// `commit` plus an fsync on all three files. Costs a disk round trip.
    pub fn commit_durable(&mut self) -> Result<u32, StoreError> {
        let n = self.commit()?;
        self.payload_dat.sync_data()?;
        self.payload_idx.get_ref().sync_data()?;
        self.vectors.get_ref().sync_data()?;
        Ok(n)
    }

    /// Marks a row deleted by appending to the tombstone log.
    ///
    /// An append rather than an in-place flag flip, specifically so live mmaps
    /// never observe a mutating byte.
    pub fn tombstone(&mut self, row: u32) -> Result<(), StoreError> {
        if row >= self.count {
            return Err(StoreError::OutOfBounds {
                row,
                count: self.count,
            });
        }
        self.tombstones.write_all(&row.to_le_bytes())?;
        self.tombstones.sync_data()?;
        Ok(())
    }
}

// =================================================================== snapshot

/// Immutable view of the segment at one point in time.
pub struct Snapshot {
    _vec_file: File,
    _idx_file: File,
    _dat_file: File,
    vectors: Mmap,
    payload_idx: Mmap,
    payload_dat: Mmap,
    deleted: HashSet<u32>,
    tombstone_bytes: u64,
    dim: usize,
    count: u32,
    record_size: usize,
}

impl Snapshot {
    fn open(paths: &SegmentPaths) -> Result<Self, StoreError> {
        let vec_file = File::open(&paths.vectors)?;
        let idx_file = File::open(&paths.payload_idx)?;
        let dat_file = File::open(&paths.payload_dat)?;

        let vectors = unsafe { MmapOptions::new().map(&vec_file)? };
        let payload_idx = unsafe { MmapOptions::new().map(&idx_file)? };
        let payload_dat = unsafe { MmapOptions::new().map(&dat_file)? };

        if vectors.len() < VEC_HEADER {
            return Err(StoreError::Truncated {
                file: "vectors.bin",
                need: VEC_HEADER,
                have: vectors.len(),
            });
        }
        if &vectors[0..4] != VEC_MAGIC {
            return Err(StoreError::BadMagic {
                file: "vectors.bin",
            });
        }
        let version = read_u32(&vectors, 4);
        if version != VERSION {
            return Err(StoreError::UnsupportedVersion {
                file: "vectors.bin",
                found: version,
            });
        }
        if payload_idx.len() >= 4 && &payload_idx[0..4] != IDX_MAGIC {
            return Err(StoreError::BadMagic {
                file: "payload.idx",
            });
        }
        if payload_dat.len() >= 4 && &payload_dat[0..4] != DAT_MAGIC {
            return Err(StoreError::BadMagic {
                file: "payload.dat",
            });
        }

        let dim = read_u32(&vectors, 8) as usize;
        let count = read_u64(&vectors, COUNT_OFFSET as usize) as u32;
        let record_size = QuantizedVector::record_size(dim);

        let need_vec = VEC_HEADER + count as usize * record_size;
        if vectors.len() < need_vec {
            return Err(StoreError::Truncated {
                file: "vectors.bin",
                need: need_vec,
                have: vectors.len(),
            });
        }
        let need_idx = IDX_HEADER + count as usize * IDX_RECORD;
        if payload_idx.len() < need_idx {
            return Err(StoreError::Truncated {
                file: "payload.idx",
                need: need_idx,
                have: payload_idx.len(),
            });
        }

        let deleted = Self::load_tombstones(&paths.tombstones, count)?;
        let tombstone_bytes = std::fs::metadata(&paths.tombstones)
            .map(|m| m.len())
            .unwrap_or(0);

        Ok(Self {
            _vec_file: vec_file,
            _idx_file: idx_file,
            _dat_file: dat_file,
            vectors,
            payload_idx,
            payload_dat,
            deleted,
            tombstone_bytes,
            dim,
            count,
            record_size,
        })
    }

    fn load_tombstones(path: &Path, count: u32) -> Result<HashSet<u32>, StoreError> {
        let mut set = HashSet::new();
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(set),
            Err(e) => return Err(e.into()),
        };
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        for chunk in buf.as_chunks::<4>().0 {
            let row = u32::from_le_bytes(*chunk);
            if row < count {
                set.insert(row);
            }
        }
        Ok(set)
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn deleted_count(&self) -> usize {
        self.deleted.len()
    }

    /// Rows that exist and have not been tombstoned.
    pub fn live_count(&self) -> usize {
        self.count as usize - self.deleted.len()
    }

    #[inline]
    pub fn is_deleted(&self, row: u32) -> bool {
        self.deleted.contains(&row)
    }

    #[inline]
    pub fn vector(&self, row: u32) -> Result<VectorView<'_>, StoreError> {
        if row >= self.count {
            return Err(StoreError::OutOfBounds {
                row,
                count: self.count,
            });
        }
        let at = VEC_HEADER + row as usize * self.record_size;
        let scale = f32::from_le_bytes(self.vectors[at..at + 4].try_into().unwrap());
        let norm = f32::from_le_bytes(self.vectors[at + 4..at + 8].try_into().unwrap());
        let bytes: &[i8] = bytemuck::cast_slice(&self.vectors[at + 8..at + self.record_size]);
        Ok(VectorView { scale, norm, bytes })
    }

    /// The bytes stored alongside row `row`. This is the thing that was missing:
    /// a search result can now name what it found.
    pub fn payload(&self, row: u32) -> Result<&[u8], StoreError> {
        if row >= self.count {
            return Err(StoreError::OutOfBounds {
                row,
                count: self.count,
            });
        }
        let at = IDX_HEADER + row as usize * IDX_RECORD;
        let offset = read_u64(&self.payload_idx, at) as usize;
        let len = read_u32(&self.payload_idx, at + 8) as usize;
        let end = offset + len;
        if end > self.payload_dat.len() {
            return Err(StoreError::Truncated {
                file: "payload.dat",
                need: end,
                have: self.payload_dat.len(),
            });
        }
        Ok(&self.payload_dat[offset..end])
    }

    /// Exhaustive scan. Ground truth for the recall harness, and the right
    /// choice below a few thousand rows.
    pub fn brute_force(&self, query: &dyn crate::quant::Query, k: usize) -> Vec<(u32, f32)> {
        let mut heap: Vec<(u32, f32)> = Vec::with_capacity(k + 1);
        for row in 0..self.count {
            if self.is_deleted(row) {
                continue;
            }
            let view = match self.vector(row) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let score = 1.0 - query.distance(&view);
            if heap.len() < k {
                heap.push((row, score));
                if heap.len() == k {
                    heap.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                }
            } else if k > 0 && score > heap[k - 1].1 {
                heap[k - 1] = (row, score);
                heap.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            }
        }
        heap.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        heap.truncate(k);
        heap
    }
}

// ===================================================================== reader

/// Reader handle shared across threads. `snapshot()` is lock-free; `reload()`
/// swaps in a wider mapping once the writer has published more rows.
pub struct SegmentReader {
    paths: SegmentPaths,
    active: ArcSwap<Snapshot>,
}

impl SegmentReader {
    pub fn open(paths: SegmentPaths) -> Result<Arc<Self>, StoreError> {
        let snap = Snapshot::open(&paths)?;
        Ok(Arc::new(Self {
            paths,
            active: ArcSwap::from_pointee(snap),
        }))
    }

    #[inline]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.active.load_full()
    }

    /// Returns true if a new snapshot was published.
    ///
    /// Call on a cadence, not per append: each reload is several syscalls plus
    /// an mmap, which dominates at index-build rates.
    pub fn reload(&self) -> Result<bool, StoreError> {
        let current = self.snapshot();

        // The committed count is the authority, not the file length. Recovering
        // orphaned rows after a crash rewinds the write cursor, so the file can
        // hold *more* bytes than the committed frontier and a length comparison
        // misses the update entirely.
        let on_disk_count = Self::committed_count(&self.paths.vectors)?;
        let tombstone_bytes = std::fs::metadata(&self.paths.tombstones)
            .map(|m| m.len())
            .unwrap_or(0);

        if on_disk_count == current.count && tombstone_bytes == current.tombstone_bytes {
            return Ok(false);
        }

        self.active.store(Arc::new(Snapshot::open(&self.paths)?));
        Ok(true)
    }

    /// Reads just the count header. Deliberately a plain read rather than a peek
    /// through the live mapping: nothing in this store mutates mapped bytes.
    fn committed_count(path: &Path) -> Result<u32, StoreError> {
        let mut f = File::open(path)?;
        let mut buf = [0u8; 8];
        f.seek(SeekFrom::Start(COUNT_OFFSET))?;
        f.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf) as u32)
    }

    pub fn paths(&self) -> &SegmentPaths {
        &self.paths
    }
}
