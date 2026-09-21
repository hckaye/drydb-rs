//! The 26-byte file header.
//!
//! ```text
//! 0  4  magic "DRY\0"
//! 4  1  major version (1)
//! 5  1  minor version (4)
//! 6  2  page filter count (u16)
//! 8  4  page size (i32)
//! 12 2  table count (u16)
//! 14 4  page count (i32, back-patched)
//! 18 8  page directory position (i64, back-patched)
//! ```

use super::{
    checked_len, read_i32, read_i64, read_slice, read_u16, read_u8, FileOffset, HEADER_LEN, MAGIC,
    MAJOR_VERSION, MINOR_VERSION,
};
use crate::error::{Error, ErrorKind, Result};

/// Decoded file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Major format version. Always [`MAJOR_VERSION`] once validated.
    pub major_version: u8,
    /// Minor format version. Always [`MINOR_VERSION`] once validated.
    pub minor_version: u8,
    /// Number of page filter ids that follow the header.
    pub page_filter_count: u16,
    /// Page size the file was built with. Advisory: real pages carry their own length.
    pub page_size: usize,
    /// Number of table descriptors.
    pub table_count: u16,
    /// Number of pages, i.e. the number of page directory slots.
    pub page_count: u64,
    /// File offset of the page directory section.
    pub page_directory_position: FileOffset,
}

impl Header {
    /// Parses and validates the header prefix of `buf`.
    pub fn parse(buf: &[u8]) -> Result<Header> {
        let magic = read_slice(buf, 0, 4).map_err(|_| {
            Error::new(
                ErrorKind::UnsupportedFormat,
                "file shorter than the 26-byte header",
            )
        })?;
        if magic != MAGIC {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!("bad magic {magic:02x?}, expected \"DRY\\0\""),
            ));
        }
        if buf.len() < HEADER_LEN {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                "file shorter than the 26-byte header",
            ));
        }
        let major_version = read_u8(buf, 4)?;
        let minor_version = read_u8(buf, 5)?;
        if major_version != MAJOR_VERSION || minor_version != MINOR_VERSION {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!(
                    "storage format {major_version}.{minor_version} is not supported; \
                     this build reads {MAJOR_VERSION}.{MINOR_VERSION} only"
                ),
            ));
        }
        let page_filter_count = read_u16(buf, 6)?;
        let page_size = checked_len(read_i32(buf, 8)?, "page size")?;
        if page_size < super::PAGE_PREFIX_LEN {
            return Err(Error::corrupt(format!(
                "page size {page_size} is smaller than the {} byte page prefix",
                super::PAGE_PREFIX_LEN
            )));
        }
        let table_count = read_u16(buf, 12)?;
        let page_count = checked_len(read_i32(buf, 14)?, "page count")? as u64;
        let position = read_i64(buf, 18)?;
        if position <= 0 {
            return Err(Error::corrupt(format!(
                "page directory position {position} is not a valid file offset"
            )));
        }
        let page_directory_position = FileOffset::from_i64(position)?;

        Ok(Header {
            major_version,
            minor_version,
            page_filter_count,
            page_size,
            table_count,
            page_count,
            page_directory_position,
        })
    }

    /// Encodes the header, with `page_count` and `page_directory_position` as written
    /// before the back-patch.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = self.major_version;
        out[5] = self.minor_version;
        out[6..8].copy_from_slice(&self.page_filter_count.to_le_bytes());
        out[8..12].copy_from_slice(&(self.page_size as i32).to_le_bytes());
        out[12..14].copy_from_slice(&self.table_count.to_le_bytes());
        out[14..18].copy_from_slice(&(self.page_count as i32).to_le_bytes());
        out[18..26].copy_from_slice(&(self.page_directory_position.get() as i64).to_le_bytes());
        out
    }

    /// Byte span of the page directory section, checked for overflow.
    pub fn directory_span(&self) -> Result<(u64, u64)> {
        let start = self.page_directory_position.get();
        let len = self
            .page_count
            .checked_mul(super::DIRECTORY_SLOT_LEN as u64)
            .ok_or_else(|| Error::corrupt("page directory size overflows"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| Error::corrupt("page directory end overflows"))?;
        Ok((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            major_version: 1,
            minor_version: 4,
            page_filter_count: 0,
            page_size: 4096,
            table_count: 1,
            page_count: 3,
            page_directory_position: FileOffset::new(1024),
        }
    }

    #[test]
    fn round_trips() {
        let h = sample();
        assert_eq!(Header::parse(&h.encode()).unwrap(), h);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample().encode();
        bytes[0] = b'X';
        assert_eq!(
            Header::parse(&bytes).unwrap_err().kind(),
            ErrorKind::UnsupportedFormat
        );
    }

    #[test]
    fn rejects_other_versions() {
        for (major, minor) in [(1u8, 3u8), (1, 5), (2, 4), (0, 0)] {
            let mut h = sample();
            h.major_version = major;
            h.minor_version = minor;
            assert_eq!(
                Header::parse(&h.encode()).unwrap_err().kind(),
                ErrorKind::UnsupportedFormat,
                "{major}.{minor} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = sample().encode();
        for len in 0..HEADER_LEN {
            assert!(Header::parse(&bytes[..len]).is_err());
        }
    }

    #[test]
    fn rejects_negative_scalars() {
        let mut bytes = sample().encode();
        bytes[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            Header::parse(&bytes).unwrap_err().kind(),
            ErrorKind::CorruptData
        );

        let mut bytes = sample().encode();
        bytes[14..18].copy_from_slice(&(-5i32).to_le_bytes());
        assert_eq!(
            Header::parse(&bytes).unwrap_err().kind(),
            ErrorKind::CorruptData
        );

        let mut bytes = sample().encode();
        bytes[18..26].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(
            Header::parse(&bytes).unwrap_err().kind(),
            ErrorKind::CorruptData
        );
    }
}
