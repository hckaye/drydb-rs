//! Bottom-up B+Tree writing.
//!
//! Entries arrive in key order and are packed into a leaf until the next one does not
//! fit; the leaf is then written, its right-sibling pointer is patched into the page
//! before it, and its first key is promoted to the parent level as a separator. The
//! same rule applies to every level, so a tree of any height is written in one forward
//! pass with one page buffer per level.
//!
//! The packing arithmetic is a port of upstream `TreeBuilder`, including which entries
//! become overflow values; `docs/compatibility.md` lists the two places it deliberately
//! differs.

use crate::encoding::KeyEncoding;
use crate::error::{Error, ErrorKind, Result};
use crate::format::node::{complete_size, NodeLayout};
use crate::format::pageref::PageRef;
use crate::format::{
    PageLocalOffset, PageOrdinal, COMPACT_OVERFLOW_BIT, OVERFLOW_SENTINEL, PAGE_PREFIX_LEN,
    RIGHT_SIBLING_OFFSET,
};

use super::sink::PageSink;

/// The smallest page the upstream builder accepts.
pub const MIN_PAGE_SIZE: usize = PAGE_PREFIX_LEN + 32;

/// Deepest tree the writer will build.
///
/// A backstop, not a design limit: with a page holding at least two separators, the
/// height grows logarithmically and never comes close. Reaching it means a level failed
/// to reduce, and stopping is better than growing without bound.
const MAX_LEVELS: usize = 64;

struct Level<E> {
    payload: Vec<u8>,
    /// `(stored key length, inline payload length)` per entry.
    sizes: Vec<(usize, usize)>,
    overflow: Vec<Option<PageRef>>,
    digests: Vec<u64>,
    extras: Vec<E>,
    first_key: Option<Vec<u8>>,
    prev_ordinal: Option<PageOrdinal>,
    prev_offset: Option<u64>,
}

impl<E> Level<E> {
    fn new() -> Level<E> {
        Level {
            payload: Vec::new(),
            sizes: Vec::new(),
            overflow: Vec::new(),
            digests: Vec::new(),
            extras: Vec::new(),
            first_key: None,
            prev_ordinal: None,
            prev_offset: None,
        }
    }

    fn entry_count(&self) -> usize {
        self.sizes.len()
    }

    fn reset(&mut self) {
        self.payload.clear();
        self.sizes.clear();
        self.overflow.clear();
        self.digests.clear();
        self.extras.clear();
        self.first_key = None;
    }
}

/// Writes one B+Tree into a [`PageSink`].
///
/// `E` is whatever the caller wants to associate with each leaf entry; it is handed
/// back with the entry's [`PageRef`] once the leaf is written, which is how secondary
/// index records learn where their record landed.
/// How much an entry's extra holds while it waits for the page it belongs to.
///
/// The writer cannot hand an extra on until the page is written, because what it is
/// handed is where the entry landed. Holding them is therefore unavoidable; holding an
/// unbounded number of them is not, so the writer counts what they come to.
pub(crate) trait ExtraBytes {
    fn extra_bytes(&self) -> usize;
}

impl ExtraBytes for () {
    fn extra_bytes(&self) -> usize {
        0
    }
}

impl ExtraBytes for Vec<Vec<u8>> {
    fn extra_bytes(&self) -> usize {
        std::mem::size_of::<Vec<Vec<u8>>>()
            + self
                .iter()
                .map(|key| key.capacity() + std::mem::size_of::<Vec<u8>>())
                .sum::<usize>()
    }
}

pub(crate) struct TreeWriter<'a, E> {
    sink: &'a mut PageSink,
    page_size: usize,
    layout: NodeLayout,
    encoding: &'a dyn KeyEncoding,
    levels: Vec<Level<E>>,
    scratch: Vec<u8>,
    entries: u64,
    /// What the extras waiting for the leaf page come to, and what they may come to
    /// before the page is written early to be rid of them.
    pending_extra: usize,
    extra_budget: usize,
}

impl<E> std::fmt::Debug for TreeWriter<'_, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeWriter")
            .field("page_size", &self.page_size)
            .field("layout", &self.layout)
            .field("levels", &self.levels.len())
            .field("entries", &self.entries)
            .finish()
    }
}

impl<'a, E: ExtraBytes> TreeWriter<'a, E> {
    /// Creates a writer. `eytzinger` and the encoding decide the page layout, exactly
    /// as upstream does.
    pub(crate) fn new(
        sink: &'a mut PageSink,
        page_size: usize,
        eytzinger: bool,
        encoding: &'a dyn KeyEncoding,
        allow_omitted_keys: bool,
    ) -> Result<TreeWriter<'a, E>> {
        if page_size < MIN_PAGE_SIZE {
            return Err(Error::invalid(format!(
                "page size {page_size} is below the {MIN_PAGE_SIZE} byte minimum"
            )));
        }
        let layout = choose_layout(page_size, eytzinger, encoding, allow_omitted_keys);
        Ok(TreeWriter {
            sink,
            page_size,
            layout,
            encoding,
            levels: vec![Level::new()],
            scratch: Vec::new(),
            entries: 0,
            pending_extra: 0,
            extra_budget: usize::MAX,
        })
    }

    /// Bounds what the extras waiting for the current leaf page may come to.
    ///
    /// Reaching it writes the page as it stands, which hands the extras on. A page that
    /// is not full costs a little space; holding every key of every index until the page
    /// fills costs whatever the caller's keys come to.
    pub(crate) fn limit_pending_extra(&mut self, bytes: usize) {
        self.extra_budget = bytes;
    }

    fn digest_area(&self, entry_count: usize) -> usize {
        self.layout
            .digest_area_size(entry_count)
            .expect("entry counts are page bounded")
    }

    fn leaf_meta(&self, entry_count: usize) -> usize {
        self.layout
            .leaf_meta_size(entry_count)
            .expect("entry counts are page bounded")
    }

    fn internal_meta(&self, entry_count: usize) -> usize {
        self.layout
            .internal_meta_size(entry_count)
            .expect("entry counts are page bounded")
    }

    fn stored_key_len(&self, key_len: usize) -> usize {
        if self.layout.omitted_keys {
            0
        } else {
            key_len
        }
    }

    /// Appends one entry. Keys must arrive in the encoding's order.
    pub(crate) fn push(
        &mut self,
        key: &[u8],
        value: &[u8],
        extra: E,
        on_value_ref: &mut dyn FnMut(E, PageRef) -> Result<()>,
    ) -> Result<()> {
        if key.len() > u16::MAX as usize {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "key is {} bytes; the format stores key lengths as u16",
                    key.len()
                ),
            ));
        }
        if value.len() > i32::MAX as usize {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "value is {} bytes, past the format's 32-bit page length",
                    value.len()
                ),
            ));
        }
        let stored_key_len = self.stored_key_len(key.len());

        // An entry that cannot fit on an empty page even as an overflow reference can
        // never be written; fail before the packing loop can spin.
        let minimum =
            PAGE_PREFIX_LEN + self.leaf_meta(1) + self.digest_area(1) + stored_key_len + 8;
        if minimum > self.page_size {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "a {} byte key needs a page of at least {minimum} bytes, but the page size \
                     is {}",
                    key.len(),
                    self.page_size
                ),
            ));
        }

        if self.levels[0].entry_count() == 0 {
            self.levels[0].first_key = Some(key.to_vec());
        }

        let mut overflow = false;
        if self.inline_needs(stored_key_len, value.len()) > self.page_size {
            let overflow_needs = self.inline_needs(stored_key_len, 8);
            if overflow_needs > self.page_size && self.levels[0].entry_count() > 0 {
                self.rotate(0, true, on_value_ref)?;
                if self.levels[0].entry_count() == 0 {
                    self.levels[0].first_key = Some(key.to_vec());
                }
            }
            if self.inline_needs(stored_key_len, value.len()) > self.page_size {
                overflow = true;
            }
        }
        // A stored length of 0xFFFF is the overflow sentinel, and the classic metadata
        // layout only has 16 bits for it, so anything at or above that has to overflow.
        // Upstream only special-cases the exact sentinel, which truncates longer inline
        // values on large pages.
        if !overflow && value.len() >= OVERFLOW_SENTINEL as usize {
            overflow = true;
        }

        let digest = self.encoding.digest(key)?;
        let blob = if overflow {
            Some(self.write_blob_page(value)?)
        } else {
            None
        };

        let level = &mut self.levels[0];
        level.digests.push(digest);
        if !self.layout.omitted_keys {
            level.payload.extend_from_slice(key);
        }
        match blob {
            Some(ordinal) => {
                level
                    .payload
                    .extend_from_slice(&(ordinal.get() as i64).to_le_bytes());
                level.sizes.push((stored_key_len, 8));
                level.overflow.push(Some(PageRef {
                    page: ordinal,
                    start: PageLocalOffset::new(PAGE_PREFIX_LEN as u32),
                    length: value.len() as u32,
                }));
            }
            None => {
                level.payload.extend_from_slice(value);
                level.sizes.push((stored_key_len, value.len()));
                level.overflow.push(None);
            }
        }
        self.pending_extra += extra.extra_bytes();
        level.extras.push(extra);
        self.entries += 1;
        // The extras cannot be handed on until the page they belong to is written, so
        // the page is written as soon as they come to more than they may. It holds what
        // it holds; a page need not be full.
        if self.pending_extra > self.extra_budget && self.levels[0].entry_count() > 0 {
            self.rotate(0, true, on_value_ref)?;
        }
        Ok(())
    }

    fn inline_needs(&self, stored_key_len: usize, value_len: usize) -> usize {
        let level = &self.levels[0];
        let next = level.entry_count() + 1;
        PAGE_PREFIX_LEN
            + self.leaf_meta(next)
            + self.digest_area(next)
            + level.payload.len()
            + stored_key_len
            + value_len
    }

    /// Writes a value that does not fit inline onto its own page.
    fn write_blob_page(&mut self, value: &[u8]) -> Result<PageOrdinal> {
        let (ordinal, _offset) = self.sink.begin_page()?;
        let page_len = PAGE_PREFIX_LEN + value.len();
        let mut page = std::mem::take(&mut self.scratch);
        page.clear();
        page.reserve(page_len);
        page.extend_from_slice(&(page_len as i32).to_le_bytes());
        page.extend_from_slice(&0i32.to_le_bytes());
        page.extend_from_slice(&0i32.to_le_bytes());
        page.extend_from_slice(&(-1i64).to_le_bytes());
        page.extend_from_slice(&(-1i64).to_le_bytes());
        page.extend_from_slice(value);
        let outcome = self.sink.write_page(&page);
        self.scratch = page;
        outcome?;
        Ok(ordinal)
    }

    /// Writes the level's current page and, when `promote` is set, adds its first key
    /// as a separator in the parent level.
    fn rotate(
        &mut self,
        level: usize,
        promote: bool,
        on_value_ref: &mut dyn FnMut(E, PageRef) -> Result<()>,
    ) -> Result<()> {
        let (ordinal, offset) = self.sink.begin_page()?;
        self.flush_page(level, ordinal, on_value_ref)?;

        if let Some(prev_offset) = self.levels[level].prev_offset {
            let bytes = (ordinal.get() as i64).to_le_bytes();
            self.sink
                .patch_at(prev_offset + RIGHT_SIBLING_OFFSET as u64, &bytes)?;
        }

        if !promote {
            self.levels[level].reset();
            self.levels[level].prev_ordinal = Some(ordinal);
            self.levels[level].prev_offset = Some(offset);
            return Ok(());
        }

        let separator = self.levels[level]
            .first_key
            .clone()
            .ok_or_else(|| Error::corrupt("a promoted page has no first key"))?;
        let parent = level + 1;
        if parent >= self.levels.len() {
            if self.levels.len() >= MAX_LEVELS {
                return Err(Error::corrupt(format!(
                    "internal error: the tree reached {MAX_LEVELS} levels without the entry \
                     count falling"
                )));
            }
            self.levels.push(Level::new());
        }
        if self.levels[parent].entry_count() == 0 {
            self.levels[parent].first_key = Some(separator.clone());
        }

        let stored_key_len = self.stored_key_len(separator.len());
        if self.internal_needs(parent, stored_key_len) > self.page_size {
            self.rotate(parent, true, on_value_ref)?;
            if self.levels[parent].entry_count() == 0 {
                self.levels[parent].first_key = Some(separator.clone());
            }
        }

        let digest = self.encoding.digest(&separator)?;
        let omitted = self.layout.omitted_keys;
        let parent_level = &mut self.levels[parent];
        parent_level.digests.push(digest);
        if !omitted {
            parent_level.payload.extend_from_slice(&separator);
        }
        parent_level
            .payload
            .extend_from_slice(&(ordinal.get() as i64).to_le_bytes());
        parent_level.sizes.push((stored_key_len, 8));
        parent_level.overflow.push(None);

        self.levels[level].reset();
        self.levels[level].prev_ordinal = Some(ordinal);
        self.levels[level].prev_offset = Some(offset);
        Ok(())
    }

    fn internal_needs(&self, level: usize, stored_key_len: usize) -> usize {
        let state = &self.levels[level];
        let next = state.entry_count() + 1;
        PAGE_PREFIX_LEN
            + self.internal_meta(next)
            + self.digest_area(next)
            + state.payload.len()
            + stored_key_len
            + 8
    }

    fn flush_page(
        &mut self,
        level: usize,
        ordinal: PageOrdinal,
        on_value_ref: &mut dyn FnMut(E, PageRef) -> Result<()>,
    ) -> Result<()> {
        let is_leaf = level == 0;
        let state = &self.levels[level];
        let entry_count = state.entry_count();
        let digest_area = self.digest_area(entry_count);
        let meta_area = if is_leaf {
            self.leaf_meta(entry_count)
        } else {
            self.internal_meta(entry_count)
        };
        let payload_base = PAGE_PREFIX_LEN + digest_area + meta_area;
        let page_len = payload_base + state.payload.len();
        if page_len > self.page_size {
            return Err(Error::corrupt(format!(
                "internal error: built a {page_len} byte page for a {} byte page size",
                self.page_size
            )));
        }

        let mut page = std::mem::take(&mut self.scratch);
        page.clear();
        page.reserve(page_len);

        let kind_bits = (if is_leaf { 0i32 } else { 1i32 }) | self.layout.flag_bits();
        page.extend_from_slice(&(page_len as i32).to_le_bytes());
        page.extend_from_slice(&kind_bits.to_le_bytes());
        page.extend_from_slice(&(entry_count as i32).to_le_bytes());
        page.extend_from_slice(&PageOrdinal::to_i64(state.prev_ordinal).to_le_bytes());
        page.extend_from_slice(&(-1i64).to_le_bytes());

        if self.layout.eytzinger {
            let slots = complete_size(entry_count);
            let mut scattered = vec![u64::MAX; slots];
            scatter_eytzinger(&state.digests, &mut scattered);
            for digest in &scattered {
                page.extend_from_slice(&digest.to_le_bytes());
            }
        } else {
            for digest in &state.digests {
                page.extend_from_slice(&digest.to_le_bytes());
            }
        }
        debug_assert_eq!(page.len(), PAGE_PREFIX_LEN + digest_area);

        if self.layout.compact_meta {
            if is_leaf || !self.layout.omitted_keys {
                let mut offset = payload_base;
                for (i, (key_len, payload_len)) in state.sizes.iter().enumerate() {
                    let mut slot = offset as u16;
                    if is_leaf && state.overflow[i].is_some() {
                        slot |= COMPACT_OVERFLOW_BIT;
                    }
                    page.extend_from_slice(&slot.to_le_bytes());
                    offset += key_len + payload_len;
                }
                page.extend_from_slice(&(offset as u16).to_le_bytes());
                if is_leaf && !self.layout.omitted_keys {
                    for (key_len, _) in &state.sizes {
                        page.extend_from_slice(&(*key_len as u16).to_le_bytes());
                    }
                }
            }
            // An internal page with omitted keys has no metadata area at all: the
            // payload is a dense array of child ordinals.
        } else {
            let mut offset = payload_base;
            for (i, (key_len, payload_len)) in state.sizes.iter().enumerate() {
                page.extend_from_slice(&(offset as i32).to_le_bytes());
                page.extend_from_slice(&(*key_len as u16).to_le_bytes());
                if is_leaf {
                    let stored = if state.overflow[i].is_some() {
                        OVERFLOW_SENTINEL
                    } else {
                        *payload_len as u16
                    };
                    page.extend_from_slice(&stored.to_le_bytes());
                }
                offset += key_len + payload_len;
            }
        }
        debug_assert_eq!(page.len(), payload_base);

        page.extend_from_slice(&state.payload);
        debug_assert_eq!(page.len(), page_len);

        let outcome = self.sink.write_page(&page);
        self.scratch = page;
        outcome?;

        if is_leaf {
            let state = &mut self.levels[level];
            let mut offset = payload_base;
            let extras = std::mem::take(&mut state.extras);
            self.pending_extra = 0;
            let sizes = state.sizes.clone();
            let overflow = state.overflow.clone();
            for (i, extra) in extras.into_iter().enumerate() {
                let (key_len, payload_len) = sizes[i];
                let reference = match overflow[i] {
                    Some(r) => r,
                    None => PageRef {
                        page: ordinal,
                        start: PageLocalOffset::new((offset + key_len) as u32),
                        length: payload_len as u32,
                    },
                };
                on_value_ref(extra, reference)?;
                offset += key_len + payload_len;
            }
        }
        Ok(())
    }

    /// Writes the remaining levels and returns the root page.
    ///
    /// A tree with no entries still gets a real, empty leaf page as its root. The C#
    /// builder writes `-1` there instead, which its own reader cannot open.
    pub(crate) fn finish(
        mut self,
        on_value_ref: &mut dyn FnMut(E, PageRef) -> Result<()>,
    ) -> Result<PageOrdinal> {
        loop {
            let mut flushed = false;
            let mut level = 0;
            while level + 1 < self.levels.len() {
                if self.levels[level].entry_count() > 0 {
                    self.rotate(level, true, on_value_ref)?;
                    flushed = true;
                }
                level += 1;
            }
            if !flushed {
                break;
            }
        }

        let top = self.levels.len() - 1;
        if self.levels[top].entry_count() > 0 || self.levels[top].prev_ordinal.is_none() {
            self.rotate(top, false, on_value_ref)?;
        }
        self.levels[top]
            .prev_ordinal
            .ok_or_else(|| Error::corrupt("tree writer produced no root page"))
    }
}

/// The page layout a build uses, given its settings and key encoding.
///
/// `allow_omitted_keys` is off for the composite keys of a non-unique index, where the
/// digest covers only the source key and dropping the key bytes would make rows with the
/// same index key indistinguishable on the page.
pub(crate) fn choose_layout(
    page_size: usize,
    eytzinger: bool,
    encoding: &dyn KeyEncoding,
    allow_omitted_keys: bool,
) -> NodeLayout {
    let compact_meta = page_size <= crate::format::MAX_COMPACT_PAGE_SIZE;
    NodeLayout {
        eytzinger,
        compact_meta,
        // Exact digests are a bijective image of the keys, so the key bytes are
        // redundant. Never with Eytzinger: enumeration needs sorted-order digests.
        omitted_keys: allow_omitted_keys
            && compact_meta
            && !eytzinger
            && encoding.is_digest_exact(),
    }
}

/// Rejects a key that would make the tree impossible to build.
///
/// Two conditions. The entry has to fit on an empty leaf, even as an overflow reference,
/// or there is nowhere to put it. And an internal page has to hold two separators of this
/// size: a level whose pages fit one separator each promotes as many entries as it
/// received, so the height would grow without the entry count ever falling.
///
/// The second check uses the same key twice, so it is conservative: a page that could
/// hold this key next to a shorter one is still refused. It only bites on page sizes
/// close to the minimum.
pub(crate) fn check_key_fits(page_size: usize, layout: NodeLayout, key_len: usize) -> Result<()> {
    let stored_key_len = if layout.omitted_keys { 0 } else { key_len };
    let digest_one = layout.digest_area_size(1).unwrap_or(usize::MAX);
    let digest_two = layout.digest_area_size(2).unwrap_or(usize::MAX);
    let leaf_meta = layout.leaf_meta_size(1).unwrap_or(usize::MAX);
    let internal_meta = layout.internal_meta_size(2).unwrap_or(usize::MAX);

    let leaf = PAGE_PREFIX_LEN
        .saturating_add(leaf_meta)
        .saturating_add(digest_one)
        .saturating_add(stored_key_len)
        .saturating_add(8);
    if leaf > page_size {
        return Err(Error::new(
            ErrorKind::ValueTooLarge,
            format!(
                "a {key_len} byte key needs a leaf page of at least {leaf} bytes, but the page \
                 size is {page_size}"
            ),
        ));
    }

    let two_separators = PAGE_PREFIX_LEN
        .saturating_add(internal_meta)
        .saturating_add(digest_two)
        .saturating_add(2usize.saturating_mul(stored_key_len.saturating_add(8)));
    if two_separators > page_size {
        return Err(Error::new(
            ErrorKind::ValueTooLarge,
            format!(
                "a {key_len} byte key needs an internal page of at least {two_separators} bytes \
                 to hold two separators, but the page size is {page_size}; a tree whose internal \
                 pages hold one separator each cannot be built"
            ),
        ));
    }
    Ok(())
}

/// Scatters sorted digests into Eytzinger (BFS) slots, padding with `u64::MAX`.
fn scatter_eytzinger(sorted: &[u64], slots: &mut [u64]) {
    fn fill(sorted: &[u64], slots: &mut [u64], node: usize, next: &mut usize) {
        if node > slots.len() {
            return;
        }
        fill(sorted, slots, node * 2, next);
        slots[node - 1] = if *next < sorted.len() {
            let value = sorted[*next];
            *next += 1;
            value
        } else {
            u64::MAX
        };
        fill(sorted, slots, node * 2 + 1, next);
    }
    let mut next = 0usize;
    fill(sorted, slots, 1, &mut next);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eytzinger_scatter_matches_upstream_shape() {
        // Sorted [10,20,30] into a complete tree of 3 slots: root is the median.
        let mut slots = vec![0u64; complete_size(3)];
        scatter_eytzinger(&[10, 20, 30], &mut slots);
        assert_eq!(slots, vec![20, 10, 30]);
    }

    #[test]
    fn eytzinger_scatter_pads_with_max() {
        let mut slots = vec![0u64; complete_size(2)];
        scatter_eytzinger(&[10, 20], &mut slots);
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0], 20);
        assert_eq!(slots[1], 10);
        assert_eq!(slots[2], u64::MAX);
    }
}
