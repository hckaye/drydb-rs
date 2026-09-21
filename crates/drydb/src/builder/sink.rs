//! The output file: page writing, sibling patching and the page directory.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use super::temp::TempFile;
use crate::error::{Error, Result};
use crate::filter::PageFilter;
use crate::format::{PageOrdinal, DIRECTORY_SLOT_LEN, PAGE_PREFIX_LEN};

/// Collects page offsets without keeping one entry per page in memory forever.
///
/// Offsets are appended in flush order. Up to `threshold` bytes stay in memory; beyond
/// that they spill to a temporary file and are streamed back when the directory section
/// is written. Nothing in the write path ever indexes the directory by ordinal -- sibling
/// back-patching uses the previous page's file offset, which the level already knows.
pub(crate) struct DirectorySpool {
    buffer: Vec<u8>,
    file: Option<TempFile>,
    spilled_bytes: u64,
    count: u64,
    threshold: usize,
    dir: PathBuf,
}

impl std::fmt::Debug for DirectorySpool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectorySpool")
            .field("count", &self.count)
            .field("spilled_bytes", &self.spilled_bytes)
            .finish()
    }
}

impl DirectorySpool {
    pub(crate) fn new(dir: PathBuf, threshold: usize) -> DirectorySpool {
        DirectorySpool {
            buffer: Vec::new(),
            file: None,
            spilled_bytes: 0,
            count: 0,
            threshold: threshold.max(DIRECTORY_SLOT_LEN),
            dir,
        }
    }

    /// Records a page's file offset and returns its ordinal.
    pub(crate) fn push(&mut self, offset: u64) -> Result<PageOrdinal> {
        if self.count >= i32::MAX as u64 {
            return Err(Error::new(
                crate::error::ErrorKind::ValueTooLarge,
                "the format stores the page count as a signed 32-bit integer; this build \
                 would exceed it",
            ));
        }
        let ordinal = PageOrdinal::new(self.count);
        self.buffer
            .extend_from_slice(&(offset as i64).to_le_bytes());
        self.count += 1;
        if self.buffer.len() >= self.threshold {
            self.spill()?;
        }
        Ok(ordinal)
    }

    fn spill(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            self.file = Some(TempFile::create_in(&self.dir, "drydb-directory")?);
        }
        let file = self.file.as_mut().expect("just created");
        file.file_mut()
            .seek(SeekFrom::Start(self.spilled_bytes))
            .map_err(|e| Error::io("cannot position the page directory spool", e))?;
        file.file_mut()
            .write_all(&self.buffer)
            .map_err(|e| Error::io("cannot write the page directory spool", e))?;
        self.spilled_bytes += self.buffer.len() as u64;
        self.buffer.clear();
        Ok(())
    }

    /// Number of pages recorded.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// Streams the directory section into `out`.
    pub(crate) fn write_into(mut self, out: &mut dyn Write) -> Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.file_mut()
                .seek(SeekFrom::Start(0))
                .map_err(|e| Error::io("cannot rewind the page directory spool", e))?;
            let mut remaining = self.spilled_bytes;
            let mut chunk = vec![0u8; 256 * 1024];
            while remaining > 0 {
                let want = chunk.len().min(remaining as usize);
                file.file_mut()
                    .read_exact(&mut chunk[..want])
                    .map_err(|e| Error::io("cannot read the page directory spool", e))?;
                out.write_all(&chunk[..want])
                    .map_err(|e| Error::io("cannot write the page directory", e))?;
                remaining -= want as u64;
            }
        }
        out.write_all(&self.buffer)
            .map_err(|e| Error::io("cannot write the page directory", e))?;
        Ok(())
    }
}

/// The file a database is being built into.
pub(crate) struct PageSink {
    writer: BufWriter<File>,
    pos: u64,
    filter: Option<Arc<dyn PageFilter>>,
    pub(crate) directory: DirectorySpool,
    encoded: Vec<u8>,
}

impl std::fmt::Debug for PageSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageSink")
            .field("pos", &self.pos)
            .field("filter", &self.filter.as_ref().map(|f| f.id().to_string()))
            .field("directory", &self.directory)
            .finish()
    }
}

impl PageSink {
    pub(crate) fn new(
        file: File,
        filter: Option<Arc<dyn PageFilter>>,
        directory: DirectorySpool,
    ) -> PageSink {
        PageSink {
            writer: BufWriter::with_capacity(1 << 20, file),
            pos: 0,
            filter,
            directory,
            encoded: Vec::new(),
        }
    }

    /// Current write position.
    pub(crate) fn pos(&self) -> u64 {
        self.pos
    }

    /// Appends raw bytes (header, descriptors, the directory section).
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|e| Error::io("cannot write to the database file", e))?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Assigns the next page ordinal for a page starting at the current position.
    pub(crate) fn begin_page(&mut self) -> Result<(PageOrdinal, u64)> {
        let offset = self.pos;
        let ordinal = self.directory.push(offset)?;
        Ok((ordinal, offset))
    }

    /// Writes one page, applying the configured filter to everything after the
    /// 28-byte prefix and rewriting the stored length accordingly.
    pub(crate) fn write_page(&mut self, page: &[u8]) -> Result<()> {
        match &self.filter {
            None => self.write_raw(page),
            Some(filter) => {
                let mut payload = std::mem::take(&mut self.encoded);
                payload.clear();
                let result = filter.encode(&page[PAGE_PREFIX_LEN..], &mut payload);
                let outcome = result.and_then(|()| {
                    let total = PAGE_PREFIX_LEN
                        .checked_add(payload.len())
                        .filter(|t| *t <= i32::MAX as usize)
                        .ok_or_else(|| {
                            Error::new(
                                crate::error::ErrorKind::ValueTooLarge,
                                "a filtered page is larger than the format's 32-bit page length",
                            )
                        })?;
                    // The prefix stays raw so that sibling pointers can still be
                    // patched into a page that has already been written.
                    let mut prefix = [0u8; PAGE_PREFIX_LEN];
                    prefix.copy_from_slice(&page[..PAGE_PREFIX_LEN]);
                    prefix[0..4].copy_from_slice(&(total as i32).to_le_bytes());
                    self.write_raw_inner(&prefix)?;
                    self.write_raw_inner(&payload)
                });
                self.encoded = payload;
                outcome
            }
        }
    }

    fn write_raw_inner(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|e| Error::io("cannot write to the database file", e))?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Overwrites bytes already written, without moving the append position.
    ///
    /// Used for the two back-patches the format needs: a page's right sibling, and the
    /// header's page count and directory position.
    pub(crate) fn patch_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.writer
            .flush()
            .map_err(|e| Error::io("cannot flush the database file", e))?;
        let file = self.writer.get_mut();
        let saved = self.pos;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io("cannot seek in the database file", e))?;
        file.write_all(bytes)
            .map_err(|e| Error::io("cannot patch the database file", e))?;
        file.seek(SeekFrom::Start(saved))
            .map_err(|e| Error::io("cannot seek in the database file", e))?;
        Ok(())
    }

    /// Flushes buffered writes.
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .map_err(|e| Error::io("cannot flush the database file", e))
    }

    /// Writes the page directory section and returns its offset and page count.
    pub(crate) fn finish_directory(&mut self) -> Result<(u64, u64)> {
        let offset = self.pos;
        let directory = std::mem::replace(
            &mut self.directory,
            DirectorySpool::new(std::env::temp_dir(), DIRECTORY_SLOT_LEN),
        );
        let count = directory.count();
        let mut counting = CountingWriter {
            inner: &mut self.writer,
            written: 0,
        };
        directory.write_into(&mut counting)?;
        let written = counting.written;
        self.pos += written;
        Ok((offset, count))
    }
}

struct CountingWriter<'a> {
    inner: &'a mut BufWriter<File>,
    written: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
