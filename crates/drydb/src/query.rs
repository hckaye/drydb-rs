//! Cursors, ranges and counting.
//!
//! A cursor holds exactly one leaf page at a time (plus the blob page of the current
//! entry, when the value overflowed). Nothing accumulates: scanning a table of any size
//! costs the pages currently under the cursor, not the result set.
//!
//! Bounds are explicit [`Bound`]s. An empty key is a key, not an open end -- the upstream
//! C# API cannot express that difference, and `docs/compatibility.md` records where the
//! two therefore disagree.

use std::ops::Bound;
use std::ops::Range;
use std::sync::Arc;

use crate::btree::{SearchOp, Tree};
use crate::budget::{Charge, BUFFER_OVERHEAD};
use crate::error::{Error, Result};
use crate::format::node::{LeafValue, PageView};
use crate::page::{PagePin, ValueGuard};

/// Iteration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Smallest key first.
    Ascending,
    /// Largest key first.
    Descending,
}

/// An owned pair of bounds over encoded keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    /// Lower bound.
    pub lower: Bound<Vec<u8>>,
    /// Upper bound.
    pub upper: Bound<Vec<u8>>,
    /// Whether the range selects nothing whatever the bounds say.
    empty: bool,
}

impl KeyRange {
    /// Every key.
    pub fn all() -> KeyRange {
        KeyRange {
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
            empty: false,
        }
    }

    /// Builds a range from borrowed bounds.
    pub fn new(lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KeyRange {
        KeyRange {
            lower: own(lower),
            upper: own(upper),
            empty: false,
        }
    }

    /// Whether this range selects nothing whatever its bounds say.
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// A range from owned bounds.
    pub fn from_bounds(lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> KeyRange {
        KeyRange {
            lower,
            upper,
            empty: false,
        }
    }

    /// No keys at all.
    ///
    /// Distinct from a reversed range, which is a caller's mistake: this is what bounds
    /// that meet without overlapping describe.
    pub fn empty() -> KeyRange {
        KeyRange {
            lower: Bound::Excluded(Vec::new()),
            upper: Bound::Unbounded,
            empty: true,
        }
    }

    /// Exactly one key.
    pub fn point(key: &[u8]) -> KeyRange {
        KeyRange {
            lower: Bound::Included(key.to_vec()),
            upper: Bound::Included(key.to_vec()),
            empty: false,
        }
    }
}

fn own(bound: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
    }
}

fn bound_key(bound: &Bound<Vec<u8>>) -> Option<&[u8]> {
    match bound {
        Bound::Unbounded => None,
        Bound::Included(k) | Bound::Excluded(k) => Some(k.as_slice()),
    }
}

/// What an owned copy of one bound costs, with `extra` bytes the caller will append.
fn bound_cost(bound: Bound<&[u8]>, extra: u64) -> u64 {
    match bound {
        Bound::Unbounded => 0,
        Bound::Included(k) | Bound::Excluded(k) => (k.len() as u64)
            .saturating_add(extra)
            .saturating_add(BUFFER_OVERHEAD),
    }
}

/// Reserves what owned copies of these bounds will cost, before they are made.
///
/// A bound is a caller's key and can be any length, so a query that copies one is
/// spending the caller's budget and has to say so first. `extra` covers bytes the caller
/// appends to each bound, such as the record id of a non-unique index key.
pub(crate) fn reserve_bounds(
    tree: &Tree,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    extra: u64,
) -> Result<Charge> {
    let wanted = bound_cost(lower, extra).saturating_add(bound_cost(upper, extra));
    tree.store().reserve(wanted)
}

/// Makes room in `buf` for `len` bytes, reserving what that costs first.
///
/// For the small buffers a cursor keeps: a rebuilt key, or the index key a secondary
/// cursor hands back. Each is bounded by one key, but a cursor is a caller's object and
/// nothing stops a caller from holding many of them, so they are charged like anything
/// else.
///
/// The capacity is grown with `reserve_exact` rather than by letting `Vec` double, so
/// what is reserved is what is allocated. The allocator may still hand back more than was
/// asked for, so the charge is reconciled with the capacity afterwards, and the buffer is
/// released rather than kept if that no longer fits. Neither the buffer nor the charge
/// ever shrinks.
pub(crate) fn charge_buffer(
    tree: &Tree,
    charge: &mut Charge,
    buf: &mut Vec<u8>,
    len: usize,
) -> Result<()> {
    if buf.capacity() >= len {
        return Ok(());
    }
    charge_at_least(tree, charge, len)?;
    // The old allocation is released before the new one is taken, so the two never
    // exist at once. Its contents are about to be overwritten in any case.
    *buf = Vec::new();
    buf.reserve_exact(len);
    if let Err(e) = charge_at_least(tree, charge, buf.capacity()) {
        *buf = Vec::new();
        return Err(e);
    }
    Ok(())
}

/// Reconciles `charge` with a buffer that has already been written, for the encodings
/// whose rebuilt key length is not known in advance.
pub(crate) fn charge_written(tree: &Tree, charge: &mut Charge, buf: &mut Vec<u8>) -> Result<()> {
    if let Err(e) = charge_at_least(tree, charge, buf.capacity()) {
        *buf = Vec::new();
        return Err(e);
    }
    Ok(())
}

/// Grows `charge` until it covers `bytes` plus one allocation header.
fn charge_at_least(tree: &Tree, charge: &mut Charge, bytes: usize) -> Result<()> {
    let wanted = (bytes as u64).saturating_add(BUFFER_OVERHEAD);
    if wanted <= charge.bytes() {
        return Ok(());
    }
    let more = tree.store().reserve(wanted - charge.bytes())?;
    charge.absorb(more);
    Ok(())
}

fn borrowed_key(bound: Bound<&[u8]>) -> Option<&[u8]> {
    match bound {
        Bound::Unbounded => None,
        Bound::Included(k) | Bound::Excluded(k) => Some(k),
    }
}

/// Borrows an owned bound.
pub(crate) fn as_ref_bound(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
    }
}

/// Where the current entry's key bytes live.
#[derive(Debug, Clone, Copy)]
enum KeySource {
    /// A range inside the current leaf page.
    Page(usize, usize),
    /// Rebuilt from an exact digest into the cursor's own buffer.
    Rebuilt,
}

#[derive(Debug, Clone)]
struct Materialized {
    key: KeySource,
    value: Range<usize>,
    value_on_blob: bool,
}

/// Everything that says where a cursor stands, so a failed step can be undone.
///
/// Public inside the crate because a secondary cursor steps this one and can then fail
/// on its own account, resolving the reference the entry points at.
pub(crate) struct Position {
    page: Option<PagePin>,
    index: usize,
    leaf_limit: usize,
    leaf_ends_scan: bool,
    started: bool,
    finished: bool,
    page_steps_left: u64,
}

/// A borrowed cursor over one B+Tree.
///
/// [`Cursor::advance`] moves to the next entry and [`Cursor::current`] borrows it. The
/// borrow keeps the cursor immobile, so a reference can never outlive the page it points
/// into; [`EntryRef::to_guard`] turns it into an owning [`ValueGuard`] when a value has
/// to be kept.
pub struct Cursor {
    tree: Arc<Tree>,
    order: Order,
    range: KeyRange,
    page: Option<PagePin>,
    index: usize,
    /// Ascending: one past the last index to emit on this leaf.
    /// Descending: the lowest index to emit on this leaf.
    leaf_limit: usize,
    /// Whether the far bound was found on this leaf, ending the scan here.
    leaf_ends_scan: bool,
    started: bool,
    finished: bool,
    current: Option<Materialized>,
    key_buf: Vec<u8>,
    /// What `key_buf` costs.
    key_charge: Charge,
    blob: Option<PagePin>,
    page_steps_left: u64,
    /// The memory the owned copy of `range` takes, held for as long as the cursor is.
    _bounds_charge: Charge,
}

impl std::fmt::Debug for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cursor")
            .field("order", &self.order)
            .field("range", &self.range)
            .field("page", &self.page.as_ref().map(|p| p.ordinal()))
            .field("index", &self.index)
            .field("finished", &self.finished)
            .finish()
    }
}

impl Cursor {
    /// The reservation comes before the range it paid for, in this signature and in
    /// `build` below, because arguments are dropped in reverse: a path out of either that
    /// does not build a cursor would otherwise release the reservation while the copies
    /// it stands for are still there.
    pub(crate) fn new(
        tree: Arc<Tree>,
        bounds_charge: Charge,
        range: KeyRange,
        order: Order,
    ) -> Result<Cursor> {
        Cursor::build(tree, bounds_charge, range, order, true)
    }

    /// A cursor whose bounds are byte prefixes rather than keys.
    ///
    /// A prefix is shorter than a key, so it is not validated as one. The encodings that
    /// allow prefix queries order keys by their bytes, which is what lets a short prefix
    /// sit in the same order as the keys it selects.
    pub(crate) fn for_prefix(
        tree: Arc<Tree>,
        bounds_charge: Charge,
        range: KeyRange,
        order: Order,
    ) -> Result<Cursor> {
        Cursor::build(tree, bounds_charge, range, order, false)
    }

    fn build(
        tree: Arc<Tree>,
        bounds_charge: Charge,
        range: KeyRange,
        order: Order,
        validate_bounds: bool,
    ) -> Result<Cursor> {
        if range.empty {
            let key_charge = tree.store().budget().try_reserve(0)?;
            return Ok(Cursor {
                tree,
                order,
                range,
                page: None,
                index: 0,
                leaf_limit: 0,
                leaf_ends_scan: false,
                started: true,
                finished: true,
                current: None,
                key_buf: Vec::new(),
                key_charge,
                blob: None,
                page_steps_left: 0,
                _bounds_charge: bounds_charge,
            });
        }
        // Checked before they are compared: comparing two keys the encoding cannot read
        // reports what it makes of the bytes, which for a caller's mistake reads as
        // damage to the file rather than as the wrong length of key.
        if validate_bounds {
            if let Some(k) = bound_key(&range.lower) {
                tree.encoding().validate_key(k)?;
            }
            if let Some(k) = bound_key(&range.upper) {
                tree.encoding().validate_key(k)?;
            }
        }
        if let (Some(lo), Some(hi)) = (bound_key(&range.lower), bound_key(&range.upper)) {
            if tree.encoding().compare(lo, hi)? == std::cmp::Ordering::Greater {
                return Err(Error::invalid(
                    "range lower bound sorts above its upper bound",
                ));
            }
        }
        let page_steps_left = tree.store().page_count().saturating_add(1);
        let tree_charge = tree.store().budget().try_reserve(0)?;
        Ok(Cursor {
            tree,
            order,
            range,
            page: None,
            index: 0,
            leaf_limit: 0,
            leaf_ends_scan: false,
            started: false,
            finished: false,
            current: None,
            key_buf: Vec::new(),
            key_charge: tree_charge,
            blob: None,
            page_steps_left,
            _bounds_charge: bounds_charge,
        })
    }

    /// Iteration order.
    pub fn order(&self) -> Order {
        self.order
    }

    /// Moves to the next entry. Returns `false` once the range is exhausted.
    pub fn advance(&mut self) -> Result<bool> {
        self.current = None;
        self.blob = None;
        if self.finished {
            return Ok(false);
        }
        // Where the cursor stands now, put back if the step fails. Reading an entry can
        // fail for a reason the caller can do something about, `BudgetExceeded` above
        // all, and a cursor that had already moved would hand back the row after the one
        // that failed and lose the one in between.
        let saved = self.position();
        match self.step_and_read() {
            Ok(more) => Ok(more),
            Err(e) => {
                self.restore(saved);
                Err(e)
            }
        }
    }

    fn step_and_read(&mut self) -> Result<bool> {
        if !self.started {
            self.started = true;
            if !self.seek_first()? {
                self.finish();
                return Ok(false);
            }
        } else if !self.step()? {
            self.finish();
            return Ok(false);
        }
        self.materialize()?;
        Ok(true)
    }

    pub(crate) fn position(&self) -> Position {
        Position {
            page: self.page.clone(),
            index: self.index,
            leaf_limit: self.leaf_limit,
            leaf_ends_scan: self.leaf_ends_scan,
            started: self.started,
            finished: self.finished,
            page_steps_left: self.page_steps_left,
        }
    }

    pub(crate) fn restore(&mut self, saved: Position) {
        self.page = saved.page;
        self.index = saved.index;
        self.leaf_limit = saved.leaf_limit;
        self.leaf_ends_scan = saved.leaf_ends_scan;
        self.started = saved.started;
        self.finished = saved.finished;
        self.page_steps_left = saved.page_steps_left;
        self.current = None;
        self.blob = None;
    }

    /// The entry the cursor is on, if any.
    pub fn current(&self) -> Option<EntryRef<'_>> {
        let current = self.current.as_ref()?;
        let page = self.page.as_ref()?;
        let key = match current.key {
            KeySource::Page(offset, len) => &page.bytes()[offset..offset + len],
            KeySource::Rebuilt => self.key_buf.as_slice(),
        };
        let value_pin = if current.value_on_blob {
            self.blob.as_ref()?
        } else {
            page
        };
        Some(EntryRef {
            key,
            value_pin,
            value_range: current.value.clone(),
            page: page.ordinal(),
            overflow: current.value_on_blob,
        })
    }

    fn finish(&mut self) {
        self.finished = true;
        self.page = None;
        self.current = None;
        self.blob = None;
    }

    fn seek_first(&mut self) -> Result<bool> {
        let start = match self.order {
            Order::Ascending => match &self.range.lower {
                Bound::Unbounded => self.tree.min_leaf()?,
                Bound::Included(k) => self.tree.search(k, SearchOp::LowerBound)?,
                Bound::Excluded(k) => self.tree.search(k, SearchOp::UpperBound)?,
            },
            Order::Descending => match &self.range.upper {
                Bound::Unbounded => self.tree.max_leaf()?,
                Bound::Included(k) => self.descending_start(k, SearchOp::UpperBound)?,
                Bound::Excluded(k) => self.descending_start(k, SearchOp::LowerBound)?,
            },
        };
        let Some((pin, index)) = start else {
            return Ok(false);
        };
        self.page = Some(pin);
        self.index = index;
        self.recompute_leaf_bounds()?;
        self.skip_to_valid()
    }

    /// The entry just before the upper bound, stepping into the left sibling when the
    /// bound sits at the start of a leaf.
    fn descending_start(&self, key: &[u8], op: SearchOp) -> Result<Option<(PagePin, usize)>> {
        let Some((pin, index)) = self.tree.search(key, op)? else {
            // Every entry satisfies the bound: start from the maximum.
            return self.tree.max_leaf();
        };
        if index > 0 {
            return Ok(Some((pin, index - 1)));
        }
        // The bound sits at the start of this leaf, so the entry before it is the last
        // entry of the nearest leaf to the left that holds one.
        let mut pin = pin;
        let mut steps_left = self.tree.store().page_count().saturating_add(1);
        loop {
            let left = pin.view()?.left_sibling();
            let Some(left) = left else {
                // As in a scan: a chain that stops before the tree does would report an
                // empty range instead of the entries the break hides.
                if !self.tree.ends_at(pin.ordinal(), false)? {
                    return Err(Error::corrupt(format!(
                        "the leaf chain stops at page {} but the tree does not end there",
                        pin.ordinal()
                    ))
                    .at_page(pin.ordinal().get()));
                }
                return Ok(None);
            };
            if steps_left == 0 {
                return Err(Error::corrupt(
                    "leaf sibling chain visits more pages than the file contains; it is cyclic",
                ));
            }
            steps_left -= 1;
            pin = self.tree.step_sibling(pin.ordinal(), left, false)?;
            let count = pin.view()?.entry_count();
            if count > 0 {
                return Ok(Some((pin, count - 1)));
            }
        }
    }

    /// After positioning or moving pages, make sure `index` is inside the emit window;
    /// otherwise walk on.
    fn skip_to_valid(&mut self) -> Result<bool> {
        loop {
            let Some(page) = self.page.clone() else {
                return Ok(false);
            };
            let count = page.view()?.entry_count();
            let in_window = match self.order {
                Order::Ascending => self.index < self.leaf_limit.min(count),
                Order::Descending => {
                    count > 0 && self.index >= self.leaf_limit && self.index < count
                }
            };
            if in_window {
                return Ok(true);
            }
            if !self.next_page()? {
                return Ok(false);
            }
        }
    }

    fn step(&mut self) -> Result<bool> {
        match self.order {
            Order::Ascending => {
                self.index += 1;
                if self.index < self.leaf_limit {
                    return Ok(true);
                }
            }
            Order::Descending => {
                if self.index > self.leaf_limit {
                    self.index -= 1;
                    return Ok(true);
                }
            }
        }
        if !self.next_page()? {
            return Ok(false);
        }
        self.skip_to_valid()
    }

    /// Moves to the adjacent leaf, honouring the "the bound ended here" flag.
    fn next_page(&mut self) -> Result<bool> {
        if self.leaf_ends_scan {
            return Ok(false);
        }
        if self.page_steps_left == 0 {
            return Err(Error::corrupt(
                "leaf sibling chain visits more pages than the file contains; it is cyclic",
            ));
        }
        self.page_steps_left -= 1;

        let Some(page) = self.page.clone() else {
            return Ok(false);
        };
        let view = page.view()?;
        let sibling = match self.order {
            Order::Ascending => view.right_sibling(),
            Order::Descending => view.left_sibling(),
        };
        let Some(sibling) = sibling else {
            // Nothing to the side of this page. Either it is where the tree ends, or the
            // chain has lost its way and stopping here would hand back part of the
            // answer as though it were all of it.
            let ascending = matches!(self.order, Order::Ascending);
            if !self.tree.ends_at(page.ordinal(), ascending)? {
                return Err(Error::corrupt(format!(
                    "the leaf chain stops at page {} but the tree does not end there",
                    page.ordinal()
                ))
                .at_page(page.ordinal().get()));
            }
            return Ok(false);
        };
        let pin = self.tree.step_sibling(
            page.ordinal(),
            sibling,
            matches!(self.order, Order::Ascending),
        )?;
        let count = pin.view()?.entry_count();
        self.page = Some(pin);
        self.index = match self.order {
            Order::Ascending => 0,
            Order::Descending => count.saturating_sub(1),
        };
        self.recompute_leaf_bounds()?;
        Ok(true)
    }

    /// Resolves the far bound inside the current leaf, exactly once per page.
    fn recompute_leaf_bounds(&mut self) -> Result<()> {
        let Some(page) = self.page.clone() else {
            return Ok(());
        };
        let view = page.view()?;
        let count = view.entry_count();
        match self.order {
            Order::Ascending => {
                let (op, key) = match &self.range.upper {
                    Bound::Unbounded => {
                        self.leaf_limit = count;
                        self.leaf_ends_scan = false;
                        return Ok(());
                    }
                    Bound::Included(k) => (SearchOp::UpperBound, k),
                    Bound::Excluded(k) => (SearchOp::LowerBound, k),
                };
                match self.leaf_search(&view, key, op)? {
                    Some(index) => {
                        self.leaf_limit = index;
                        self.leaf_ends_scan = true;
                    }
                    None => {
                        self.leaf_limit = count;
                        self.leaf_ends_scan = false;
                    }
                }
            }
            Order::Descending => {
                let (op, key) = match &self.range.lower {
                    Bound::Unbounded => {
                        self.leaf_limit = 0;
                        self.leaf_ends_scan = false;
                        return Ok(());
                    }
                    Bound::Included(k) => (SearchOp::LowerBound, k),
                    Bound::Excluded(k) => (SearchOp::UpperBound, k),
                };
                match self.leaf_search(&view, key, op)? {
                    Some(index) => {
                        self.leaf_limit = index;
                        // Reaching index 0 means the whole leaf is inside the range and
                        // the left sibling may hold more.
                        self.leaf_ends_scan = index > 0;
                    }
                    None => {
                        // Every entry here sorts below the lower bound, and so does
                        // everything further left.
                        self.leaf_limit = count;
                        self.leaf_ends_scan = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn leaf_search(&self, view: &PageView<'_>, key: &[u8], op: SearchOp) -> Result<Option<usize>> {
        let digest = self.tree.digest_of(key)?;
        self.tree.leaf_search(view, key, digest, op)
    }

    fn materialize(&mut self) -> Result<()> {
        let page = self
            .page
            .clone()
            .ok_or_else(|| Error::corrupt("cursor lost its page"))?;
        let view = page.view()?;
        let index = self.index;
        let entry = view.leaf_entry(index)?;

        let key = if view.layout().omitted_keys {
            // Reserved before the rebuild writes anything. The reconciliation afterwards
            // is the backstop for an encoding that wrote more than it said it would.
            let capacity = self.tree.rebuilt_key_capacity()?;
            charge_buffer(
                &self.tree,
                &mut self.key_charge,
                &mut self.key_buf,
                capacity,
            )?;
            if self.tree.rebuild_key(&view, index, &mut self.key_buf)? {
                charge_written(&self.tree, &mut self.key_charge, &mut self.key_buf)?;
                KeySource::Rebuilt
            } else {
                KeySource::Page(entry.key_offset, entry.key_len)
            }
        } else {
            KeySource::Page(entry.key_offset, entry.key_len)
        };

        let (value, on_blob) = match entry.value {
            LeafValue::Inline { offset, len } => (offset..offset + len, false),
            LeafValue::Overflow { page: blob_page } => {
                let (pin, range) = self.tree.store().blob(blob_page)?;
                self.blob = Some(pin);
                (range, true)
            }
        };

        self.current = Some(Materialized {
            key,
            value,
            value_on_blob: on_blob,
        });
        Ok(())
    }
}

/// One entry, borrowed from the cursor.
#[derive(Debug)]
pub struct EntryRef<'a> {
    key: &'a [u8],
    value_pin: &'a PagePin,
    value_range: Range<usize>,
    page: crate::format::PageOrdinal,
    overflow: bool,
}

impl<'a> EntryRef<'a> {
    /// The entry key. On pages that omit key bytes this is rebuilt from the digest.
    pub fn key(&self) -> &'a [u8] {
        self.key
    }

    /// The entry value.
    pub fn value(&self) -> &[u8] {
        &self.value_pin.bytes()[self.value_range.clone()]
    }

    /// Value length.
    pub fn value_len(&self) -> usize {
        self.value_range.end - self.value_range.start
    }

    /// Whether the value lives on its own blob page.
    pub fn is_overflow(&self) -> bool {
        self.overflow
    }

    /// The leaf page the entry lives on.
    pub fn page(&self) -> crate::format::PageOrdinal {
        self.page
    }

    /// Turns the borrowed value into an owning guard, so it can outlive the cursor.
    pub fn to_guard(&self) -> Result<ValueGuard> {
        self.value_pin.clone().into_guard(self.value_range.clone())
    }

    /// Copies the key out.
    pub fn key_to_vec(&self) -> Vec<u8> {
        self.key.to_vec()
    }
}

/// Counts entries in a range without reading any value.
///
/// Takes borrowed bounds so that counting allocates nothing at all: unlike a cursor,
/// which outlives the call that created it, this never needs to own its bounds.
pub(crate) fn count_range(
    tree: &Arc<Tree>,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> Result<u64> {
    // Checked before they are compared, for the same reason as above.
    if let Some(k) = borrowed_key(lower) {
        tree.encoding().validate_key(k)?;
    }
    if let Some(k) = borrowed_key(upper) {
        tree.encoding().validate_key(k)?;
    }
    if let (Some(lo), Some(hi)) = (borrowed_key(lower), borrowed_key(upper)) {
        if tree.encoding().compare(lo, hi)? == std::cmp::Ordering::Greater {
            return Err(Error::invalid(
                "range lower bound sorts above its upper bound",
            ));
        }
    }

    let start = match lower {
        Bound::Unbounded => tree.min_leaf()?,
        Bound::Included(k) => tree.search(k, SearchOp::LowerBound)?,
        Bound::Excluded(k) => tree.search(k, SearchOp::UpperBound)?,
    };
    let Some((mut page, mut index)) = start else {
        return Ok(0);
    };

    let mut count: u64 = 0;
    let mut steps_left = tree.store().page_count().saturating_add(1);
    loop {
        let view = page.view()?;
        let entry_count = view.entry_count();
        let end_op = match upper {
            Bound::Unbounded => None,
            Bound::Included(k) => Some((SearchOp::UpperBound, k)),
            Bound::Excluded(k) => Some((SearchOp::LowerBound, k)),
        };
        if let Some((op, key)) = end_op {
            let digest = tree.digest_of(key)?;
            if let Some(bound_index) = tree.leaf_search(&view, key, digest, op)? {
                return Ok(count + (bound_index.saturating_sub(index)) as u64);
            }
        }
        count += entry_count.saturating_sub(index) as u64;

        let Some(right) = view.right_sibling() else {
            // As in a scan: a chain that stops before the tree does is a count of part
            // of the range given as the count of all of it.
            if !tree.ends_at(page.ordinal(), true)? {
                return Err(Error::corrupt(format!(
                    "the leaf chain stops at page {} but the tree does not end there",
                    page.ordinal()
                ))
                .at_page(page.ordinal().get()));
            }
            return Ok(count);
        };
        if steps_left == 0 {
            return Err(Error::corrupt(
                "leaf sibling chain visits more pages than the file contains; it is cyclic",
            ));
        }
        steps_left -= 1;
        page = tree.step_sibling(page.ordinal(), right, true)?;
        index = 0;
    }
}

/// Turns a byte prefix into a half-open key range.
///
/// Only meaningful for encodings whose order is byte lexicographic; callers check that
/// first.
pub(crate) fn prefix_range(prefix: &[u8]) -> KeyRange {
    if prefix.is_empty() {
        return KeyRange::all();
    }
    let mut end_len = prefix.len();
    while end_len > 0 && prefix[end_len - 1] == 0xFF {
        end_len -= 1;
    }
    if end_len == 0 {
        // Every byte is 0xFF: the prefix matches everything from here on.
        return KeyRange {
            lower: Bound::Included(prefix.to_vec()),
            upper: Bound::Unbounded,
            empty: false,
        };
    }
    let mut end = prefix[..end_len].to_vec();
    end[end_len - 1] += 1;
    KeyRange {
        lower: Bound::Included(prefix.to_vec()),
        upper: Bound::Excluded(end),
        empty: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_range_increments_the_last_byte() {
        let r = prefix_range(b"ab");
        assert_eq!(r.lower, Bound::Included(b"ab".to_vec()));
        assert_eq!(r.upper, Bound::Excluded(b"ac".to_vec()));
    }

    #[test]
    fn prefix_range_strips_trailing_ff() {
        let r = prefix_range(&[0x61, 0xff, 0xff]);
        assert_eq!(r.lower, Bound::Included(vec![0x61, 0xff, 0xff]));
        assert_eq!(r.upper, Bound::Excluded(vec![0x62]));
    }

    #[test]
    fn prefix_range_of_all_ff_is_open_ended() {
        let r = prefix_range(&[0xff, 0xff]);
        assert_eq!(r.upper, Bound::Unbounded);
    }

    #[test]
    fn empty_prefix_matches_everything() {
        assert_eq!(prefix_range(b""), KeyRange::all());
    }

    /// A cursor keeps the tree it reads from alive and nothing else. The reservation
    /// that paid for that tree used to go when the database did, leaving the tree the
    /// cursor is still reading from charged to nobody.
    #[test]
    fn a_cursor_outliving_its_database_keeps_its_tree_charged() {
        use crate::encoding::Int64Encoding;
        use crate::{Database, DatabaseBuilder};
        use std::sync::Arc;

        // Many tables, so what the catalog and its handles cost is unmistakable next to
        // what one cursor holds.
        let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
        for i in 0..200u32 {
            let table = builder
                .create_table(format!("t{i}"), Arc::new(Int64Encoding))
                .unwrap();
            builder
                .append(table, &Int64Encoding::encode(0), b"v")
                .unwrap();
        }
        let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();
        let table = db.table("t0").unwrap();
        let cursor = table.scan(Order::Ascending).unwrap();

        let budget = Arc::clone(cursor.tree.store().budget());
        let held = budget.in_use();
        assert!(held > 10_000, "the open charged {held} bytes");

        drop(table);
        drop(db);
        let after = budget.in_use();
        assert!(
            after >= held / 2,
            "the database went and took {} of {held} bytes of reservation with it, while \
             the cursor is still reading the tree they paid for",
            held - after
        );
        drop(cursor);
        assert_eq!(budget.in_use(), 0, "everything goes when the cursor does");
    }
}
