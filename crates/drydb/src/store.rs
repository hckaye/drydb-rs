//! Page loading: directory lookup, positional read, filter decode, validation, caching.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::budget::{
    hash_table_bytes, hash_table_peak_bytes, Bookkeeping, Budget, Charge, BUFFER_OVERHEAD,
};
use crate::cache::PageCache;
use crate::directory::PageDirectory;
use crate::error::{Error, Result};
use crate::filter::PageFilter;
use crate::format::node::{NodeKind, PageView};
use crate::format::{read_i32, FileOffset, PageOrdinal, PAGE_LENGTH_LEN, PAGE_PREFIX_LEN};
use crate::io::{read_exact_at, PageSource};
use crate::metrics::Metrics;
use crate::page::{PageBuffer, PagePin};

/// How many eviction rounds a reservation attempts before giving up.
const EVICT_ROUNDS: usize = 16;

/// Free budget, in multiples of the page being retained, required before the store will
/// hold a page for the life of the database.
const RETAIN_HEADROOM_PAGES: u64 = 8;

/// One entry of the retained roots table.
const ROOT_ENTRY_BYTES: usize = std::mem::size_of::<(u64, PagePin)>();

/// Root pages the trees asked to keep, with what the table holding them costs.
#[derive(Default)]
struct RetainedRoots {
    roots: HashMap<u64, PagePin>,
    /// What the table could hold at the bucket count it has now.
    ///
    /// Tracked by hand because `HashMap::capacity` is what the table will still take
    /// before it grows, and that falls as entries are removed although the buckets stay
    /// exactly where they are. Charging from it would give back memory that is still
    /// held.
    slots: usize,
    /// Charged by what the table has allocated rather than per entry: a table holds its
    /// slots after the entries are gone, and this store can outlive the database that
    /// made it, through a reader still holding it. Per-entry charges would leave those
    /// slots accounted to nobody, with nothing left that could give them back.
    book: Bookkeeping,
}

impl RetainedRoots {
    fn footprint(&self) -> u64 {
        hash_table_bytes(self.slots, ROOT_ENTRY_BYTES)
    }
}

/// Hard limits that bound the work a single malformed page can cause.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest page accepted as stored on disk.
    pub max_stored_page_bytes: usize,
    /// Largest page accepted after page filters have decoded it.
    ///
    /// A filter that cannot say how large its output will be makes the reader reserve
    /// this much per page load, so on a filtered database it has to fit in the memory
    /// budget; opening one where it does not is rejected up front.
    pub max_decoded_page_bytes: usize,
    /// Largest header plus descriptor section accepted while opening.
    pub max_catalog_bytes: usize,
    /// Deepest root-to-leaf descent accepted before a tree is called cyclic.
    pub max_tree_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_stored_page_bytes: 64 << 20,
            max_decoded_page_bytes: 8 << 20,
            max_catalog_bytes: 8 << 20,
            max_tree_depth: 64,
        }
    }
}

/// The pieces a [`PageStore`] is assembled from.
pub(crate) struct PageStoreParts {
    pub source: Arc<dyn PageSource>,
    pub directory: PageDirectory,
    pub cache: PageCache,
    pub filter: Option<Arc<dyn PageFilter>>,
    pub budget: Arc<Budget>,
    pub limits: Limits,
    pub metrics: Arc<Metrics>,
    /// File offset of the page directory, which every page must sit before.
    pub directory_base: FileOffset,
    /// The store's own fixed memory, reserved by the caller before the parts were built.
    pub charge: Charge,
}

/// Everything needed to turn a page ordinal into bytes.
pub(crate) struct PageStore {
    source: Arc<dyn PageSource>,
    directory: PageDirectory,
    cache: PageCache,
    filter: Option<Arc<dyn PageFilter>>,
    budget: Arc<Budget>,
    limits: Limits,
    metrics: Arc<Metrics>,
    directory_base: u64,
    /// Root pages trees asked to keep, by tree.
    ///
    /// Held here rather than in each tree so that reclaiming can reach them: a tree that
    /// kept its own root would hold it past the point where a read needs the memory more,
    /// and no amount of evicting would get it back.
    retained_roots: Mutex<RetainedRoots>,
    /// The store's own fixed memory, chiefly the cache's shard array.
    ///
    /// Held here rather than on the `Database` because table and index handles keep the
    /// store alive on their own and can outlive the database they came from.
    _charge: Charge,
}

impl PageStore {
    pub(crate) fn new(parts: PageStoreParts) -> PageStore {
        PageStore {
            source: parts.source,
            directory: parts.directory,
            cache: parts.cache,
            filter: parts.filter,
            budget: parts.budget,
            limits: parts.limits,
            metrics: parts.metrics,
            directory_base: parts.directory_base.get(),
            retained_roots: Mutex::new(RetainedRoots::default()),
            _charge: parts.charge,
        }
    }

    pub(crate) fn cache(&self) -> &PageCache {
        &self.cache
    }

    /// The root page a tree asked to keep, if it is still kept.
    pub(crate) fn retained_root(&self, tree: u64) -> Option<PagePin> {
        self.retained_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .roots
            .get(&tree)
            .cloned()
    }

    /// Keeps a tree's root page for as long as the budget has room to spare.
    ///
    /// The caller has already checked that the page itself fits with room left over;
    /// what is reserved here is the table that holds it. Failing to reserve just means
    /// the page is not kept, which costs a cache lookup and nothing else.
    pub(crate) fn retain_root(&self, tree: u64, pin: PagePin) {
        let mut held = self
            .retained_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Reserved before the entry goes in, at the most the table will hold while it
        // grows: the old allocation and the new one are both live until the move is
        // done.
        let want = hash_table_peak_bytes(
            held.slots,
            held.roots.capacity(),
            held.roots.len(),
            ROOT_ENTRY_BYTES,
        );
        if held.book.settle(want, &self.budget).is_err() {
            return;
        }
        held.roots.insert(tree, pin);
        held.slots = held.slots.max(held.roots.capacity());
        // Back down to what it actually grew to, which is at most what was asked for.
        let settled = held.footprint();
        let _ = held.book.settle(settled, &self.budget);
    }

    /// Lets go of one tree's retained root, when the tree itself goes away.
    pub(crate) fn release_root(&self, tree: u64) {
        let mut held = self
            .retained_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = held.roots.remove(&tree);
        // The table keeps its slots after an entry goes, so give them back once half of
        // them are idle. Without this a store outliving its database would hold a table
        // sized for every tree it ever had. Shrinking allocates the smaller table before
        // it lets the bigger one go, so both are reserved while the entries move, and
        // the shrink is skipped when that does not fit.
        if held.slots > 2 * held.roots.len() {
            let during = held.footprint() + hash_table_bytes(held.roots.len(), ROOT_ENTRY_BYTES);
            if held.book.settle(during, &self.budget).is_ok() {
                let before = held.roots.capacity();
                held.roots.shrink_to_fit();
                // A table that was not rebuilt still has the buckets it had, whatever
                // its capacity reads now.
                if held.roots.capacity() != before {
                    held.slots = held.roots.capacity();
                }
            }
        }
        let want = held.footprint();
        let _ = held.book.settle(want, &self.budget);
        drop(held);
        drop(entry);
    }

    /// Lets go of every retained root. Returns how many bytes of page they held.
    ///
    /// Counted the way the cache counts an eviction: the bytes of what was let go, which
    /// return to the budget once nothing else is still reading the page.
    fn release_roots(&self) -> u64 {
        // Taken out from under the lock so the pages are freed after it is released:
        // dropping the last pin gives its bytes back to the budget. The table goes with
        // them, and its charge with it.
        let held = std::mem::take(
            &mut *self
                .retained_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        held.roots.values().map(|pin| pin.len() as u64).sum()
    }

    /// Frees memory held on this budget for a caller that needs it more, most
    /// expendable first. Returns how many bytes were let go.
    ///
    /// The page directory reclaims through here as well as the page store: the two draw
    /// on one budget, so a directory chunk that does not fit has to be able to take back
    /// what the pages are holding, including a root kept for a tree.
    fn reclaim_pages(&self, bytes: u64) -> u64 {
        let freed = self.cache.evict_bytes(bytes);
        if freed > 0 {
            return freed;
        }
        // Roots last: they are the pages every search starts from, so they go only when
        // there is nothing else to give.
        self.release_roots()
    }

    pub(crate) fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }

    /// Whether the budget has room to hold `bytes` indefinitely without squeezing the
    /// pages a search needs.
    ///
    /// Used before retaining anything for the life of the database, so that a tight
    /// budget is never permanently reduced by bookkeeping.
    pub(crate) fn has_headroom_for(&self, bytes: u64) -> bool {
        let reserve = bytes.saturating_mul(RETAIN_HEADROOM_PAGES);
        self.budget.limit().saturating_sub(self.budget.in_use()) >= reserve
    }

    pub(crate) fn limits(&self) -> &Limits {
        &self.limits
    }

    pub(crate) fn page_count(&self) -> u64 {
        self.directory.page_count()
    }

    /// Returns the page, loading it if the cache does not have it.
    pub(crate) fn page(&self, ordinal: PageOrdinal) -> Result<PagePin> {
        self.cache
            .get_or_load(
                ordinal.get(),
                // Reclaims from the page directory too, which the cache cannot reach.
                |bytes| self.reserve(bytes),
                || self.load(ordinal),
            )
            .map_err(|e| annotate(e, ordinal))
    }

    /// Where a page has to end: at the page after it, or at the page directory.
    ///
    /// Checking only the directory lets a page's declared length run into the page that
    /// follows it, and the bytes of that page come back as part of this one's. Pages are
    /// written in ordinal order, so the next ordinal's offset is the boundary; a file
    /// that does not order them that way falls back to the directory, which is still an
    /// upper bound.
    pub(crate) fn page_limit(&self, ordinal: PageOrdinal, offset: u64) -> Result<u64> {
        let next = ordinal.get().saturating_add(1);
        if next >= self.page_count() {
            return Ok(self.directory_base);
        }
        let next_offset = self.offset_of(PageOrdinal::new(next))?.get();
        Ok(if next_offset > offset {
            next_offset.min(self.directory_base)
        } else {
            self.directory_base
        })
    }

    /// Returns the page only if it is already cached. Never performs I/O.
    pub(crate) fn page_cached(&self, ordinal: PageOrdinal) -> Option<PagePin> {
        self.cache.get(ordinal.get())
    }

    /// Reserves budget, evicting cached pages when the first attempt does not fit.
    /// Reserves `bytes`, reclaiming from the cache and the page directory if it has to.
    ///
    /// Anything a query holds for a while goes through here, not only page buffers: a
    /// reservation that refuses to reclaim would make a query fail or succeed depending
    /// on what the cache happened to be holding.
    pub(crate) fn reserve(&self, bytes: u64) -> Result<Charge> {
        match self.budget.try_reserve(bytes) {
            Ok(charge) => return Ok(charge),
            Err(e) if !e.is_retryable() => return Err(e),
            Err(_) => {}
        }
        // Pages and page directory chunks share one budget, so reclaim from both: a
        // page that does not fit because the directory is holding the memory has to be
        // able to take it back, and the other way round.
        let mut last = None;
        // Keep going while something is still being given back. The directory releases
        // one chunk at a time, so a fixed number of rounds gives up with memory still
        // reclaimable and hands the caller a failure that a retry would have fixed. The
        // bound is what there is to reclaim, so a budget that cannot be satisfied still
        // ends rather than spins.
        let mut rounds = self
            .cache
            .len()
            .saturating_add(self.directory.chunks_held())
            .saturating_add(EVICT_ROUNDS);
        while rounds > 0 {
            rounds -= 1;
            let freed = self.cache.evict_bytes(bytes);
            let dropped_chunk = freed == 0 && self.directory.evict_one();
            // Roots last: they are the pages every search starts from, so they are worth
            // keeping until there is nothing else to give.
            let released = if freed == 0 && !dropped_chunk {
                self.release_roots()
            } else {
                0
            };
            match self.budget.try_reserve(bytes) {
                Ok(charge) => return Ok(charge),
                Err(e) => {
                    last = Some(e);
                    if freed == 0 && !dropped_chunk && released == 0 {
                        break;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::corrupt("budget reservation failed without reporting a reason")
        }))
    }

    fn load(&self, ordinal: PageOrdinal) -> Result<(PagePin, u64)> {
        let offset = self.offset_of(ordinal)?;

        let mut length_bytes = [0u8; PAGE_LENGTH_LEN];
        read_exact_at(self.source.as_ref(), &mut length_bytes, offset.get())?;
        Metrics::bump(&self.metrics.bytes_read, PAGE_LENGTH_LEN as u64);

        let stored_len = crate::format::checked_len(read_i32(&length_bytes, 0)?, "page length")?;
        if stored_len < PAGE_PREFIX_LEN {
            return Err(Error::corrupt(format!(
                "page length {stored_len} is smaller than the {PAGE_PREFIX_LEN} byte prefix"
            )));
        }
        if stored_len > self.limits.max_stored_page_bytes {
            return Err(Error::corrupt(format!(
                "page length {stored_len} exceeds the {} byte limit",
                self.limits.max_stored_page_bytes
            )));
        }
        let end = offset
            .get()
            .checked_add(stored_len as u64)
            .ok_or_else(|| Error::corrupt("page end offset overflows"))?;
        if end > self.page_limit(ordinal, offset.get())? {
            return Err(Error::corrupt(format!(
                "page spans {}..{end}, into what follows it",
                offset.get()
            )));
        }

        let stored_charge = self.reserve(stored_len as u64 + BUFFER_OVERHEAD)?;
        let mut stored = vec![0u8; stored_len];
        read_exact_at(self.source.as_ref(), &mut stored, offset.get())?;
        Metrics::bump(&self.metrics.page_reads, 1);
        Metrics::bump(&self.metrics.bytes_read, stored_len as u64);

        // The reservation is named first so that it is dropped last: Rust drops these
        // in reverse, and a reservation released while the buffer it stands for is
        // still there leaves the budget reporting room that is not free. The paths
        // below this can fail with the buffer in hand.
        let (charge, data) = match &self.filter {
            None => (stored_charge, stored.into_boxed_slice()),
            Some(filter) => {
                let limit = self.limits.max_decoded_page_bytes;
                let encoded = &stored[PAGE_PREFIX_LEN..];
                // Reserve what the encoded form says it needs, and only fall back to
                // the whole limit when it does not say.
                let capacity = match filter.decoded_size_hint(encoded) {
                    Some(size) if size <= limit => size,
                    Some(size) => {
                        return Err(Error::corrupt(format!(
                            "page filter `{}` reports a decoded size of {size} bytes, above \
                             the {limit} byte limit",
                            filter.id()
                        )))
                    }
                    None => limit,
                };
                let scratch_charge = self.reserve(capacity as u64 + BUFFER_OVERHEAD)?;
                let mut payload = Vec::with_capacity(capacity);
                let reserved = payload.capacity();
                // What the filter needs for itself, held only while it decodes. zstd
                // allocates its context through C's allocator, where nothing on the Rust
                // side can see it, so the filter is asked instead. Asking can allocate
                // in its own right, so the question is reserved for as long as it takes
                // to answer, the same way the open path reserves it.
                let working = {
                    let _probe = self.reserve(filter.probe_bytes())?;
                    filter.working_set_bytes()
                };
                let working_charge = self.reserve(working)?;
                let decoded = filter.decode(encoded, &mut payload);
                drop(working_charge);
                decoded?;
                if payload.capacity() > reserved {
                    return Err(Error::corrupt(format!(
                        "page filter `{}` grew its output buffer past the {reserved} bytes it \
                         was given",
                        filter.id()
                    )));
                }
                if payload.len() > limit {
                    return Err(Error::corrupt(format!(
                        "page filter `{}` produced {} bytes, above the {limit} byte limit",
                        filter.id(),
                        payload.len()
                    )));
                }
                Metrics::bump(&self.metrics.decompressed_bytes, payload.len() as u64);

                let total = PAGE_PREFIX_LEN
                    .checked_add(payload.len())
                    .ok_or_else(|| Error::corrupt("decoded page size overflows"))?;
                let exact_charge = self.reserve(total as u64 + BUFFER_OVERHEAD)?;
                let mut page = Vec::with_capacity(total);
                page.extend_from_slice(&stored[..PAGE_PREFIX_LEN]);
                page.extend_from_slice(&payload);
                // The stored length field describes the compressed page; rewrite it to
                // the decoded length, exactly as the upstream reader does.
                page[0..4].copy_from_slice(&(total as i32).to_le_bytes());

                let boxed = page.into_boxed_slice();
                // Each buffer goes before the reservation that stood for it, not after:
                // between the two, the budget would say the memory is free while it is
                // still held, and another thread reserving in that window would take it
                // and put the two of them together over the limit.
                drop(payload);
                drop(scratch_charge);
                drop(stored);
                drop(stored_charge);
                (exact_charge, boxed)
            }
        };

        let bytes = charge.bytes();
        {
            let view = PageView::parse(&data).map_err(|e| e.at_page(ordinal.get()))?;
            view.validate().map_err(|e| e.at_page(ordinal.get()))?;
        }
        Ok((
            PagePin::new(Arc::new(PageBuffer::new(ordinal, data, charge))),
            bytes,
        ))
    }

    /// Resolves a blob page into the pin plus the byte range holding the value.
    pub(crate) fn blob(&self, ordinal: PageOrdinal) -> Result<(PagePin, std::ops::Range<usize>)> {
        self.blob_from_pin(self.page(ordinal)?)
    }

    /// The same, for a page the caller already holds.
    ///
    /// Callers that found the page in the cache use this rather than asking for it
    /// again: between the two calls the page can be evicted, and asking again would turn
    /// a lookup that promised not to perform I/O into one that does.
    pub(crate) fn blob_from_pin(&self, pin: PagePin) -> Result<(PagePin, std::ops::Range<usize>)> {
        let ordinal = pin.ordinal();
        let view = pin.view()?;
        if view.kind() != NodeKind::Leaf || view.entry_count() != 0 {
            return Err(Error::corrupt(format!(
                "page {ordinal} is referenced as a blob page but holds {} entries",
                view.entry_count()
            ))
            .at_page(ordinal.get()));
        }
        let len = pin.len();
        Ok((pin, PAGE_PREFIX_LEN..len))
    }

    /// The byte source, for streaming readers that bypass the cache.
    pub(crate) fn source(&self) -> &Arc<dyn PageSource> {
        &self.source
    }

    /// File offset of a page, for streaming readers.
    pub(crate) fn offset_of(&self, ordinal: PageOrdinal) -> Result<FileOffset> {
        self.directory
            .offset_of(ordinal, &|bytes| self.reclaim_pages(bytes))
    }

    /// Whether page filters are configured for this file.
    pub(crate) fn has_filter(&self) -> bool {
        self.filter.is_some()
    }
}

fn annotate(e: Error, ordinal: PageOrdinal) -> Error {
    if e.location().page.is_some() {
        e
    } else {
        e.at_page(ordinal.get())
    }
}
