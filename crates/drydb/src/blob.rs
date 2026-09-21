//! Streaming reads of values stored on their own page.
//!
//! [`Table::get`](crate::Table::get) hands back a guard over the whole blob page, which
//! means the page is resident and charged to the budget for as long as the guard lives.
//! For a value too large to want resident, `BlobReader` reads straight from the file in
//! caller-sized chunks and never allocates the whole value.
//!
//! It bypasses the page cache, so it only works on an unfiltered database: a compressed
//! page has to be decoded as a whole before any of its bytes mean anything.

use std::io;
use std::sync::Arc;

use crate::error::{Error, ErrorKind, Result};
use crate::format::node::{NodeKind, PageView};
use crate::format::{PageOrdinal, PAGE_PREFIX_LEN};
use crate::io::{read_exact_at, read_up_to_at, PageSource};
use crate::store::PageStore;

/// A positional reader over one blob page's payload.
pub struct BlobReader {
    source: Arc<dyn PageSource>,
    /// Kept so the reader can reserve the buffers it makes, the way every other path in
    /// this crate does, reclaiming from the cache when it has to.
    store: Arc<PageStore>,
    page: PageOrdinal,
    start: u64,
    len: u64,
    pos: u64,
}

impl std::fmt::Debug for BlobReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobReader")
            .field("page", &self.page)
            .field("len", &self.len)
            .field("pos", &self.pos)
            .finish()
    }
}

impl BlobReader {
    pub(crate) fn open(store: &Arc<PageStore>, page: PageOrdinal) -> Result<BlobReader> {
        if store.has_filter() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "blob streaming needs an unfiltered database: a filtered page has to be \
                 decoded in full before any of its bytes can be read",
            ));
        }
        let offset = store.offset_of(page)?;
        let mut prefix = [0u8; PAGE_PREFIX_LEN];
        read_exact_at(store.source().as_ref(), &mut prefix, offset.get())?;

        let stored_len =
            crate::format::checked_len(crate::format::read_i32(&prefix, 0)?, "page length")?;
        if stored_len < PAGE_PREFIX_LEN {
            return Err(Error::corrupt(format!(
                "blob page length {stored_len} is below the {PAGE_PREFIX_LEN} byte prefix"
            ))
            .at_page(page.get()));
        }
        if stored_len > store.limits().max_stored_page_bytes {
            return Err(Error::corrupt(format!(
                "blob page length {stored_len} exceeds the {} byte limit",
                store.limits().max_stored_page_bytes
            ))
            .at_page(page.get()));
        }
        // A page ends where the next one starts, and the last one ends at the page
        // directory. Without this the declared length alone would decide how far a read
        // goes, and a page claiming more would hand back what comes after it.
        let end = offset
            .get()
            .checked_add(stored_len as u64)
            .ok_or_else(|| Error::corrupt("blob page end offset overflows"))?;
        if end > store.page_limit(page, offset.get())? {
            return Err(Error::corrupt(format!(
                "blob page spans {}..{end}, into what follows it",
                offset.get()
            ))
            .at_page(page.get()));
        }
        // Parse the prefix as if the page were exactly that long, to check kind and
        // entry count without reading the payload.
        let mut probe = prefix;
        probe[0..4].copy_from_slice(&(PAGE_PREFIX_LEN as i32).to_le_bytes());
        let view = PageView::parse(&probe).map_err(|e| e.at_page(page.get()))?;
        if view.kind() != NodeKind::Leaf || view.entry_count() != 0 {
            return Err(Error::corrupt(format!(
                "page {page} is referenced as a blob page but holds {} entries",
                view.entry_count()
            ))
            .at_page(page.get()));
        }

        Ok(BlobReader {
            source: Arc::clone(store.source()),
            store: Arc::clone(store),
            page,
            start: offset.get() + PAGE_PREFIX_LEN as u64,
            len: (stored_len - PAGE_PREFIX_LEN) as u64,
            pos: 0,
        })
    }

    /// Value length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The blob page.
    pub fn page(&self) -> PageOrdinal {
        self.page
    }

    /// Reads at most `buf.len()` bytes starting at `offset` within the value.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let remaining = self.len - offset;
        let want = (buf.len() as u64).min(remaining) as usize;
        let filled = read_up_to_at(self.source.as_ref(), &mut buf[..want], self.start + offset)?;
        if filled < want {
            return Err(Error::io(
                format!("blob page {} is truncated in the file", self.page),
                io::Error::from(io::ErrorKind::UnexpectedEof),
            )
            .at_page(self.page.get()));
        }
        Ok(filled)
    }

    /// Copies the whole value into `writer` using a fixed-size chunk buffer.
    pub fn copy_to(&self, writer: &mut dyn io::Write, chunk_size: usize) -> Result<u64> {
        let chunk_size = chunk_size.clamp(1, 1 << 20);
        // Reserved before it is allocated, like every other buffer this crate makes on a
        // caller's behalf. `chunk_size` is the caller's number.
        let _charge = self
            .store
            .reserve(chunk_size as u64 + crate::budget::BUFFER_OVERHEAD)?;
        let mut buf = vec![0u8; chunk_size];
        let mut offset = 0u64;
        while offset < self.len {
            let n = self.read_at(&mut buf, offset)?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .map_err(|e| Error::io("writing a blob to the destination failed", e))?;
            offset += n as u64;
        }
        Ok(offset)
    }
}

impl io::Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_at(buf, self.pos).map_err(io::Error::from)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl io::Seek for BlobReader {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let next = match pos {
            io::SeekFrom::Start(n) => n as i128,
            io::SeekFrom::End(n) => self.len as i128 + n as i128,
            io::SeekFrom::Current(n) => self.pos as i128 + n as i128,
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot seek before the start of the value",
            ));
        }
        if next > u64::MAX as i128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position does not fit in a 64 bit offset",
            ));
        }
        // A position past the end is kept as asked for, the way a file does it, so that
        // a later relative seek counts from where the caller put the position rather
        // than from the end. Reading there gives no bytes.
        self.pos = next as u64;
        Ok(self.pos)
    }
}
