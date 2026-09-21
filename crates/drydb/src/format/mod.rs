//! On-disk format of DryDB storage version 1.4.
//!
//! Everything here decodes with explicit little-endian reads against a length-checked
//! slice. No byte range from the file is ever reinterpreted as a `repr(C)` struct, so a
//! hostile file can produce [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData)
//! but not undefined behaviour.
//!
//! The layout is transcribed from upstream commit
//! `6b175929491793948e63430c20c2d6f58300d97f` (`src/DryDB/DryDBCodec.Decode.cs`,
//! `src/DryDB/BTree/*`); `docs/compatibility.md` records the field table and the open
//! questions.

pub mod catalog;
pub mod header;
pub mod node;
pub mod pageref;

use crate::error::{Error, Result};

/// File magic: `DRY\0`.
pub const MAGIC: [u8; 4] = *b"DRY\0";
/// Supported major version.
pub const MAJOR_VERSION: u8 = 1;
/// Supported minor version.
pub const MINOR_VERSION: u8 = 4;
/// Size of the file header in bytes.
pub const HEADER_LEN: usize = 26;
/// Byte offset of the back-patched `page_count` field inside the header.
pub const PAGE_COUNT_FIELD_OFFSET: u64 = 14;

/// Per-page prefix holding the stored page length (`int32`).
pub const PAGE_LENGTH_LEN: usize = 4;
/// Per-page node header following the page length.
pub const NODE_HEADER_LEN: usize = 24;
/// Bytes every page starts with: page length plus node header.
///
/// Page filters leave this prefix uncompressed, which is what lets the builder
/// back-patch sibling pointers after a page has been written.
pub const PAGE_PREFIX_LEN: usize = PAGE_LENGTH_LEN + NODE_HEADER_LEN;

/// Offset of the right-sibling ordinal inside a page.
pub const RIGHT_SIBLING_OFFSET: usize = PAGE_PREFIX_LEN - 8;
/// Offset of the left-sibling ordinal inside a page.
pub const LEFT_SIBLING_OFFSET: usize = PAGE_PREFIX_LEN - 16;
/// Offset of the entry count inside a page.
pub const ENTRY_COUNT_OFFSET: usize = 8;
/// Offset of the node kind/flags word inside a page.
pub const KIND_OFFSET: usize = 4;
/// Offset at which a page's entry payload area can start at the earliest.
pub const DIGEST_BASE: usize = PAGE_PREFIX_LEN;

/// Encoded width of a page directory slot.
pub const DIRECTORY_SLOT_LEN: usize = 8;

/// Largest page size for which the builder emits the compact metadata layout.
///
/// Compact offsets are 15 bit wide (bit 15 flags an overflow entry) and the closing
/// sentinel offset may equal the page length, so both must fit in 15 bits.
pub const MAX_COMPACT_PAGE_SIZE: usize = 0x7FFF;

/// Inline value length that means "the payload is an 8-byte blob page ordinal".
pub const OVERFLOW_SENTINEL: u16 = u16::MAX;
/// Bit 15 of a compact leaf offset: the entry's value lives on a blob page.
pub const COMPACT_OVERFLOW_BIT: u16 = 0x8000;
/// Mask for the offset part of a compact leaf offset slot.
pub const COMPACT_OFFSET_MASK: u16 = 0x7FFF;

/// Dense page ordinal, assigned in flush order while building.
///
/// Every on-disk page pointer (roots, siblings, children, blob refs, secondary index
/// `PageRef`s) stores one of these; the page directory maps it to a byte offset.
/// `-1` on disk means "no page" and decodes to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageOrdinal(u64);

impl PageOrdinal {
    /// Wraps a validated ordinal.
    pub const fn new(value: u64) -> Self {
        PageOrdinal(value)
    }

    /// The ordinal as a `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Decodes the on-disk signed representation. `-1` is the empty sentinel.
    pub fn from_i64(value: i64) -> Result<Option<Self>> {
        match value {
            -1 => Ok(None),
            v if v >= 0 => Ok(Some(PageOrdinal(v as u64))),
            other => Err(Error::corrupt(format!("negative page ordinal {other}"))),
        }
    }

    /// The on-disk signed representation.
    pub fn to_i64(this: Option<Self>) -> i64 {
        match this {
            None => -1,
            Some(p) => p.0 as i64,
        }
    }
}

impl std::fmt::Display for PageOrdinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Byte offset inside the database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileOffset(u64);

impl FileOffset {
    /// Wraps a validated offset.
    pub const fn new(value: u64) -> Self {
        FileOffset(value)
    }

    /// The offset as a `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Decodes the on-disk signed representation, rejecting negatives.
    pub fn from_i64(value: i64) -> Result<Self> {
        if value < 0 {
            return Err(Error::corrupt(format!("negative file offset {value}")));
        }
        Ok(FileOffset(value as u64))
    }
}

impl std::fmt::Display for FileOffset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Byte offset inside a single page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageLocalOffset(u32);

impl PageLocalOffset {
    /// Wraps a validated page-local offset.
    pub const fn new(value: u32) -> Self {
        PageLocalOffset(value)
    }

    /// The offset as a `usize`.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

/// Reads a `u8` at `offset`, or reports corruption.
#[inline]
pub(crate) fn read_u8(buf: &[u8], offset: usize) -> Result<u8> {
    buf.get(offset)
        .copied()
        .ok_or_else(|| Error::corrupt("truncated read (u8)").at_offset(offset as u64))
}

macro_rules! read_le {
    ($name:ident, $ty:ty, $len:expr) => {
        #[inline]
        pub(crate) fn $name(buf: &[u8], offset: usize) -> Result<$ty> {
            let end = offset
                .checked_add($len)
                .ok_or_else(|| Error::corrupt("offset overflow").at_offset(offset as u64))?;
            let slice = buf.get(offset..end).ok_or_else(|| {
                Error::corrupt(concat!("truncated read (", stringify!($ty), ")"))
                    .at_offset(offset as u64)
            })?;
            let mut bytes = [0u8; $len];
            bytes.copy_from_slice(slice);
            Ok(<$ty>::from_le_bytes(bytes))
        }
    };
}

read_le!(read_u16, u16, 2);
read_le!(read_i32, i32, 4);
read_le!(read_i64, i64, 8);
read_le!(read_u64, u64, 8);

/// Reads a length-checked sub-slice.
#[inline]
pub(crate) fn read_slice(buf: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::corrupt("slice length overflow").at_offset(offset as u64))?;
    buf.get(offset..end)
        .ok_or_else(|| Error::corrupt("slice out of page bounds").at_offset(offset as u64))
}

/// Converts an on-disk `i32` count/length to `usize`, rejecting negatives.
#[inline]
pub(crate) fn checked_len(value: i32, what: &str) -> Result<usize> {
    if value < 0 {
        return Err(Error::corrupt(format!("negative {what}: {value}")));
    }
    Ok(value as usize)
}
