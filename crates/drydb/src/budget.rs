//! The managed memory budget.
//!
//! Every allocation this crate makes on behalf of a query -- page buffers, decompression
//! scratch, page directory chunks -- is charged here before it is made, and the charge is
//! released when the allocation drops. A buffer that the cache evicted but a live
//! [`ValueGuard`](crate::ValueGuard) still holds keeps its charge, which is the point:
//! the number is what this crate is holding, not what the cache thinks it holds.
//!
//! What the budget does *not* cover is stated plainly so the number is not mistaken for
//! an RSS cap: the allocator's own overhead, values the caller copied out, the OS page
//! cache, pages made resident by an `mmap` source, and the file itself when it is an
//! in-memory image. The image is what is being read, the way a file on disk is, and it
//! is the caller's: a budget of a few kilobytes over an image of a few megabytes is a
//! normal thing to ask for, and it bounds the reading, not the thing being read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::error::{BudgetInfo, Error, Result};

/// Bytes charged on top of each buffer's capacity for its bookkeeping.
///
/// Covers the `Arc` control block, the buffer struct, and the room a structure needs
/// while it is being rehashed or regrown, when the old allocation and the new one are
/// both live. It is an estimate, not a measurement of allocator behaviour. The page
/// cache's own tables are not in here: it charges those by their actual capacity, which
/// a per-page constant cannot stand in for when a shard holds only one or two pages.
pub const BUFFER_OVERHEAD: u64 = 192;

/// The group of buckets a hash table scans at a time. It keeps one spare group past the
/// end so a scan that wraps around still reads whole groups, and aligns the allocation
/// to it.
const HASH_GROUP: u64 = 16;

/// What the smallest hash table that holds `entries` of `entry` bytes has allocated.
///
/// An upper bound, so a charge computed from it is never short: a table keeps a power of
/// two buckets, one entry and one control byte each, and leaves an eighth of them free
/// so that looking a missing key up still ends. Checked against what a real table
/// allocates for every count from one to six hundred, where it is never short.
pub(crate) fn hash_table_bytes(entries: usize, entry: usize) -> u64 {
    if entries == 0 {
        return 0;
    }
    let buckets = entries
        .saturating_mul(8)
        .div_ceil(7)
        .max(4)
        .next_power_of_two() as u64;
    (buckets * (entry as u64 + 1) + HASH_GROUP).next_multiple_of(HASH_GROUP)
}

/// What a hash table needs at its peak to take one more entry.
///
/// `slots` is what the table has room for at the bucket count it has now, `capacity`
/// what it says it will still take before it rehashes, and `len` how many it holds. The
/// two are not the same once entries have been removed: a removal leaves a slot the
/// table will not hand out again until it rehashes, so `capacity` falls while the
/// buckets stay. Only `capacity` says whether the next entry rehashes; only `slots` says
/// what the table costs.
///
/// Rehashing means allocating the new table, moving the entries across and only then
/// letting the old one go, so both are live at once. A reservation for the difference
/// alone is short by the old table for as long as the move takes.
pub(crate) fn hash_table_peak_bytes(
    slots: usize,
    capacity: usize,
    len: usize,
    entry: usize,
) -> u64 {
    let now = hash_table_bytes(slots, entry);
    if capacity > len {
        return now;
    }
    // It may rehash in place, keeping the buckets it has, or into a table twice the
    // size. Reserve for the larger of the two.
    let next = if slots == 0 {
        hash_table_bytes(1, entry)
    } else {
        hash_table_bytes(slots.saturating_mul(2), entry)
    };
    now.saturating_add(next)
}

/// A charge kept in step with what a structure has allocated.
///
/// For the tables this crate keeps for its own bookkeeping, which hold their slots after
/// their entries are gone. Charging per entry leaves that memory accounted to nobody;
/// charging what the table has allocated does not.
#[derive(Debug, Default)]
pub(crate) struct Bookkeeping(Option<Charge>);

impl Bookkeeping {
    /// Bytes currently charged.
    pub(crate) fn bytes(&self) -> u64 {
        self.0.as_ref().map_or(0, |c| c.bytes())
    }

    /// Folds a charge taken elsewhere into this one.
    pub(crate) fn absorb(&mut self, charge: Charge) {
        match &mut self.0 {
            Some(held) => held.absorb(charge),
            None => self.0 = Some(charge),
        }
    }

    /// Brings the charge to `want`. Growing can fail; shrinking cannot.
    pub(crate) fn settle(&mut self, want: u64, budget: &Arc<Budget>) -> Result<()> {
        let have = self.bytes();
        match want.cmp(&have) {
            std::cmp::Ordering::Greater => match &mut self.0 {
                Some(charge) => charge.grow(want - have)?,
                None => self.0 = Some(budget.try_reserve(want)?),
            },
            std::cmp::Ordering::Less if want == 0 => self.0 = None,
            std::cmp::Ordering::Less => {
                if let Some(charge) = &mut self.0 {
                    charge.shrink(have - want);
                }
            }
            std::cmp::Ordering::Equal => {}
        }
        Ok(())
    }
}

/// Something that can set aside memory on a database's budget.
///
/// Implemented by [`Database`](crate::Database) and [`Table`](crate::Table), which
/// reclaim cached pages to make room, and by a bare [`Budget`], which has nothing to
/// reclaim from. An adapter that allocates on a caller's behalf takes one of these
/// rather than a budget, so that asking for memory behaves the way it does inside the
/// database: a reservation that refuses to reclaim fails while the cache is holding
/// exactly the room it needed.
pub trait Reserve: Send + Sync + std::fmt::Debug {
    /// Sets `bytes` aside, reclaiming if it can and has to.
    fn reserve(&self, bytes: u64) -> Result<Charge>;
}

impl Reserve for Arc<Budget> {
    fn reserve(&self, bytes: u64) -> Result<Charge> {
        // A budget on its own holds nothing it could give back.
        self.try_reserve(bytes)
    }
}

/// A shared byte budget.
#[derive(Debug)]
pub struct Budget {
    limit: u64,
    used: AtomicU64,
    peak: AtomicU64,
}

impl Budget {
    /// Creates a budget of `limit` bytes.
    pub fn new(limit: u64) -> Arc<Budget> {
        Arc::new(Budget {
            limit,
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        })
    }

    /// The configured limit.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Bytes currently charged.
    pub fn in_use(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    /// Highest number of bytes ever charged at once.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Acquire)
    }

    /// Reserves `bytes`, or reports [`ErrorKind::BudgetExceeded`](crate::ErrorKind).
    ///
    /// The error says whether the request could ever fit: a request larger than the
    /// whole budget is not retryable, one that merely does not fit right now is.
    pub fn try_reserve(self: &Arc<Self>, bytes: u64) -> Result<Charge> {
        self.take(bytes)?;
        Ok(Charge {
            budget: Arc::clone(self),
            bytes,
        })
    }

    /// Adds `bytes` to the accounting, or reports the failure.
    fn take(&self, bytes: u64) -> Result<()> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = match used.checked_add(bytes) {
                Some(n) if n <= self.limit => n,
                _ => {
                    return Err(Error::budget(BudgetInfo {
                        requested: bytes,
                        in_use: used,
                        limit: self.limit,
                        retryable: bytes <= self.limit,
                    }))
                }
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.peak.fetch_max(next, Ordering::AcqRel);
                    return Ok(());
                }
                Err(current) => used = current,
            }
        }
    }

    fn release(&self, bytes: u64) {
        let previous = self.used.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(
            previous >= bytes,
            "budget underflow: released more than charged"
        );
    }
}

/// A value and the reservation that paid for the memory it holds.
///
/// A collection handed back to a caller has to keep its reservation for as long as the
/// memory is there, and keeping it in the elements is not enough: emptying a list drops
/// the elements and keeps the allocation, so the budget would get its bytes back while
/// the list still held them. Pairing the two means the reservation goes when the value
/// does.
///
/// The value is readable through [`Deref`](std::ops::Deref) and can be taken out with
/// [`Charged::into_inner`], which hands back the reservation beside it so the two can be
/// kept together.
#[derive(Debug)]
pub struct Charged<T> {
    value: T,
    charge: Charge,
}

impl<T> Charged<T> {
    /// Pairs a value with the reservation that covers it.
    pub fn new(value: T, charge: Charge) -> Charged<T> {
        Charged { value, charge }
    }

    /// What the reservation holds.
    pub fn charged_bytes(&self) -> u64 {
        self.charge.bytes()
    }

    /// The value and its reservation, to be kept together or released in that order.
    pub fn into_inner(self) -> (T, Charge) {
        (self.value, self.charge)
    }
}

impl<T> std::ops::Deref for Charged<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> AsRef<T> for Charged<T> {
    fn as_ref(&self) -> &T {
        &self.value
    }
}

/// A live reservation. Releases its bytes on drop.
#[derive(Debug)]
pub struct Charge {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Charge {
    /// Bytes this charge holds.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Folds another charge on the same budget into this one.
    ///
    /// For building one reservation out of several steps, where a step had to reclaim
    /// memory to succeed and so could not be a plain [`Charge::grow`].
    pub fn absorb(&mut self, other: Charge) {
        debug_assert!(
            Arc::ptr_eq(&self.budget, &other.budget),
            "charges from different budgets cannot be merged"
        );
        self.bytes += other.take_bytes();
    }

    /// Hands over the bytes without releasing them.
    ///
    /// The charge is dropped as usual, which releases nothing and lets go of its
    /// reference to the budget; forgetting it instead would keep the budget alive for as
    /// long as the charge that absorbed it.
    fn take_bytes(mut self) -> u64 {
        let bytes = self.bytes;
        self.bytes = 0;
        bytes
    }

    /// Reserves `bytes` more on the same budget, keeping one charge.
    ///
    /// For a structure that is built up piece by piece and has to stay inside the budget
    /// while it is being built, not only once it is finished. Nothing is added to the
    /// charge when the reservation fails.
    pub fn grow(&mut self, bytes: u64) -> Result<()> {
        self.budget.take(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    /// Gives `bytes` of this charge back, keeping the rest.
    ///
    /// For a structure that shrinks in place: the part that is gone should not stay
    /// charged until the whole reservation drops. Giving back more than is held gives
    /// back all of it.
    pub fn shrink(&mut self, bytes: u64) {
        let bytes = bytes.min(self.bytes);
        self.budget.release(bytes);
        self.bytes -= bytes;
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn absorbing_a_charge_keeps_the_bytes_and_drops_the_reference() {
        let budget = Budget::new(100);
        let mut a = budget.try_reserve(10).unwrap();
        let b = budget.try_reserve(20).unwrap();
        a.absorb(b);
        assert_eq!(a.bytes(), 30);
        assert_eq!(budget.in_use(), 30);
        // Only the test and `a` hold the budget; the absorbed charge let go of it.
        assert_eq!(Arc::strong_count(&budget), 2);
        drop(a);
        assert_eq!(budget.in_use(), 0);
        assert_eq!(Arc::strong_count(&budget), 1);
    }

    #[test]
    fn reserves_and_releases() {
        let budget = Budget::new(100);
        let a = budget.try_reserve(60).unwrap();
        assert_eq!(budget.in_use(), 60);
        let b = budget.try_reserve(40).unwrap();
        assert_eq!(budget.in_use(), 100);
        assert_eq!(
            budget.try_reserve(1).unwrap_err().kind(),
            ErrorKind::BudgetExceeded
        );
        drop(b);
        assert_eq!(budget.in_use(), 60);
        let _c = budget.try_reserve(40).unwrap();
        drop(a);
        assert_eq!(budget.in_use(), 40);
        assert_eq!(budget.peak(), 100);
    }

    #[test]
    fn distinguishes_retryable_from_impossible() {
        let budget = Budget::new(100);
        let _a = budget.try_reserve(90).unwrap();
        let err = budget.try_reserve(20).unwrap_err();
        assert!(err.is_retryable());
        let err = budget.try_reserve(200).unwrap_err();
        assert!(!err.is_retryable());
        let info = err.budget_info().unwrap();
        assert_eq!(info.requested, 200);
        assert_eq!(info.limit, 100);
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        let budget = Budget::new(64);
        let observed = Arc::new(AtomicU64::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let budget = Arc::clone(&budget);
                let observed = Arc::clone(&observed);
                s.spawn(move || {
                    for _ in 0..2000 {
                        if let Ok(c) = budget.try_reserve(16) {
                            observed.fetch_max(budget.in_use(), Ordering::AcqRel);
                            drop(c);
                        }
                    }
                });
            }
        });
        assert!(observed.load(Ordering::Acquire) <= 64);
        assert_eq!(budget.in_use(), 0);
    }

    /// A table that has to grow allocates the new one, moves the entries across and only
    /// then lets the old one go. A reservation for the difference between the two is
    /// short by the old table for as long as the move takes.
    #[test]
    fn a_growing_table_is_charged_for_both_halves_of_the_move() {
        const ENTRY: usize = 32;
        // Seven entries in a table that holds seven: the next one rehashes it.
        let before = hash_table_bytes(7, ENTRY);
        let after = hash_table_bytes(14, ENTRY);
        assert!(after > before, "{after} follows {before}");
        assert_eq!(hash_table_peak_bytes(7, 7, 7, ENTRY), before + after);

        // With a free slot there is no move, so there is nothing extra to cover.
        assert_eq!(hash_table_peak_bytes(7, 7, 6, ENTRY), before);

        // Removals leave slots the table will not hand out again, so what it will still
        // take can be less than its size suggests. The entry that rehashes it is then
        // one the size alone said there was room for.
        assert_eq!(hash_table_peak_bytes(7, 4, 4, ENTRY), before + after);

        // An empty table has nothing to hold on to while it takes its first entry.
        assert_eq!(hash_table_bytes(0, ENTRY), 0);
        assert_eq!(
            hash_table_peak_bytes(0, 0, 0, ENTRY),
            hash_table_bytes(1, ENTRY)
        );
    }

    /// The figure is what a table of that many buckets really allocates, rounded up to
    /// the alignment it asks for, so a charge computed from it is never short.
    #[test]
    fn a_table_is_charged_by_its_buckets() {
        const ENTRY: usize = 32;
        // Three entries live in four buckets, four need eight, seven fit eight, fourteen
        // sixteen.
        for (entries, buckets) in [(1usize, 4u64), (3, 4), (4, 8), (7, 8), (14, 16), (224, 256)] {
            let want = (buckets * (ENTRY as u64 + 1) + HASH_GROUP).next_multiple_of(HASH_GROUP);
            assert_eq!(hash_table_bytes(entries, ENTRY), want, "entries {entries}");
        }
    }
}
