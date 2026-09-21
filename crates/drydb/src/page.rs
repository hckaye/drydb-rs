//! Page buffers and the guards that keep them alive.
//!
//! The cache owns immutable [`PageBuffer`]s. A [`PagePin`] is a counted reference to
//! one; a [`ValueGuard`] is a pin plus a validated byte range. Borrowing the bytes
//! borrows the guard, so a slice can never outlive the buffer, and eviction or dropping
//! the [`Database`](crate::Database) cannot invalidate a slice that is still reachable.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::budget::Charge;
use crate::error::{Error, Result};
use crate::format::node::PageView;
use crate::format::PageOrdinal;

/// An immutable page, owned by the cache and shared through [`PagePin`].
#[derive(Debug)]
pub(crate) struct PageBuffer {
    ordinal: PageOrdinal,
    data: Box<[u8]>,
    /// Which tree has already checked this page's digests against its keys, or zero.
    ///
    /// Lives with the buffer, so it is cleared whenever the page is evicted and read
    /// again, and it is keyed by tree because two trees read the same bytes with
    /// different key encodings.
    validated_for: AtomicU64,
    /// Released when the last pin drops.
    _charge: Charge,
}

impl PageBuffer {
    pub(crate) fn new(ordinal: PageOrdinal, data: Box<[u8]>, charge: Charge) -> PageBuffer {
        PageBuffer {
            ordinal,
            data,
            validated_for: AtomicU64::new(0),
            _charge: charge,
        }
    }
}

/// A counted reference to a cached page.
///
/// Cloning is an atomic increment. While any pin exists the bytes stay valid and the
/// buffer keeps its budget charge, whether or not the cache still lists it.
#[derive(Debug, Clone)]
pub struct PagePin(Arc<PageBuffer>);

impl PagePin {
    pub(crate) fn new(buffer: Arc<PageBuffer>) -> PagePin {
        PagePin(buffer)
    }

    /// The page's ordinal.
    pub fn ordinal(&self) -> PageOrdinal {
        self.0.ordinal
    }

    /// The whole page, including its 28-byte prefix.
    pub fn bytes(&self) -> &[u8] {
        &self.0.data
    }

    /// Page length in bytes.
    pub fn len(&self) -> usize {
        self.0.data.len()
    }

    /// Always `false`: a page always carries at least its prefix.
    pub fn is_empty(&self) -> bool {
        self.0.data.is_empty()
    }

    /// Whether `tree` has already checked this page's digests.
    pub(crate) fn digests_checked_by(&self, tree: u64) -> bool {
        self.0.validated_for.load(Ordering::Acquire) == tree
    }

    /// Records that `tree` has checked this page's digests.
    pub(crate) fn mark_digests_checked(&self, tree: u64) {
        self.0.validated_for.store(tree, Ordering::Release);
    }

    /// Parses the page structure. Cheap: the store already validated it on load.
    pub(crate) fn view(&self) -> Result<PageView<'_>> {
        PageView::parse(&self.0.data).map_err(|e| e.at_page(self.0.ordinal.get()))
    }

    /// Turns the pin into a guard over `range`, checking the bounds.
    pub(crate) fn into_guard(self, range: Range<usize>) -> Result<ValueGuard> {
        if range.start > range.end || range.end > self.0.data.len() {
            return Err(Error::corrupt(format!(
                "value range {}..{} is outside the {} byte page",
                range.start,
                range.end,
                self.0.data.len()
            ))
            .at_page(self.0.ordinal.get()));
        }
        Ok(ValueGuard { pin: self, range })
    }
}

/// A value, kept alive by the page it lives on.
///
/// The bytes are borrowed straight from the page buffer: reading a value copies
/// nothing. The guard is `Send` and `Sync`, and outlives the
/// [`Database`](crate::Database) it came from.
#[derive(Debug, Clone)]
pub struct ValueGuard {
    pin: PagePin,
    range: Range<usize>,
}

impl ValueGuard {
    /// The value bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.pin.bytes()[self.range.clone()]
    }

    /// Value length in bytes.
    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The page the value lives on.
    pub fn page(&self) -> PageOrdinal {
        self.pin.ordinal()
    }

    /// The value's byte range inside that page.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// The pin the guard holds.
    pub fn pin(&self) -> &PagePin {
        &self.pin
    }

    /// Copies the value out, releasing nothing: the guard still holds its page.
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl AsRef<[u8]> for ValueGuard {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl std::ops::Deref for ValueGuard {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl PartialEq<[u8]> for ValueGuard {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_bytes() == other
    }
}

impl PartialEq<&[u8]> for ValueGuard {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_bytes() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budget;

    fn pin_of(data: Vec<u8>) -> PagePin {
        let budget = Budget::new(1 << 20);
        let charge = budget.try_reserve(data.len() as u64).unwrap();
        PagePin::new(Arc::new(PageBuffer::new(
            PageOrdinal::new(3),
            data.into_boxed_slice(),
            charge,
        )))
    }

    #[test]
    fn guard_borrows_the_page() {
        let pin = pin_of(b"0123456789".to_vec());
        let guard = pin.clone().into_guard(2..5).unwrap();
        assert_eq!(guard.as_bytes(), b"234");
        assert_eq!(guard.page(), PageOrdinal::new(3));
        drop(pin);
        // The guard still owns a reference, so the bytes stay valid.
        assert_eq!(guard.as_bytes(), b"234");
    }

    #[test]
    fn guard_rejects_out_of_range() {
        let pin = pin_of(b"abc".to_vec());
        assert!(pin.clone().into_guard(0..4).is_err());
        // A reversed range is rejected rather than treated as empty.
        let reversed = std::ops::Range {
            start: 3usize,
            end: 2usize,
        };
        assert!(pin.into_guard(reversed).is_err());
    }

    #[test]
    fn charge_is_released_with_the_last_pin() {
        let budget = Budget::new(1 << 20);
        let charge = budget.try_reserve(16).unwrap();
        let pin = PagePin::new(Arc::new(PageBuffer::new(
            PageOrdinal::new(0),
            vec![0u8; 16].into_boxed_slice(),
            charge,
        )));
        let guard = pin.clone().into_guard(0..8).unwrap();
        assert_eq!(budget.in_use(), 16);
        drop(pin);
        assert_eq!(budget.in_use(), 16, "a live guard keeps the charge");
        drop(guard);
        assert_eq!(budget.in_use(), 0);
    }
}
