//! Counters for the page path.
//!
//! Cheap relaxed atomics; they are diagnostics, not a consistent snapshot.

use std::sync::atomic::{AtomicU64, Ordering};

/// Live counters owned by a [`Database`](crate::Database).
#[derive(Debug, Default)]
pub struct Metrics {
    pub(crate) cache_hits: AtomicU64,
    pub(crate) cache_misses: AtomicU64,
    pub(crate) coalesced_loads: AtomicU64,
    pub(crate) page_reads: AtomicU64,
    pub(crate) bytes_read: AtomicU64,
    pub(crate) evictions: AtomicU64,
    pub(crate) directory_chunk_reads: AtomicU64,
    pub(crate) decompressed_bytes: AtomicU64,
}

/// Page path counters, as returned by
/// [`Database::metrics`](crate::Database::metrics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Page lookups served from the cache.
    pub cache_hits: u64,
    /// Page lookups that had to load.
    pub cache_misses: u64,
    /// Lookups that waited on another thread's in-flight load of the same page.
    pub coalesced_loads: u64,
    /// Pages actually read from the source.
    pub page_reads: u64,
    /// Bytes read from the source, including page length probes.
    pub bytes_read: u64,
    /// Pages dropped from the cache by the eviction policy.
    pub evictions: u64,
    /// Page directory chunks read.
    pub directory_chunk_reads: u64,
    /// Bytes produced by page filters while decoding.
    pub decompressed_bytes: u64,
}

impl Metrics {
    pub(crate) fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            coalesced_loads: self.coalesced_loads.load(Ordering::Relaxed),
            page_reads: self.page_reads.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            directory_chunk_reads: self.directory_chunk_reads.load(Ordering::Relaxed),
            decompressed_bytes: self.decompressed_bytes.load(Ordering::Relaxed),
        }
    }

    #[inline]
    pub(crate) fn bump(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

/// Memory accounting reported by [`Database::memory_report`](crate::Database::memory_report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReport {
    /// Configured budget in bytes.
    pub budget_limit: u64,
    /// Bytes charged right now: live page buffers, directory chunks and scratch.
    pub charged: u64,
    /// Highest charge seen so far.
    pub peak_charged: u64,
    /// Bytes the page cache is currently retaining.
    pub cache_resident: u64,
    /// Soft limit the cache evicts against.
    pub cache_capacity: u64,
}
