//! `PageRef`: the 16-byte pointer a secondary index stores in place of the value.
//!
//! Upstream writes the C# struct `readonly record struct PageRef(PageNumber, int, int)`
//! with `Unsafe.WriteUnaligned`, i.e. sequential layout, little-endian:
//!
//! ```text
//! 0  8  page ordinal (i64)
//! 8  4  start offset inside that page (i32)
//! 12 4  length in bytes (i32)
//! ```

use super::{read_i32, read_i64, PageLocalOffset, PageOrdinal};
use crate::error::{Error, Result};

/// Encoded size of a `PageRef`.
pub const PAGE_REF_LEN: usize = 16;

/// A pointer to a byte range inside another page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRef {
    /// Page holding the bytes.
    pub page: PageOrdinal,
    /// Offset of the first byte inside that page.
    pub start: PageLocalOffset,
    /// Number of bytes.
    pub length: u32,
}

impl PageRef {
    /// Decodes a `PageRef` from exactly [`PAGE_REF_LEN`] bytes.
    pub fn parse(buf: &[u8]) -> Result<PageRef> {
        if buf.len() < PAGE_REF_LEN {
            return Err(Error::corrupt(format!(
                "secondary index value is {} bytes, expected {PAGE_REF_LEN}",
                buf.len()
            )));
        }
        let ordinal = PageOrdinal::from_i64(read_i64(buf, 0)?)?
            .ok_or_else(|| Error::corrupt("secondary index PageRef points at no page"))?;
        let start = read_i32(buf, 8)?;
        let length = read_i32(buf, 12)?;
        if start < 0 || length < 0 {
            return Err(Error::corrupt(format!(
                "PageRef has negative start/length ({start}, {length})"
            )));
        }
        Ok(PageRef {
            page: ordinal,
            start: PageLocalOffset::new(start as u32),
            length: length as u32,
        })
    }

    /// Encodes the reference.
    pub fn encode(&self) -> [u8; PAGE_REF_LEN] {
        let mut out = [0u8; PAGE_REF_LEN];
        out[0..8].copy_from_slice(&(self.page.get() as i64).to_le_bytes());
        out[8..12].copy_from_slice(&(self.start.get() as i32).to_le_bytes());
        out[12..16].copy_from_slice(&(self.length as i32).to_le_bytes());
        out
    }

    /// Byte range the reference designates, checked against `page_len`.
    pub fn range(&self, page_len: usize) -> Result<std::ops::Range<usize>> {
        let start = self.start.get();
        let end = start
            .checked_add(self.length as usize)
            .ok_or_else(|| Error::corrupt("PageRef range overflows"))?;
        if end > page_len {
            return Err(Error::corrupt(format!(
                "PageRef range {start}..{end} exceeds page length {page_len}"
            ))
            .at_page(self.page.get()));
        }
        // A value never starts inside the page prefix, so a reference that does is
        // pointing at the header rather than at a record, and would hand the header back
        // as if it were the value. Whether it lines up with a record's value exactly is
        // more than a read can afford to check; `Database::verify` does that.
        if start < super::PAGE_PREFIX_LEN {
            return Err(Error::corrupt(format!(
                "PageRef starts at {start}, inside the {} byte page prefix",
                super::PAGE_PREFIX_LEN
            ))
            .at_page(self.page.get()));
        }
        Ok(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let r = PageRef {
            page: PageOrdinal::new(7),
            start: PageLocalOffset::new(28),
            length: 12,
        };
        assert_eq!(PageRef::parse(&r.encode()).unwrap(), r);
    }

    #[test]
    fn rejects_short_input() {
        assert!(PageRef::parse(&[0u8; 15]).is_err());
    }

    #[test]
    fn rejects_empty_ordinal() {
        let mut bytes = [0u8; PAGE_REF_LEN];
        bytes[0..8].copy_from_slice(&(-1i64).to_le_bytes());
        assert!(PageRef::parse(&bytes).is_err());
    }

    #[test]
    fn range_is_bounds_checked() {
        let r = PageRef {
            page: PageOrdinal::new(1),
            start: PageLocalOffset::new(30),
            length: 10,
        };
        assert!(r.range(39).is_err());
        assert_eq!(r.range(40).unwrap(), 30..40);
    }
}
