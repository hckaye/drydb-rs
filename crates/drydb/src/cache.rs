//! Bounded page cache with de-duplicated loads.
//!
//! Sharded `Mutex<HashMap>` plus a CLOCK eviction ring. Two rules shape it:
//!
//! * No I/O, decompression or user code runs while a shard lock is held. A miss marks
//!   the slot as in flight, releases the lock, loads, then re-takes the lock to publish.
//! * Concurrent misses on the same page collapse into one load. Waiters block on the
//!   shard's condition variable and are woken whether the load succeeded or failed.
//!
//! Failures are not cached: a waiter woken by a failed load retries the load itself.
//! That costs at most one redundant read per waiting thread on a persistently bad page,
//! and keeps a transient I/O error from being remembered as a permanent one.
//!
//! Eviction only drops the cache's own reference. A page a [`ValueGuard`] still holds
//! stays alive and stays charged to the budget until that guard goes away.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::budget::{hash_table_bytes, hash_table_peak_bytes, Bookkeeping, Budget, Charge};
use crate::error::{Error, Result};
use crate::metrics::Metrics;
use crate::page::PagePin;

enum Slot {
    Ready {
        pin: PagePin,
        bytes: u64,
        referenced: bool,
    },
    Loading,
}

#[derive(Default)]
struct ShardInner {
    map: HashMap<u64, Slot>,
    clock: VecDeque<u64>,
    resident: u64,
    /// What the table could hold at the bucket count it has now.
    ///
    /// Tracked by hand because `HashMap::capacity` is what the table will still take
    /// before it grows, and that falls as entries are removed although the buckets stay
    /// exactly where they are. Charging from it would give back memory that is still
    /// held, so the largest it has read since the table was last resized stands for the
    /// buckets instead.
    map_slots: usize,
    /// How many ring pushes loads in flight are still to make.
    ///
    /// The push happens with the shard locked, where there is no reserving, so the room
    /// for it is held from the time the marker goes in until the page is published. It
    /// is counted rather than measured: what the ring will cost depends on how many
    /// pushes are coming, not on what each of them thought when it started, and by the
    /// time the last of them publishes the ring has grown under all the others.
    pending_pushes: usize,
    /// What this shard's map and ring cost, charged by what they have allocated.
    ///
    /// Not folded into the per-page allowance: a shard holding one page still pays for
    /// a whole table and a whole ring, so a constant per page is short by most of it
    /// exactly when a cache is spread over many shards.
    book: Bookkeeping,
}

#[derive(Default)]
struct Shard {
    inner: Mutex<ShardInner>,
    ready: Condvar,
}

impl Shard {
    fn lock(&self) -> MutexGuard<'_, ShardInner> {
        // The critical sections here never leave the shard half-updated, so a poisoned
        // lock (a panic elsewhere in the process while holding it) still hands back
        // consistent state.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Upper bound on shards, so the bookkeeping a caller can ask for stays bounded too.
const MAX_SHARDS: usize = 1024;

/// What one in-flight load's marker costs: a map entry and its share of the table.
pub(crate) const LOADING_SLOT_BYTES: u64 = 128;

/// One entry of a shard's table.
const SLOT_BYTES: usize = std::mem::size_of::<(u64, Slot)>();

/// The smallest a ring is after it has grown at all.
///
/// A growing buffer asks for at least this many slots however few it had, so a ring of
/// one does not grow to two.
const RING_MIN_CAPACITY: usize = 4;

/// How many times a miss asks again when another thread grew the shard in between.
const RESERVE_ATTEMPTS: u32 = 8;

/// A bounded, sharded page cache.
pub(crate) struct PageCache {
    shards: Box<[Shard]>,
    capacity_per_shard: u64,
    capacity: u64,
    metrics: Arc<Metrics>,
    next_victim_shard: AtomicUsize,
    /// Where the shards' own tables are charged.
    budget: Arc<Budget>,
}

impl PageCache {
    /// Shards a request for `requested` actually produces.
    ///
    /// Separate from [`PageCache::new`] so a caller can reserve the memory the shard
    /// array needs before the array exists.
    pub(crate) fn shard_count(requested: usize) -> usize {
        requested.clamp(1, MAX_SHARDS).next_power_of_two()
    }

    /// What the shard array costs before a single page is cached.
    ///
    /// `cache_shards` is a caller's number and the array is allocated once per database,
    /// so without this a large shard count spends memory the budget never hears about.
    /// The shards' maps and rings are not counted here: they are charged as they grow,
    /// by what they have actually allocated.
    pub(crate) fn bookkeeping_bytes(shards: usize) -> u64 {
        (shards as u64)
            .saturating_mul(std::mem::size_of::<Shard>() as u64)
            .saturating_add(crate::budget::BUFFER_OVERHEAD)
    }

    /// Creates a cache retaining at most `capacity` bytes, spread over `shards`.
    ///
    /// `shards` is taken as given: it comes from [`PageCache::shard_count`], which the
    /// caller already used to reserve [`PageCache::bookkeeping_bytes`].
    pub(crate) fn new(
        capacity: u64,
        shards: usize,
        metrics: Arc<Metrics>,
        budget: Arc<Budget>,
    ) -> PageCache {
        let shards = PageCache::shard_count(shards);
        let capacity_per_shard = (capacity / shards as u64).max(1);
        PageCache {
            shards: (0..shards)
                .map(|_| Shard::default())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            capacity_per_shard,
            capacity,
            metrics,
            next_victim_shard: AtomicUsize::new(0),
            budget,
        }
    }

    fn shard_of(&self, ordinal: u64) -> &Shard {
        // Ordinals are dense, so the low bits alone would put every page of a leaf run
        // in the same shard; mix first.
        let mixed = ordinal.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
        &self.shards[(mixed as usize) & (self.shards.len() - 1)]
    }

    /// Soft capacity the cache evicts against.
    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes currently retained by the cache.
    pub(crate) fn resident_bytes(&self) -> u64 {
        self.shards.iter().map(|s| s.lock().resident).sum()
    }

    /// Number of pages currently retained.
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                s.lock()
                    .map
                    .values()
                    .filter(|v| matches!(v, Slot::Ready { .. }))
                    .count()
            })
            .sum()
    }

    /// Cache-only lookup. Marks the page as recently used on a hit.
    pub(crate) fn get(&self, ordinal: u64) -> Option<PagePin> {
        let shard = self.shard_of(ordinal);
        let mut inner = shard.lock();
        match inner.map.get_mut(&ordinal) {
            Some(Slot::Ready {
                pin, referenced, ..
            }) => {
                *referenced = true;
                let pin = pin.clone();
                Metrics::bump(&self.metrics.cache_hits, 1);
                Some(pin)
            }
            _ => None,
        }
    }

    /// Returns the cached page, or runs `load` exactly once for concurrent callers.
    /// `reserve` is asked for the marker's memory and for whatever the shard's table and
    /// ring have to grow by to hold it, and only on the miss path. It comes from the
    /// caller because reclaiming has to be able to reach the page directory as well as
    /// this cache, and only the page store knows about both.
    pub(crate) fn get_or_load<R, F>(&self, ordinal: u64, reserve: R, load: F) -> Result<PagePin>
    where
        R: Fn(u64) -> Result<Charge>,
        F: FnOnce() -> Result<(PagePin, u64)>,
    {
        let shard = self.shard_of(ordinal);
        // Taken on the miss path only, and before the marker goes in: an in-flight load
        // is an entry in the shard's map, one per concurrent caller, and nothing else
        // stands for it until the page itself arrives.
        let mut slot_charge: Option<Charge> = None;
        // Bounds the retry below, which only runs when another thread grew this shard
        // between the reservation and the insert.
        let mut attempts = 0u32;
        let mut inner = shard.lock();
        loop {
            match inner.map.get_mut(&ordinal) {
                Some(Slot::Ready {
                    pin, referenced, ..
                }) => {
                    *referenced = true;
                    let pin = pin.clone();
                    Metrics::bump(&self.metrics.cache_hits, 1);
                    return Ok(pin);
                }
                Some(Slot::Loading) => {
                    Metrics::bump(&self.metrics.coalesced_loads, 1);
                    inner = shard.ready.wait(inner).unwrap_or_else(|e| e.into_inner());
                }
                None if slot_charge.is_none() => {
                    // What the table and the ring will cost if this entry makes them
                    // grow, asked for before the entry goes in. Reserving can evict,
                    // which takes shard locks, so it happens with this one released and
                    // the map is looked at again afterwards.
                    let want = LOADING_SLOT_BYTES + growth_for_one_more(&inner);
                    drop(inner);
                    slot_charge = Some(reserve(want)?);
                    inner = shard.lock();
                }
                None => {
                    // The shard may have grown under another thread while this one had
                    // the lock down, so what was reserved out there may no longer cover
                    // the entry. Checked before it goes in, because the table allocates
                    // as soon as it does: settling afterwards would be settling up for
                    // memory already taken.
                    let needed = growth_for_one_more(&inner);
                    if slot_charge.as_ref().map_or(0, |c| c.bytes()) < needed {
                        slot_charge = None;
                        attempts += 1;
                        if attempts > RESERVE_ATTEMPTS {
                            drop(inner);
                            return Err(Error::budget(crate::error::BudgetInfo {
                                requested: needed,
                                in_use: self.budget.in_use(),
                                limit: self.budget.limit(),
                                retryable: true,
                            }));
                        }
                        continue;
                    }
                    inner.map.insert(ordinal, Slot::Loading);
                    note_table(&mut inner);
                    // Counted until this load publishes, because the push it will make
                    // happens with the shard locked, where nothing can reserve.
                    inner.pending_pushes += 1;
                    if let Some(charge) = slot_charge.take() {
                        inner.book.absorb(charge);
                    }
                    // Only ever downwards from here: the reservation above covered the
                    // most the table could have grown by, so this cannot fail. If it
                    // ever does, the table has grown with nothing covering it.
                    let settled = settle_bookkeeping(&mut inner, &self.budget);
                    debug_assert!(
                        settled.is_ok(),
                        "a shard's table grew past what was reserved for it"
                    );
                    break;
                }
            }
        }
        drop(inner);

        Metrics::bump(&self.metrics.cache_misses, 1);

        // Releases the in-flight marker even if `load` panics, so waiters never hang.
        let mut in_flight = InFlight {
            shard,
            ordinal,
            armed: true,
            budget: &self.budget,
        };
        let loaded = load();
        in_flight.armed = false;

        let mut inner = shard.lock();
        inner.map.remove(&ordinal);
        // Whatever happens now, this load makes no further push.
        inner.pending_pushes = inner.pending_pushes.saturating_sub(1);
        match loaded {
            Ok((pin, bytes)) => {
                inner.resident += bytes;
                inner.map.insert(
                    ordinal,
                    Slot::Ready {
                        pin: pin.clone(),
                        bytes,
                        referenced: true,
                    },
                );
                inner.clock.push_back(ordinal);
                note_table(&mut inner);
                self.evict_down_to_capacity(&mut inner);
                // The page is loaded and charged whatever happens next; caching it is
                // the optional part. If the shard's table cannot be paid for, give up
                // the least useful pages, and then this one, rather than the budget.
                while settle_bookkeeping(&mut inner, &self.budget).is_err() {
                    if evict_one(&mut inner, &self.budget).is_none() {
                        break;
                    }
                    Metrics::bump(&self.metrics.evictions, 1);
                }
                if settle_bookkeeping(&mut inner, &self.budget).is_err() {
                    if let Some(Slot::Ready { bytes, .. }) = inner.map.remove(&ordinal) {
                        inner.resident = inner.resident.saturating_sub(bytes);
                    }
                    release_bookkeeping(&mut inner, &self.budget);
                }
                drop(inner);
                shard.ready.notify_all();
                Ok(pin)
            }
            Err(e) => {
                // The slot is gone, so give back the room the map grew to hold it.
                release_bookkeeping(&mut inner, &self.budget);
                drop(inner);
                shard.ready.notify_all();
                Err(e)
            }
        }
    }

    fn evict_down_to_capacity(&self, inner: &mut ShardInner) {
        while inner.resident > self.capacity_per_shard {
            if evict_one(inner, &self.budget).is_none() {
                break;
            }
            Metrics::bump(&self.metrics.evictions, 1);
        }
    }

    /// Drops cached references until at least `target` bytes have been released, or no
    /// further progress is possible. Returns the number of bytes dropped.
    ///
    /// The bytes are only actually freed for pages no guard still holds.
    pub(crate) fn evict_bytes(&self, target: u64) -> u64 {
        let mut released = 0u64;
        let shard_count = self.shards.len();
        let start = self.next_victim_shard.fetch_add(1, AtomicOrdering::Relaxed);
        let mut idle_shards = 0usize;
        let mut cursor = 0usize;
        while released < target && idle_shards < shard_count {
            let shard = &self.shards[(start + cursor) % shard_count];
            cursor += 1;
            let mut inner = shard.lock();
            match evict_one(&mut inner, &self.budget) {
                Some(bytes) => {
                    released += bytes;
                    idle_shards = 0;
                    Metrics::bump(&self.metrics.evictions, 1);
                }
                None => idle_shards += 1,
            }
        }
        released
    }
}

fn ring_bytes(capacity: usize) -> u64 {
    capacity as u64 * std::mem::size_of::<u64>() as u64
}

/// What a ring holding `entries` costs, given what it has room for now.
///
/// A ring doubles, and while the last growth moves the entries across, the buffer it
/// came from is still there. Both are counted, because both are live at once.
fn ring_target_bytes(capacity: usize, entries: usize) -> u64 {
    if entries <= capacity {
        return ring_bytes(capacity);
    }
    // It doubles from what it has, which is neither a power of two nor at least four
    // once it has been shrunk to fit: a ring of seven grows to fourteen, and one of
    // three to six. A growth never leaves it smaller than four, however little it had,
    // so one of one grows to four rather than to two.
    let mut grown = capacity;
    while grown < entries {
        grown = grown.saturating_mul(2).max(RING_MIN_CAPACITY);
    }
    ring_bytes(grown) + ring_bytes(grown / 2)
}

fn bookkeeping_footprint(inner: &ShardInner) -> u64 {
    hash_table_bytes(inner.map_slots, SLOT_BYTES)
        + ring_target_bytes(
            inner.clock.capacity(),
            inner.clock.len().saturating_add(inner.pending_pushes),
        )
}

/// Takes note of a table that has just grown.
fn note_table(inner: &mut ShardInner) {
    inner.map_slots = inner.map_slots.max(inner.map.capacity());
}

/// What a shard's table and ring would cost with one more entry in them, beyond what is
/// already charged for.
///
/// A ring with no free slot at least doubles, like a table. Asked before the entry goes
/// in, so the room is reserved before the allocation rather than after it.
///
/// Growing means allocating the new table, moving the entries across and only then
/// letting the old one go, so both are live at once and the reservation covers both. It
/// comes back down to the new one alone once the move is done.
fn growth_for_one_more(inner: &ShardInner) -> u64 {
    let map_want = hash_table_peak_bytes(
        inner.map_slots,
        inner.map.capacity(),
        inner.map.len(),
        SLOT_BYTES,
    );
    let ring_want = ring_target_bytes(
        inner.clock.capacity(),
        inner
            .clock
            .len()
            .saturating_add(inner.pending_pushes)
            .saturating_add(1),
    );
    map_want
        .saturating_add(ring_want)
        .saturating_sub(inner.book.bytes())
}

/// Brings a shard's bookkeeping charge in line with what it has allocated.
///
/// Called after every change to the table or the ring, with the shard locked. Growing
/// can fail, which is the whole point: a shard's tables are memory like any other, and a
/// budget with no room for them has to say so rather than quietly grow past it.
/// Shrinking always succeeds.
fn settle_bookkeeping(inner: &mut ShardInner, budget: &Arc<Budget>) -> Result<()> {
    let want = bookkeeping_footprint(inner);
    inner.book.settle(want, budget)
}

/// Removes the in-flight marker if the load unwinds.
struct InFlight<'a> {
    shard: &'a Shard,
    ordinal: u64,
    armed: bool,
    /// So the marker's room goes back with it.
    budget: &'a Arc<Budget>,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut inner = self.shard.lock();
            inner.map.remove(&self.ordinal);
            inner.pending_pushes = inner.pending_pushes.saturating_sub(1);
            release_bookkeeping(&mut inner, self.budget);
            drop(inner);
            self.shard.ready.notify_all();
        }
    }
}

/// Gives back the map and ring capacity a shard no longer needs.
///
/// Neither shrinks on its own, so a shard that filled up and was then evicted down keeps
/// the memory it grew into while the per-page reservations that stood for it are gone.
/// Shrinking when a shard has emptied to half its capacity keeps what it retains inside
/// the per-page allowance, and costs a rehash only after the shard has halved. There is
/// no floor: an empty shard gives everything back, and a cache spread over many shards
/// is exactly where a small floor each adds up to something.
fn release_bookkeeping(inner: &mut ShardInner, budget: &Arc<Budget>) {
    // Shrinking allocates the smaller table before it lets the bigger one go, so both
    // are live while the entries move. That is reserved first, and the shrink is skipped
    // when it does not fit: the room it would give back is worth having, but not at the
    // cost of going over the budget while taking it.
    if inner.map_slots > 2 * inner.map.len() {
        let during = bookkeeping_footprint(inner) + hash_table_bytes(inner.map.len(), SLOT_BYTES);
        if inner.book.settle(during, budget).is_ok() {
            let before = inner.map.capacity();
            inner.map.shrink_to_fit();
            // A table that was not rebuilt still has the buckets it had, whatever its
            // capacity reads now.
            if inner.map.capacity() != before {
                inner.map_slots = inner.map.capacity();
            }
        }
    }
    // The ring keeps room for the pushes already promised as well as what it holds: a
    // load that found a free slot when it started reserved nothing for one, and taking
    // that slot away now would leave it allocating with nothing covering it.
    let keep = inner.clock.len().saturating_add(inner.pending_pushes);
    if inner.clock.capacity() > 2 * keep {
        // What it will have afterwards, rounded up, because a ring may keep more than it
        // was asked to.
        let after = ring_bytes(keep.max(4).next_power_of_two());
        let during = bookkeeping_footprint(inner) + after;
        if inner.book.settle(during, budget).is_ok() {
            inner.clock.shrink_to(keep);
        }
    }
    // Only ever downwards from here, so it cannot fail.
    let _ = settle_bookkeeping(inner, budget);
}

/// One CLOCK step: evicts the first unreferenced page, giving referenced pages a second
/// chance.
fn evict_one(inner: &mut ShardInner, budget: &Arc<Budget>) -> Option<u64> {
    // Bounded, because a ring full of referenced pages hands every one of them a second
    // chance before any of them can go.
    let mut steps = inner.clock.len().saturating_mul(2).saturating_add(1);
    while steps > 0 {
        steps -= 1;
        let ordinal = inner.clock.pop_front()?;
        match inner.map.get_mut(&ordinal) {
            Some(Slot::Ready {
                referenced, bytes, ..
            }) => {
                if *referenced {
                    *referenced = false;
                    inner.clock.push_back(ordinal);
                } else {
                    let bytes = *bytes;
                    inner.map.remove(&ordinal);
                    inner.resident = inner.resident.saturating_sub(bytes);
                    release_bookkeeping(inner, budget);
                    return Some(bytes);
                }
            }
            // A stale ring entry: the page is loading again, or was already removed.
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budget;
    use crate::format::PageOrdinal;
    use crate::page::PageBuffer;
    use std::sync::atomic::AtomicU64;
    use std::sync::Barrier;

    fn make_pin(budget: &Arc<Budget>, ordinal: u64, size: usize) -> (PagePin, u64) {
        let charge = budget.try_reserve(size as u64).unwrap();
        let pin = PagePin::new(Arc::new(PageBuffer::new(
            PageOrdinal::new(ordinal),
            vec![0u8; size].into_boxed_slice(),
            charge,
        )));
        (pin, size as u64)
    }

    #[test]
    fn serves_hits_without_loading() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = PageCache::new(1 << 20, 4, Arc::clone(&metrics), Arc::clone(&budget));
        let loads = AtomicU64::new(0);

        for _ in 0..5 {
            cache
                .get_or_load(
                    7,
                    |bytes| budget.try_reserve(bytes),
                    || {
                        loads.fetch_add(1, AtomicOrdering::SeqCst);
                        Ok(make_pin(&budget, 7, 64))
                    },
                )
                .unwrap();
        }
        assert_eq!(loads.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(metrics.snapshot().cache_hits, 4);
    }

    #[test]
    fn concurrent_misses_load_once() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = Arc::new(PageCache::new(
            1 << 20,
            8,
            Arc::clone(&metrics),
            Arc::clone(&budget),
        ));
        let loads = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(Barrier::new(16));

        std::thread::scope(|s| {
            for _ in 0..16 {
                let cache = Arc::clone(&cache);
                let budget = Arc::clone(&budget);
                let loads = Arc::clone(&loads);
                let barrier = Arc::clone(&barrier);
                s.spawn(move || {
                    barrier.wait();
                    let pin = cache
                        .get_or_load(
                            42,
                            |bytes| budget.try_reserve(bytes),
                            || {
                                loads.fetch_add(1, AtomicOrdering::SeqCst);
                                std::thread::sleep(std::time::Duration::from_millis(5));
                                Ok(make_pin(&budget, 42, 128))
                            },
                        )
                        .unwrap();
                    assert_eq!(pin.ordinal(), PageOrdinal::new(42));
                });
            }
        });
        assert_eq!(loads.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn failed_loads_wake_waiters_and_are_not_cached() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = Arc::new(PageCache::new(
            1 << 20,
            2,
            Arc::clone(&metrics),
            Arc::clone(&budget),
        ));
        let attempts = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(Barrier::new(4));

        std::thread::scope(|s| {
            for _ in 0..4 {
                let cache = Arc::clone(&cache);
                let attempts = Arc::clone(&attempts);
                let barrier = Arc::clone(&barrier);
                let budget = Arc::clone(&budget);
                s.spawn(move || {
                    barrier.wait();
                    let _ = cache.get_or_load(
                        1,
                        |bytes| budget.try_reserve(bytes),
                        || {
                            attempts.fetch_add(1, AtomicOrdering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            Err(crate::error::Error::corrupt("boom"))
                        },
                    );
                });
            }
        });
        // Every waiter retried rather than hanging, and nothing was published.
        assert!(attempts.load(AtomicOrdering::SeqCst) >= 1);
        assert!(cache.get(1).is_none());

        // A later successful load still works.
        let pin = cache
            .get_or_load(
                1,
                |bytes| budget.try_reserve(bytes),
                || Ok(make_pin(&budget, 1, 32)),
            )
            .unwrap();
        assert_eq!(pin.ordinal(), PageOrdinal::new(1));
    }

    #[test]
    fn a_panicking_load_does_not_strand_waiters() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = Arc::new(PageCache::new(
            1 << 20,
            2,
            Arc::clone(&metrics),
            Arc::clone(&budget),
        ));

        let cache2 = Arc::clone(&cache);
        let thread_budget = Arc::clone(&budget);
        let handle = std::thread::spawn(move || {
            let _ = cache2.get_or_load(
                9,
                |bytes| thread_budget.try_reserve(bytes),
                || -> Result<(PagePin, u64)> { panic!("load blew up") },
            );
        });
        assert!(handle.join().is_err());

        // The slot was released, so a later load proceeds.
        let pin = cache
            .get_or_load(
                9,
                |bytes| budget.try_reserve(bytes),
                || Ok(make_pin(&budget, 9, 16)),
            )
            .unwrap();
        assert_eq!(pin.ordinal(), PageOrdinal::new(9));
    }

    #[test]
    fn evicts_down_to_capacity() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        // One shard so the capacity maths is exact.
        let cache = PageCache::new(256, 1, Arc::clone(&metrics), Arc::clone(&budget));
        for i in 0..10u64 {
            cache
                .get_or_load(
                    i,
                    |bytes| budget.try_reserve(bytes),
                    || Ok(make_pin(&budget, i, 64)),
                )
                .unwrap();
        }
        assert!(
            cache.resident_bytes() <= 256,
            "resident {}",
            cache.resident_bytes()
        );
        assert!(metrics.snapshot().evictions > 0);
    }

    #[test]
    fn eviction_keeps_pinned_pages_alive() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = PageCache::new(128, 1, Arc::clone(&metrics), Arc::clone(&budget));
        let held = cache
            .get_or_load(
                0,
                |bytes| budget.try_reserve(bytes),
                || Ok(make_pin(&budget, 0, 64)),
            )
            .unwrap();
        let guard = held.clone().into_guard(0..64).unwrap();
        for i in 1..10u64 {
            cache
                .get_or_load(
                    i,
                    |bytes| budget.try_reserve(bytes),
                    || Ok(make_pin(&budget, i, 64)),
                )
                .unwrap();
        }
        assert!(cache.get(0).is_none(), "page 0 should have been evicted");
        assert_eq!(guard.as_bytes().len(), 64);
        assert!(budget.in_use() >= 64, "the pinned page keeps its charge");
    }

    /// Shrinking a table allocates the smaller one before it lets the bigger one go, so
    /// both are live while the entries move. With no room for the pair of them, the
    /// table is kept rather than shrunk past the budget.
    #[test]
    fn a_shard_keeps_its_table_when_the_move_cannot_be_reserved() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(64 * 1024);
        let cache = PageCache::new(64 * 1024, 1, Arc::clone(&metrics), Arc::clone(&budget));
        for i in 0..200u64 {
            cache
                .get_or_load(
                    i,
                    |bytes| budget.try_reserve(bytes),
                    || Ok(make_pin(&budget, i, 16)),
                )
                .unwrap();
        }
        let grown = cache.shards[0].lock().map_slots;
        assert!(grown >= 200, "the table grew to hold them: {grown}");

        // Everything that is left, so the move a shrink needs has nowhere to go. The
        // pages themselves are small, so evicting them all does not give back enough.
        let mut held = Vec::new();
        while let Ok(charge) = budget.try_reserve(64) {
            held.push(charge);
        }

        // Enough to take the table past half empty, which is when it would be shrunk,
        // but not so much that the pages it gives back pay for the move.
        cache.evict_bytes(1_600);
        let (len, slots) = {
            let inner = cache.shards[0].lock();
            (inner.map.len(), inner.map_slots)
        };
        assert!(len > 0 && slots > 2 * len, "half empty: {len} of {slots}");
        assert_eq!(
            slots, grown,
            "the table is kept when the move cannot be reserved"
        );

        // With the room back, the next entry to go takes the slots with it.
        drop(held);
        cache.evict_bytes(16);
        assert!(
            cache.shards[0].lock().map_slots < grown,
            "the slots go back once the move fits"
        );
    }

    /// A miss reserves with the shard unlocked, so the shard can grow before the entry
    /// goes in. Inserting on the strength of what was reserved back then grew the table
    /// with nothing covering it, and on a budget with no room to make it up afterwards
    /// the shard was left holding a table it had not paid for.
    ///
    /// What this drives is the shape of that: a shard grown by one thread while another
    /// waits inside its reservation. It passes against the old code as well, because the
    /// discrepancy is at its worst while the entry goes in and is settled afterwards
    /// whenever there is room. The `debug_assert` in `get_or_load` is what would catch
    /// it, in this test or in any other, and it needs an interleaving finer than a
    /// barrier gives.
    #[test]
    fn a_shard_that_grew_while_a_miss_waited_reserves_the_table_it_ends_with() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Barrier;

        let metrics = Arc::new(Metrics::default());
        // Tight, so that settling up after the fact cannot quietly make good what was
        // taken: the reservation has to be right before the table grows.
        let budget = Budget::new(7_500);
        let cache = PageCache::new(7_500, 1, Arc::clone(&metrics), Arc::clone(&budget));
        let reserved = Barrier::new(2);
        let grown = Barrier::new(2);
        let waited = AtomicBool::new(false);

        std::thread::scope(|s| {
            s.spawn(|| {
                let _ = cache.get_or_load(
                    0,
                    |bytes| {
                        let charge = budget.try_reserve(bytes)?;
                        // Once: hold here with the shard unlocked, so the other thread
                        // can grow the table this reservation was worked out for.
                        if !waited.swap(true, AtomicOrdering::SeqCst) {
                            reserved.wait();
                            grown.wait();
                        }
                        Ok(charge)
                    },
                    || {
                        Err(crate::error::Error::invalid(
                            "the load itself is not the point",
                        ))
                    },
                );
            });
            s.spawn(|| {
                reserved.wait();
                // Some of these run out of room, which is the point: the budget has no
                // slack left to cover a table that grew without asking.
                for i in 1..200u64 {
                    let _ = cache.get_or_load(
                        i,
                        |bytes| budget.try_reserve(bytes),
                        || Ok(make_pin(&budget, i, 16)),
                    );
                }
                grown.wait();
            });
        });

        let inner = cache.shards[0].lock();
        let footprint = bookkeeping_footprint(&inner);
        assert!(
            inner.book.bytes() >= footprint,
            "the shard's table costs {footprint} bytes with {} charged for it",
            inner.book.bytes()
        );
    }

    /// A free slot in the ring is free for one push. Two loads in flight each saw the
    /// same one and both decided the ring would not grow, so the second to publish grew
    /// it with nothing covering it. What the ring will cost is now worked out from how
    /// many pushes are coming, not from what each of them found when it started.
    #[test]
    fn a_free_ring_slot_is_free_for_one_push() {
        // Three held in a ring with room for four.
        assert_eq!(ring_target_bytes(4, 4), ring_bytes(4), "the fourth fits");
        assert!(
            ring_target_bytes(4, 5) > ring_bytes(4),
            "a fifth does not, and the ring it grows into is live beside the old one"
        );

        // Seventeen pushes coming at an empty ring: what it ends up with, and the buffer
        // it came from while the last growth moves across.
        assert_eq!(ring_target_bytes(0, 17), ring_bytes(32) + ring_bytes(16));
        // Once it has the room, nothing extra is needed to fill it.
        assert_eq!(ring_target_bytes(32, 17), ring_bytes(32));

        // Shrinking leaves a ring whose size is whatever it was asked for, and it
        // doubles from there: seven grows to fourteen, not to eight, and three to six,
        // not to four.
        assert_eq!(ring_target_bytes(7, 8), ring_bytes(14) + ring_bytes(7));
        assert_eq!(ring_target_bytes(7, 7), ring_bytes(7));
        assert_eq!(ring_target_bytes(3, 4), ring_bytes(6) + ring_bytes(3));
        // What a ring really grows to, measured: however little it had, a growth leaves
        // it with at least four.
        for (from, to) in [
            (0usize, 4usize),
            (1, 4),
            (2, 4),
            (3, 6),
            (5, 10),
            (7, 14),
            (9, 18),
        ] {
            assert_eq!(
                ring_target_bytes(from, from + 1),
                ring_bytes(to) + ring_bytes(to / 2),
                "a ring of {from} grows to {to}"
            );
        }
        assert_eq!(ring_target_bytes(1, 1), ring_bytes(1));
    }

    /// Shrinking the ring used to fit what it holds, taking away the slot a load in
    /// flight had already been told it could use. That load then pushed into a ring with
    /// nothing covering the room it needed.
    #[test]
    fn shrinking_the_ring_keeps_room_for_a_push_already_promised() {
        let budget = Budget::new(1 << 20);
        let mut inner = ShardInner {
            clock: VecDeque::with_capacity(8),
            ..Default::default()
        };
        let capacity = inner.clock.capacity();
        assert!(capacity >= 8, "a ring with room to give back: {capacity}");

        // One load in flight, which found a free slot and so reserved nothing for it.
        inner.pending_pushes = 1;
        release_bookkeeping(&mut inner, &budget);
        assert!(
            inner.clock.capacity() >= inner.clock.len() + inner.pending_pushes,
            "the promised push still has a slot: {} for {} held and {} promised",
            inner.clock.capacity(),
            inner.clock.len(),
            inner.pending_pushes
        );

        // With nothing in flight, the ring gives everything back.
        inner.pending_pushes = 0;
        release_bookkeeping(&mut inner, &budget);
        assert_eq!(inner.clock.capacity(), 0);
    }

    /// Seventeen loads started before any of them finishes: by the time the last one
    /// publishes, the ring has grown under all the others, and what it reserved when it
    /// started is for a ring several sizes smaller.
    #[test]
    fn a_load_that_publishes_last_reserved_for_the_ring_it_finds() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = PageCache::new(1 << 20, 1, Arc::clone(&metrics), Arc::clone(&budget));

        // Every marker goes in first, then every page is published, which is the order a
        // wave of concurrent misses arrives in.
        for i in 0..17u64 {
            cache
                .get_or_load(
                    i,
                    |bytes| budget.try_reserve(bytes),
                    || Ok(make_pin(&budget, i, 16)),
                )
                .unwrap();
            let inner = cache.shards[0].lock();
            let footprint = bookkeeping_footprint(&inner);
            assert!(
                inner.book.bytes() >= footprint,
                "after {} loads the shard costs {footprint} bytes with {} charged",
                i + 1,
                inner.book.bytes()
            );
        }
    }

    /// A ring that has been shrunk holds whatever it was shrunk to, which is not a power
    /// of two. Growing it again doubles from there, and the estimate used to assume
    /// otherwise and come up short.
    #[test]
    fn a_shrunk_ring_that_grows_again_is_reserved_for_what_it_doubles_to() {
        let metrics = Arc::new(Metrics::default());
        let budget = Budget::new(1 << 20);
        let cache = PageCache::new(1 << 20, 1, Arc::clone(&metrics), Arc::clone(&budget));

        let load = |ordinal: u64| {
            cache
                .get_or_load(
                    ordinal,
                    |bytes| budget.try_reserve(bytes),
                    || Ok(make_pin(&budget, ordinal, 64)),
                )
                .unwrap()
        };
        for i in 0..16u64 {
            load(i);
        }
        // Enough of them out that the ring is shrunk, and then one more in.
        cache.evict_bytes(9 * 64);
        load(100);

        let inner = cache.shards[0].lock();
        let footprint = bookkeeping_footprint(&inner);
        assert!(
            inner.book.bytes() >= footprint,
            "the shard costs {footprint} bytes with {} charged, ring {}",
            inner.book.bytes(),
            inner.clock.capacity()
        );
    }
}
