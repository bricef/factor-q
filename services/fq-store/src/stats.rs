//! Aggregate statistics for a content store — the basis for CAS metrics.

/// A snapshot of a store's contents. Derived ratios (deduplication, sharing)
/// are computed on demand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Distinct objects (logical files) stored.
    pub objects: u64,
    /// Distinct blocks (physical deduplication units) stored.
    pub blocks: u64,
    /// Total content size across all objects — what callers stored, counting
    /// shared content once per object.
    pub logical_bytes: u64,
    /// Bytes actually stored for content (sum of distinct block sizes).
    pub physical_bytes: u64,
    /// Total block references across all objects' manifests.
    pub block_refs: u64,
}

impl Stats {
    /// `logical_bytes / physical_bytes` — how much deduplication shrank
    /// storage (>= 1.0; higher means more sharing). `1.0` when empty.
    pub fn dedup_ratio(&self) -> f64 {
        if self.physical_bytes == 0 {
            1.0
        } else {
            self.logical_bytes as f64 / self.physical_bytes as f64
        }
    }

    /// Fraction of logical bytes saved by deduplication (`0.0..=1.0`).
    pub fn dedup_savings(&self) -> f64 {
        if self.logical_bytes == 0 {
            0.0
        } else {
            1.0 - (self.physical_bytes as f64 / self.logical_bytes as f64)
        }
    }

    /// Average object references per block (`block_refs / blocks`) — how
    /// reused blocks are. `0.0` when empty.
    pub fn avg_block_sharing(&self) -> f64 {
        if self.blocks == 0 {
            0.0
        } else {
            self.block_refs as f64 / self.blocks as f64
        }
    }
}
