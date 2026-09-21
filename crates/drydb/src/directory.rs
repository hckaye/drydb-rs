//! The page directory: page ordinal to file offset.
//!
//! The section is `page_count` little-endian `i64` offsets at the end of the file. A
//! database with a hundred million pages has an 800 MB directory, so this reads it in
//! fixed-size chunks and keeps a bounded number of them; nothing here grows with the
//! page count. The dense-array shortcut is just the degenerate case where the whole
//! directory is one chunk.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::budget::{Budget, Charge, BUFFER_OVERHEAD};
use crate::error::{Error, Result};
use crate::format::{read_i64, FileOffset, PageOrdinal, DIRECTORY_SLOT_LEN, HEADER_LEN};
use crate::io::{read_exact_at, PageSource};
use crate::metrics::Metrics;

struct CachedChunk {
    index: u64,
    data: Box<[u8]>,
    last_used: u64,
    _charge: Charge,
}

/// Bounded reader over the page directory section.
pub(crate) struct PageDirectory {
    source: Arc<dyn PageSource>,
    base: u64,
    count: u64,
    entries_per_chunk: u64,
    max_chunks: usize,
    budget: Arc<Budget>,
    metrics: Arc<Metrics>,
    chunks: Mutex<Vec<CachedChunk>>,
    tick: AtomicU64,
}

impl PageDirectory {
    /// Creates a directory reader.
    ///
    /// `base` is the directory's file offset and `count` its slot count, both from the
    /// validated header.
    pub(crate) fn new(
        source: Arc<dyn PageSource>,
        base: FileOffset,
        count: u64,
        entries_per_chunk: u64,
        max_chunks: usize,
        budget: Arc<Budget>,
        metrics: Arc<Metrics>,
    ) -> PageDirectory {
        PageDirectory {
            source,
            base: base.get(),
            count,
            entries_per_chunk: entries_per_chunk.max(1),
            max_chunks: max_chunks.max(1),
            budget,
            metrics,
            chunks: Mutex::new(Vec::new()),
            tick: AtomicU64::new(0),
        }
    }

    /// Number of pages in the file.
    pub(crate) fn page_count(&self) -> u64 {
        self.count
    }

    /// Resolves an ordinal to its file offset.
    ///
    /// `reclaim` releases memory held elsewhere on the same budget, and returns how many
    /// bytes it freed. Directory chunks and page buffers draw on one budget, so a chunk
    /// that does not fit has to be able to take memory back from the pages, exactly as a
    /// page can take it from the chunks.
    pub(crate) fn offset_of(
        &self,
        ordinal: PageOrdinal,
        reclaim: &dyn Fn(u64) -> u64,
    ) -> Result<FileOffset> {
        let index = ordinal.get();
        if index >= self.count {
            return Err(Error::corrupt(format!(
                "page ordinal {index} is past the {} pages in this file",
                self.count
            )));
        }
        let chunk_index = index / self.entries_per_chunk;
        let slot = (index % self.entries_per_chunk) as usize * DIRECTORY_SLOT_LEN;

        if let Some(raw) = self.lookup(chunk_index, slot)? {
            return self.validate_offset(raw, ordinal);
        }

        let chunk = self.read_chunk(chunk_index, reclaim)?;
        let raw = read_i64(&chunk.data, slot)?;
        self.insert(chunk);
        self.validate_offset(raw, ordinal)
    }

    /// Returns the raw slot value if its chunk is cached.
    fn lookup(&self, chunk_index: u64, slot: usize) -> Result<Option<i64>> {
        let mut chunks = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        for chunk in chunks.iter_mut() {
            if chunk.index == chunk_index {
                chunk.last_used = tick;
                return read_i64(&chunk.data, slot).map(Some);
            }
        }
        Ok(None)
    }

    fn validate_offset(&self, raw: i64, ordinal: PageOrdinal) -> Result<FileOffset> {
        let offset = FileOffset::from_i64(raw).map_err(|e| e.at_page(ordinal.get()))?;
        if offset.get() < HEADER_LEN as u64 || offset.get() >= self.base {
            return Err(Error::corrupt(format!(
                "page directory entry points at offset {}, outside the page area [{}, {})",
                offset.get(),
                HEADER_LEN,
                self.base
            ))
            .at_page(ordinal.get()));
        }
        Ok(offset)
    }

    fn read_chunk(&self, chunk_index: u64, reclaim: &dyn Fn(u64) -> u64) -> Result<CachedChunk> {
        let first = chunk_index
            .checked_mul(self.entries_per_chunk)
            .ok_or_else(|| Error::corrupt("page directory chunk index overflows"))?;
        let remaining = self.count - first;
        let entries = remaining.min(self.entries_per_chunk);
        let len = (entries as usize)
            .checked_mul(DIRECTORY_SLOT_LEN)
            .ok_or_else(|| Error::corrupt("page directory chunk size overflows"))?;
        let offset = self
            .base
            .checked_add(first * DIRECTORY_SLOT_LEN as u64)
            .ok_or_else(|| Error::corrupt("page directory offset overflows"))?;

        // Give up resident chunks until the new one fits. Reserving without evicting
        // first would fail on a tight budget even though the memory about to be freed is
        // exactly what the new chunk needs, and one eviction is not always enough: the
        // last chunk of the directory is shorter than the rest.
        let wanted = len as u64 + BUFFER_OVERHEAD;
        let charge = loop {
            match self.budget.try_reserve(wanted) {
                Ok(charge) => break charge,
                Err(e) => {
                    if !self.evict_one() && reclaim(wanted) == 0 {
                        return Err(e);
                    }
                }
            }
        };
        let mut data = vec![0u8; len].into_boxed_slice();
        read_exact_at(self.source.as_ref(), &mut data, offset)?;
        Metrics::bump(&self.metrics.directory_chunk_reads, 1);
        Metrics::bump(&self.metrics.bytes_read, len as u64);

        Ok(CachedChunk {
            index: chunk_index,
            data,
            last_used: self.tick.fetch_add(1, Ordering::Relaxed),
            _charge: charge,
        })
    }

    /// Drops the least recently used chunk, releasing its charge. Returns whether one
    /// was dropped.
    ///
    /// Called both from here, when a new chunk does not fit, and from the page store,
    /// when a page does not fit: the two caches draw on one budget, so either has to be
    /// able to reclaim from the other.
    pub(crate) fn evict_one(&self) -> bool {
        let mut chunks = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        match chunks
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| c.last_used)
            .map(|(i, _)| i)
        {
            Some(pos) => {
                chunks.swap_remove(pos);
                // The vector does not shrink on its own, so without this the room a
                // chunk took stays allocated while its reservation is gone.
                if chunks.capacity() > 2 * chunks.len() {
                    chunks.shrink_to_fit();
                }
                true
            }
            None => false,
        }
    }

    /// How many chunks are resident, for a caller deciding how long to keep reclaiming.
    pub(crate) fn chunks_held(&self) -> usize {
        self.chunks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn insert(&self, chunk: CachedChunk) {
        let mut chunks = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        if chunks.iter().any(|c| c.index == chunk.index) {
            // Another thread read the same chunk first; keep theirs.
            return;
        }
        if chunks.len() >= self.max_chunks {
            if let Some(pos) = chunks
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(i, _)| i)
            {
                chunks.swap_remove(pos);
            }
        }
        chunks.push(chunk);
    }

    /// Chunks currently held, for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn cached_chunks(&self) -> usize {
        self.chunks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::MemorySource;

    fn directory_image(offsets: &[i64], base: usize) -> Vec<u8> {
        let mut image = vec![0u8; base];
        for o in offsets {
            image.extend_from_slice(&o.to_le_bytes());
        }
        image
    }

    fn no_reclaim(_bytes: u64) -> u64 {
        0
    }

    fn directory(offsets: &[i64], entries_per_chunk: u64, max_chunks: usize) -> PageDirectory {
        let base = 4096usize;
        let image = directory_image(offsets, base);
        let source: Arc<dyn PageSource> = Arc::new(MemorySource::new(image));
        PageDirectory::new(
            source,
            FileOffset::new(base as u64),
            offsets.len() as u64,
            entries_per_chunk,
            max_chunks,
            Budget::new(1 << 20),
            Arc::new(Metrics::default()),
        )
    }

    #[test]
    fn resolves_every_ordinal() {
        let offsets: Vec<i64> = (0..100).map(|i| 26 + i * 40).collect();
        let dir = directory(&offsets, 8, 4);
        for (i, expected) in offsets.iter().enumerate() {
            assert_eq!(
                dir.offset_of(PageOrdinal::new(i as u64), &no_reclaim)
                    .unwrap()
                    .get(),
                *expected as u64
            );
        }
    }

    #[test]
    fn keeps_only_a_bounded_number_of_chunks() {
        let offsets: Vec<i64> = (0..1000).map(|i| 26 + i * 4).collect();
        let dir = directory(&offsets, 8, 4);
        for i in 0..1000u64 {
            dir.offset_of(PageOrdinal::new(i), &no_reclaim).unwrap();
        }
        assert!(
            dir.cached_chunks() <= 4,
            "cached {} chunks",
            dir.cached_chunks()
        );
    }

    #[test]
    fn rejects_out_of_range_ordinals() {
        let dir = directory(&[26, 60], 8, 4);
        assert!(dir.offset_of(PageOrdinal::new(2), &no_reclaim).is_err());
    }

    #[test]
    fn rejects_offsets_outside_the_page_area() {
        let dir = directory(&[3, 60], 8, 4);
        assert!(
            dir.offset_of(PageOrdinal::new(0), &no_reclaim).is_err(),
            "offset inside the header"
        );
        let dir = directory(&[26, 999_999], 8, 4);
        assert!(
            dir.offset_of(PageOrdinal::new(1), &no_reclaim).is_err(),
            "offset past the directory"
        );
        let dir = directory(&[-5, 60], 8, 4);
        assert!(
            dir.offset_of(PageOrdinal::new(0), &no_reclaim).is_err(),
            "negative offset"
        );
    }
}
