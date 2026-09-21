//! B+Tree page decoding.
//!
//! A page is `page_length(i32)`, a 24-byte node header, the mandatory digest array,
//! the entry metadata area and finally the entry payloads:
//!
//! ```text
//! 0  4   page_length (i32, counts the whole page including this field)
//! 4  4   kind | flags (i32)
//! 8  4   entry_count (i32)
//! 12 8   left sibling ordinal (i64, -1 = none)
//! 20 8   right sibling ordinal (i64, -1 = none)
//! 28 ..  digest array (u64 per entry, or a padded Eytzinger tree)
//! ..     entry metadata (classic records, or compact u16 offsets)
//! ..     entry payloads (key bytes, value bytes / child ordinals)
//! ```
//!
//! [`PageView::parse`] checks the header and the area bounds; [`PageView::validate`]
//! walks every entry. The page store runs both when a page enters the cache, and each
//! accessor re-checks its own bounds, so a corrupt page can only produce
//! [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData).

use super::{
    read_i32, read_i64, read_u16, read_u64, PageOrdinal, COMPACT_OFFSET_MASK, COMPACT_OVERFLOW_BIT,
    DIGEST_BASE, ENTRY_COUNT_OFFSET, KIND_OFFSET, LEFT_SIBLING_OFFSET, MAX_COMPACT_PAGE_SIZE,
    OVERFLOW_SENTINEL, PAGE_PREFIX_LEN, RIGHT_SIBLING_OFFSET,
};
use crate::error::{Error, Result};

/// Low byte of the kind word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// Holds records (or, for a blob page, raw value bytes with `entry_count == 0`).
    Leaf,
    /// Holds separator keys and child ordinals.
    Internal,
}

const KIND_MASK: i32 = 0xFF;
const FLAG_LEGACY_KEY_DIGESTS: i32 = 1 << 8;
const FLAG_EYTZINGER: i32 = 1 << 9;
const FLAG_COMPACT_META: i32 = 1 << 10;
const FLAG_OMITTED_KEYS: i32 = 1 << 11;
const KNOWN_FLAGS: i32 = FLAG_EYTZINGER | FLAG_COMPACT_META | FLAG_OMITTED_KEYS;

/// Per-page layout flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeLayout {
    /// Digests are a `u64::MAX`-padded complete binary tree in BFS order.
    pub eytzinger: bool,
    /// Entry metadata is a `u16` offset array with derived lengths.
    pub compact_meta: bool,
    /// The page stores no key bytes; the exact digest array is the key column.
    pub omitted_keys: bool,
}

impl NodeLayout {
    /// The flag bits this layout sets in the kind word.
    pub fn flag_bits(self) -> i32 {
        (if self.eytzinger { FLAG_EYTZINGER } else { 0 })
            | (if self.compact_meta {
                FLAG_COMPACT_META
            } else {
                0
            })
            | (if self.omitted_keys {
                FLAG_OMITTED_KEYS
            } else {
                0
            })
    }

    /// Slots the digest array occupies for `entry_count` entries.
    pub fn digest_slots(self, entry_count: usize) -> usize {
        if self.eytzinger {
            complete_size(entry_count)
        } else {
            entry_count
        }
    }

    /// Bytes the leaf metadata area occupies.
    pub fn leaf_meta_size(self, entry_count: usize) -> Option<usize> {
        if self.compact_meta {
            let slots = entry_count
                .checked_add(1)?
                .checked_add(if self.omitted_keys { 0 } else { entry_count })?;
            slots.checked_mul(2)
        } else {
            entry_count.checked_mul(8)
        }
    }

    /// Bytes the internal metadata area occupies.
    pub fn internal_meta_size(self, entry_count: usize) -> Option<usize> {
        if self.compact_meta {
            if self.omitted_keys {
                Some(0)
            } else {
                entry_count.checked_add(1)?.checked_mul(2)
            }
        } else {
            entry_count.checked_mul(6)
        }
    }

    /// Bytes the digest array occupies.
    pub fn digest_area_size(self, entry_count: usize) -> Option<usize> {
        self.digest_slots(entry_count).checked_mul(8)
    }
}

/// The smallest complete-tree slot count (`2^k - 1`) that holds `count` digests.
pub fn complete_size(count: usize) -> usize {
    let mut m: usize = 1;
    while m < count.saturating_add(1) {
        match m.checked_shl(1) {
            Some(next) if next != 0 => m = next,
            _ => return usize::MAX,
        }
    }
    m - 1
}

/// Decoded node header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeHeader {
    /// Leaf or internal.
    pub kind: NodeKind,
    /// Layout flags.
    pub layout: NodeLayout,
    /// Number of entries on the page.
    pub entry_count: usize,
    /// Left sibling at the same level, if any.
    pub left_sibling: Option<PageOrdinal>,
    /// Right sibling at the same level, if any.
    pub right_sibling: Option<PageOrdinal>,
}

/// A parsed, bounds-checked view over one page's bytes.
#[derive(Debug, Clone, Copy)]
pub struct PageView<'a> {
    bytes: &'a [u8],
    header: NodeHeader,
    meta_base: usize,
    payload_base: usize,
}

/// Where a leaf entry's value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafValue {
    /// Bytes inside this page.
    Inline {
        /// Offset of the first value byte.
        offset: usize,
        /// Value length.
        len: usize,
    },
    /// Bytes on a dedicated blob page.
    Overflow {
        /// The blob page.
        page: PageOrdinal,
    },
}

/// One decoded leaf entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafEntry {
    /// Offset of the key bytes. Meaningless when `key_len` is zero.
    pub key_offset: usize,
    /// Key length. Zero on `OmittedKeys` pages, where the key comes from the digest.
    pub key_len: usize,
    /// The value.
    pub value: LeafValue,
}

/// One decoded internal entry (separator key plus child).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternalEntry {
    /// Offset of the separator key bytes.
    pub key_offset: usize,
    /// Separator key length. Zero on `OmittedKeys` pages.
    pub key_len: usize,
    /// Child page.
    pub child: PageOrdinal,
}

impl<'a> PageView<'a> {
    /// Parses the header and area bounds of a page.
    ///
    /// `bytes` must be exactly the page: its length must equal the stored page length.
    pub fn parse(bytes: &'a [u8]) -> Result<PageView<'a>> {
        if bytes.len() < PAGE_PREFIX_LEN {
            return Err(Error::corrupt(format!(
                "page is {} bytes, shorter than the {PAGE_PREFIX_LEN} byte prefix",
                bytes.len()
            )));
        }
        let stored_len = read_i32(bytes, 0)?;
        let stored_len = super::checked_len(stored_len, "page length")?;
        if stored_len != bytes.len() {
            return Err(Error::corrupt(format!(
                "page length field says {stored_len} but the page buffer holds {} bytes",
                bytes.len()
            )));
        }

        let kind_word = read_i32(bytes, KIND_OFFSET)?;
        let kind = match kind_word & KIND_MASK {
            0 => NodeKind::Leaf,
            1 => NodeKind::Internal,
            other => return Err(Error::corrupt(format!("unknown node kind {other}"))),
        };
        if kind_word & FLAG_LEGACY_KEY_DIGESTS != 0 {
            return Err(Error::corrupt(
                "page sets the retired HasKeyDigests flag (pre-1.4 layout)",
            ));
        }
        let unknown = kind_word & !(KIND_MASK | KNOWN_FLAGS);
        if unknown != 0 {
            return Err(Error::corrupt(format!(
                "page sets unknown node flags 0x{unknown:x}"
            )));
        }
        let layout = NodeLayout {
            eytzinger: kind_word & FLAG_EYTZINGER != 0,
            compact_meta: kind_word & FLAG_COMPACT_META != 0,
            omitted_keys: kind_word & FLAG_OMITTED_KEYS != 0,
        };
        if layout.omitted_keys && !layout.compact_meta {
            return Err(Error::corrupt("OmittedKeys requires CompactMeta"));
        }
        if layout.omitted_keys && layout.eytzinger {
            return Err(Error::corrupt(
                "OmittedKeys and EytzingerDigests cannot be combined",
            ));
        }
        if layout.compact_meta && stored_len > MAX_COMPACT_PAGE_SIZE {
            return Err(Error::corrupt(format!(
                "CompactMeta page is {stored_len} bytes, above the {MAX_COMPACT_PAGE_SIZE} byte limit"
            )));
        }

        let entry_count = super::checked_len(read_i32(bytes, ENTRY_COUNT_OFFSET)?, "entry count")?;
        let left_sibling = PageOrdinal::from_i64(read_i64(bytes, LEFT_SIBLING_OFFSET)?)?;
        let right_sibling = PageOrdinal::from_i64(read_i64(bytes, RIGHT_SIBLING_OFFSET)?)?;

        let digest_area = layout
            .digest_area_size(entry_count)
            .ok_or_else(|| Error::corrupt("digest area size overflows"))?;
        let meta_base = DIGEST_BASE
            .checked_add(digest_area)
            .ok_or_else(|| Error::corrupt("digest area end overflows"))?;
        if meta_base > bytes.len() {
            return Err(Error::corrupt(format!(
                "digest area ends at {meta_base}, past the {} byte page",
                bytes.len()
            )));
        }
        let meta_area = match kind {
            NodeKind::Leaf => layout.leaf_meta_size(entry_count),
            NodeKind::Internal => layout.internal_meta_size(entry_count),
        }
        .ok_or_else(|| Error::corrupt("metadata area size overflows"))?;
        let payload_base = meta_base
            .checked_add(meta_area)
            .ok_or_else(|| Error::corrupt("metadata area end overflows"))?;
        if payload_base > bytes.len() {
            return Err(Error::corrupt(format!(
                "metadata area ends at {payload_base}, past the {} byte page",
                bytes.len()
            )));
        }

        Ok(PageView {
            bytes,
            header: NodeHeader {
                kind,
                layout,
                entry_count,
                left_sibling,
                right_sibling,
            },
            meta_base,
            payload_base,
        })
    }

    /// The page bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The decoded header.
    pub fn header(&self) -> NodeHeader {
        self.header
    }

    /// Leaf or internal.
    pub fn kind(&self) -> NodeKind {
        self.header.kind
    }

    /// Layout flags.
    pub fn layout(&self) -> NodeLayout {
        self.header.layout
    }

    /// Number of entries.
    pub fn entry_count(&self) -> usize {
        self.header.entry_count
    }

    /// Left sibling at the same level.
    pub fn left_sibling(&self) -> Option<PageOrdinal> {
        self.header.left_sibling
    }

    /// Right sibling at the same level.
    pub fn right_sibling(&self) -> Option<PageOrdinal> {
        self.header.right_sibling
    }

    /// Payload bytes of a blob page: everything after the page prefix.
    pub fn blob_payload(&self) -> &'a [u8] {
        &self.bytes[PAGE_PREFIX_LEN..]
    }

    /// Walks every entry, checking that its metadata stays inside the page.
    pub fn validate(&self) -> Result<()> {
        match self.header.kind {
            NodeKind::Leaf => {
                for i in 0..self.header.entry_count {
                    self.leaf_entry(i)?;
                }
            }
            NodeKind::Internal => {
                for i in 0..self.header.entry_count {
                    self.internal_entry(i)?;
                }
            }
        }
        if self.header.layout.eytzinger {
            // The padded tree must be fully present.
            let slots = complete_size(self.header.entry_count);
            let end = DIGEST_BASE
                .checked_add(
                    slots
                        .checked_mul(8)
                        .ok_or_else(|| Error::corrupt("Eytzinger digest area size overflows"))?,
                )
                .ok_or_else(|| Error::corrupt("Eytzinger digest area end overflows"))?;
            if end > self.bytes.len() {
                return Err(Error::corrupt("Eytzinger digest area exceeds the page"));
            }
        }
        Ok(())
    }

    /// The digest stored for the `index`-th entry in sorted order.
    ///
    /// Only meaningful when the digests are stored sorted; `OmittedKeys` pages, which
    /// need this to rebuild keys, never use the Eytzinger layout.
    pub fn digest_at(&self, index: usize) -> Result<u64> {
        if self.header.layout.eytzinger {
            return Err(Error::corrupt("sorted digest access on an Eytzinger page"));
        }
        if index >= self.header.entry_count {
            return Err(Error::corrupt(format!(
                "digest index {index} is past the {} entries on this page",
                self.header.entry_count
            )));
        }
        read_u64(self.bytes, DIGEST_BASE + index * 8)
    }

    /// The raw digest area, one `u64` per slot.
    ///
    /// For a sorted page this is one digest per entry; for an Eytzinger page it is the
    /// padded complete tree in BFS order, so slot `i` is node `i + 1`.
    pub fn digest_slot(&self, slot: usize) -> Result<u64> {
        if slot >= self.digest_slot_count() {
            return Err(Error::corrupt(format!(
                "digest slot {slot} is past the {} slots on this page",
                self.digest_slot_count()
            )));
        }
        read_u64(self.bytes, DIGEST_BASE + slot * 8)
    }

    /// Number of `u64` slots in the digest area.
    pub fn digest_slot_count(&self) -> usize {
        (self.meta_base - DIGEST_BASE) / 8
    }

    /// Number of digests strictly less than `key_digest`, i.e. the index of the first
    /// entry that can match.
    pub fn lower_bound_rank(&self, key_digest: u64) -> Result<usize> {
        if self.header.layout.eytzinger {
            let complete = self.digest_slot_count();
            let mut i: usize = 1;
            while i <= complete {
                let digest = self.digest_slot(i - 1)?;
                // Branch-free descent: the padded tree is complete, so this always
                // terminates after `log2(complete + 1)` steps.
                i = 2 * i + usize::from(digest < key_digest);
            }
            Ok(i - complete - 1)
        } else {
            let mut lo = 0usize;
            let mut hi = self.header.entry_count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if self.digest_slot(mid)? < key_digest {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            Ok(lo)
        }
    }

    fn check_index(&self, index: usize) -> Result<()> {
        if index >= self.header.entry_count {
            return Err(Error::corrupt(format!(
                "entry index {index} is past the {} entries on this page",
                self.header.entry_count
            )));
        }
        Ok(())
    }

    fn payload_range(&self, offset: usize, len: usize) -> Result<()> {
        if offset < self.payload_base {
            return Err(Error::corrupt(format!(
                "entry payload offset {offset} points inside the metadata area (payloads start at {})",
                self.payload_base
            )));
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::corrupt("entry payload end overflows"))?;
        if end > self.bytes.len() {
            return Err(Error::corrupt(format!(
                "entry payload {offset}..{end} exceeds the {} byte page",
                self.bytes.len()
            )));
        }
        Ok(())
    }

    /// Decodes the `index`-th leaf entry.
    pub fn leaf_entry(&self, index: usize) -> Result<LeafEntry> {
        if self.header.kind != NodeKind::Leaf {
            return Err(Error::corrupt("leaf accessor used on an internal page"));
        }
        self.check_index(index)?;

        let (offset, key_len, value_len) = if self.header.layout.compact_meta {
            let raw = read_u16(self.bytes, self.meta_base + index * 2)?;
            let next = read_u16(self.bytes, self.meta_base + (index + 1) * 2)?;
            let offset = (raw & COMPACT_OFFSET_MASK) as usize;
            let next_offset = (next & COMPACT_OFFSET_MASK) as usize;
            let key_len = if self.header.layout.omitted_keys {
                0usize
            } else {
                read_u16(
                    self.bytes,
                    self.meta_base + (self.header.entry_count + 1) * 2 + index * 2,
                )? as usize
            };
            if next_offset < offset {
                return Err(Error::corrupt(format!(
                    "compact offsets are not monotonic at entry {index} ({offset} > {next_offset})"
                )));
            }
            let span = next_offset - offset;
            if key_len > span {
                return Err(Error::corrupt(format!(
                    "entry {index} declares a {key_len} byte key in a {span} byte payload"
                )));
            }
            let value_len = if raw & COMPACT_OVERFLOW_BIT != 0 {
                OVERFLOW_SENTINEL as usize
            } else {
                span - key_len
            };
            (offset, key_len, value_len)
        } else {
            let base = self.meta_base + index * 8;
            let offset = super::checked_len(read_i32(self.bytes, base)?, "entry payload offset")?;
            let key_len = read_u16(self.bytes, base + 4)? as usize;
            let value_len = read_u16(self.bytes, base + 6)? as usize;
            (offset, key_len, value_len)
        };

        if self.header.layout.omitted_keys && key_len != 0 {
            return Err(Error::corrupt(format!(
                "entry {index} on an OmittedKeys page declares a {key_len} byte key"
            )));
        }

        if value_len == OVERFLOW_SENTINEL as usize {
            self.payload_range(offset, key_len + 8)?;
            let ordinal = PageOrdinal::from_i64(read_i64(self.bytes, offset + key_len)?)?
                .ok_or_else(|| {
                    Error::corrupt(format!("entry {index} points at an empty blob page"))
                })?;
            Ok(LeafEntry {
                key_offset: offset,
                key_len,
                value: LeafValue::Overflow { page: ordinal },
            })
        } else {
            self.payload_range(offset, key_len + value_len)?;
            Ok(LeafEntry {
                key_offset: offset,
                key_len,
                value: LeafValue::Inline {
                    offset: offset + key_len,
                    len: value_len,
                },
            })
        }
    }

    /// Decodes the `index`-th internal entry.
    pub fn internal_entry(&self, index: usize) -> Result<InternalEntry> {
        if self.header.kind != NodeKind::Internal {
            return Err(Error::corrupt("internal accessor used on a leaf page"));
        }
        self.check_index(index)?;

        let (offset, key_len) = if self.header.layout.compact_meta {
            if self.header.layout.omitted_keys {
                // No metadata area at all: the payload is a dense array of child
                // ordinals.
                (self.meta_base + index * 8, 0usize)
            } else {
                let raw = read_u16(self.bytes, self.meta_base + index * 2)? as usize;
                let next = read_u16(self.bytes, self.meta_base + (index + 1) * 2)? as usize;
                if next < raw + 8 {
                    return Err(Error::corrupt(format!(
                        "internal entry {index} has a {} byte payload, too small for a child ordinal",
                        next.saturating_sub(raw)
                    )));
                }
                (raw, next - raw - 8)
            }
        } else {
            let base = self.meta_base + index * 6;
            let offset = super::checked_len(read_i32(self.bytes, base)?, "entry payload offset")?;
            let key_len = read_u16(self.bytes, base + 4)? as usize;
            (offset, key_len)
        };

        if self.header.layout.omitted_keys && key_len != 0 {
            return Err(Error::corrupt(format!(
                "internal entry {index} on an OmittedKeys page declares a {key_len} byte key"
            )));
        }

        self.payload_range(offset, key_len + 8)?;
        let child = PageOrdinal::from_i64(read_i64(self.bytes, offset + key_len)?)?
            .ok_or_else(|| Error::corrupt(format!("internal entry {index} has no child page")))?;
        Ok(InternalEntry {
            key_offset: offset,
            key_len,
            child,
        })
    }

    /// Key bytes of a leaf entry. Empty on `OmittedKeys` pages.
    pub fn leaf_key(&self, entry: &LeafEntry) -> Result<&'a [u8]> {
        super::read_slice(self.bytes, entry.key_offset, entry.key_len)
    }

    /// Key bytes of an internal entry. Empty on `OmittedKeys` pages.
    pub fn internal_key(&self, entry: &InternalEntry) -> Result<&'a [u8]> {
        super::read_slice(self.bytes, entry.key_offset, entry.key_len)
    }

    /// Inline value bytes.
    pub fn inline_value(&self, offset: usize, len: usize) -> Result<&'a [u8]> {
        super::read_slice(self.bytes, offset, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_size_matches_upstream() {
        assert_eq!(complete_size(0), 0);
        assert_eq!(complete_size(1), 1);
        assert_eq!(complete_size(2), 3);
        assert_eq!(complete_size(3), 3);
        assert_eq!(complete_size(4), 7);
        assert_eq!(complete_size(7), 7);
        assert_eq!(complete_size(8), 15);
    }

    fn page_prefix(kind: i32, entry_count: i32, len: usize) -> Vec<u8> {
        let mut page = vec![0u8; len];
        page[0..4].copy_from_slice(&(len as i32).to_le_bytes());
        page[4..8].copy_from_slice(&kind.to_le_bytes());
        page[8..12].copy_from_slice(&entry_count.to_le_bytes());
        page[12..20].copy_from_slice(&(-1i64).to_le_bytes());
        page[20..28].copy_from_slice(&(-1i64).to_le_bytes());
        page
    }

    #[test]
    fn parses_a_blob_page() {
        let mut page = page_prefix(0, 0, 40);
        page[28..].copy_from_slice(b"hello world!");
        let view = PageView::parse(&page).unwrap();
        assert_eq!(view.kind(), NodeKind::Leaf);
        assert_eq!(view.entry_count(), 0);
        assert_eq!(view.blob_payload(), b"hello world!");
        view.validate().unwrap();
    }

    #[test]
    fn rejects_page_length_mismatch() {
        let mut page = page_prefix(0, 0, 40);
        page[0..4].copy_from_slice(&41i32.to_le_bytes());
        assert!(PageView::parse(&page).is_err());
    }

    #[test]
    fn rejects_unknown_flags() {
        let page = page_prefix(1 << 12, 0, 40);
        assert!(PageView::parse(&page).is_err());
    }

    #[test]
    fn rejects_retired_digest_flag() {
        let page = page_prefix(1 << 8, 0, 40);
        assert!(PageView::parse(&page).is_err());
    }

    #[test]
    fn rejects_omitted_keys_without_compact_meta() {
        let page = page_prefix(FLAG_OMITTED_KEYS, 0, 40);
        assert!(PageView::parse(&page).is_err());
    }

    #[test]
    fn rejects_omitted_keys_with_eytzinger() {
        let page = page_prefix(
            FLAG_OMITTED_KEYS | FLAG_COMPACT_META | FLAG_EYTZINGER,
            0,
            40,
        );
        assert!(PageView::parse(&page).is_err());
    }

    #[test]
    fn rejects_entry_count_that_does_not_fit() {
        let page = page_prefix(0, 1000, 64);
        assert!(PageView::parse(&page).is_err());
    }

    /// Classic leaf with one entry: key "ab", value "xyz".
    fn classic_leaf() -> Vec<u8> {
        let entry_count = 1usize;
        let digest_area = entry_count * 8;
        let meta_area = entry_count * 8;
        let payload_base = 28 + digest_area + meta_area;
        let len = payload_base + 2 + 3;
        let mut page = page_prefix(0, entry_count as i32, len);
        page[28..36].copy_from_slice(&0x6162_0000_0000_0000u64.to_le_bytes());
        let meta = 28 + digest_area;
        page[meta..meta + 4].copy_from_slice(&(payload_base as i32).to_le_bytes());
        page[meta + 4..meta + 6].copy_from_slice(&2u16.to_le_bytes());
        page[meta + 6..meta + 8].copy_from_slice(&3u16.to_le_bytes());
        page[payload_base..payload_base + 2].copy_from_slice(b"ab");
        page[payload_base + 2..payload_base + 5].copy_from_slice(b"xyz");
        page
    }

    #[test]
    fn decodes_a_classic_leaf_entry() {
        let page = classic_leaf();
        let view = PageView::parse(&page).unwrap();
        view.validate().unwrap();
        let entry = view.leaf_entry(0).unwrap();
        assert_eq!(view.leaf_key(&entry).unwrap(), b"ab");
        match entry.value {
            LeafValue::Inline { offset, len } => {
                assert_eq!(view.inline_value(offset, len).unwrap(), b"xyz")
            }
            LeafValue::Overflow { .. } => panic!("expected inline"),
        }
        assert_eq!(view.digest_at(0).unwrap(), 0x6162_0000_0000_0000);
    }

    #[test]
    fn rejects_payload_pointing_into_metadata() {
        let mut page = classic_leaf();
        let meta = 28 + 8;
        page[meta..meta + 4].copy_from_slice(&4i32.to_le_bytes());
        let view = PageView::parse(&page).unwrap();
        assert!(view.validate().is_err());
    }

    #[test]
    fn rejects_payload_past_the_page_end() {
        let mut page = classic_leaf();
        let meta = 28 + 8;
        page[meta + 6..meta + 8].copy_from_slice(&9000u16.to_le_bytes());
        let view = PageView::parse(&page).unwrap();
        assert!(view.validate().is_err());
    }

    #[test]
    fn lower_bound_rank_on_sorted_digests() {
        let entry_count = 4usize;
        let digest_area = entry_count * 8;
        let meta_area = entry_count * 8;
        let payload_base = 28 + digest_area + meta_area;
        let mut page = page_prefix(0, entry_count as i32, payload_base);
        for (i, d) in [10u64, 20, 20, 30].iter().enumerate() {
            page[28 + i * 8..36 + i * 8].copy_from_slice(&d.to_le_bytes());
        }
        for i in 0..entry_count {
            let meta = 28 + digest_area + i * 8;
            page[meta..meta + 4].copy_from_slice(&(payload_base as i32).to_le_bytes());
        }
        let view = PageView::parse(&page).unwrap();
        assert_eq!(view.lower_bound_rank(5).unwrap(), 0);
        assert_eq!(view.lower_bound_rank(10).unwrap(), 0);
        assert_eq!(view.lower_bound_rank(15).unwrap(), 1);
        assert_eq!(view.lower_bound_rank(20).unwrap(), 1);
        assert_eq!(view.lower_bound_rank(25).unwrap(), 3);
        assert_eq!(view.lower_bound_rank(30).unwrap(), 3);
        assert_eq!(view.lower_bound_rank(31).unwrap(), 4);
    }
}
