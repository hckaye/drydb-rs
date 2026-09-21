//! Positional page I/O.
//!
//! The search path never shares a seek position: every read names its own offset, so
//! concurrent lookups do not serialise on a `Mutex<File>`. Short reads, `EINTR` and
//! premature EOF are handled here rather than in the B+Tree code.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};

/// A random-access byte source holding one database file.
pub trait PageSource: Send + Sync {
    /// Reads into `buf` starting at `offset`, returning the number of bytes read.
    /// A return of `0` means end of file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// Total size in bytes, when cheaply available.
    fn size(&self) -> io::Result<u64>;

    /// Short human-readable name used in diagnostics.
    fn describe(&self) -> String {
        "page source".to_string()
    }
}

/// Fills `buf` completely, retrying short reads and `EINTR`.
pub fn read_exact_at(source: &dyn PageSource, buf: &mut [u8], offset: u64) -> Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let at = offset
            .checked_add(filled as u64)
            .ok_or_else(|| Error::corrupt("read offset overflows"))?;
        match source.read_at(&mut buf[filled..], at) {
            Ok(0) => {
                return Err(Error::io(
                    format!(
                        "unexpected end of file: wanted {} bytes at offset {offset}, got {filled}",
                        buf.len()
                    ),
                    io::Error::from(io::ErrorKind::UnexpectedEof),
                )
                .at_offset(offset))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(
                    Error::io(format!("read of {} bytes at {offset} failed", buf.len()), e)
                        .at_offset(offset),
                )
            }
        }
    }
    Ok(())
}

/// Reads at most `buf.len()` bytes, stopping at end of file. Returns how many were
/// read.
pub fn read_up_to_at(source: &dyn PageSource, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let at = offset
            .checked_add(filled as u64)
            .ok_or_else(|| Error::corrupt("read offset overflows"))?;
        match source.read_at(&mut buf[filled..], at) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(Error::io(format!("read at {offset} failed"), e).at_offset(offset))
            }
        }
    }
    Ok(filled)
}

/// A regular file read through positional reads (`pread` / `ReadFile` with an offset).
#[derive(Debug)]
pub struct FileSource {
    file: File,
    path: String,
}

impl FileSource {
    /// Opens `path` read-only.
    pub fn open(path: impl AsRef<Path>) -> Result<FileSource> {
        let path = path.as_ref();
        let file = File::open(path)
            .map_err(|e| Error::io(format!("cannot open `{}`", path.display()), e))?;
        Ok(FileSource {
            file,
            path: path.display().to_string(),
        })
    }

    /// Wraps an already-open file.
    pub fn from_file(file: File) -> FileSource {
        FileSource {
            file,
            path: "<file>".to_string(),
        }
    }

    /// The underlying handle.
    pub fn file(&self) -> &File {
        &self.file
    }
}

#[cfg(unix)]
impl PageSource for FileSource {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn describe(&self) -> String {
        self.path.clone()
    }
}

#[cfg(windows)]
impl PageSource for FileSource {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        use std::os::windows::fs::FileExt;
        self.file.seek_read(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn describe(&self) -> String {
        self.path.clone()
    }
}

/// An in-memory database image.
#[derive(Debug)]
pub struct MemorySource {
    bytes: Arc<[u8]>,
}

impl MemorySource {
    /// Wraps an owned image.
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> MemorySource {
        MemorySource {
            bytes: bytes.into(),
        }
    }
}

impl PageSource for MemorySource {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let offset = match usize::try_from(offset) {
            Ok(o) if o <= self.bytes.len() => o,
            _ => return Ok(0),
        };
        let available = &self.bytes[offset..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        Ok(n)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn describe(&self) -> String {
        format!("in-memory image ({} bytes)", self.bytes.len())
    }
}

// The only `unsafe` in this crate: mapping a file. Everything else, including every
// byte decoded from a database, is safe Rust.
#[cfg(feature = "mmap")]
#[allow(unsafe_code)]
mod mmap_source {
    use super::*;

    /// A read-only memory mapping of the database file.
    ///
    /// Pages are still copied out of the mapping into owned, budgeted buffers, so no
    /// reference ever escapes into the mapping. What the mapping changes is the
    /// failure mode of the *copy*: if another process truncates or rewrites the file
    /// while it is mapped, the copy can fault (`SIGBUS`) instead of returning an I/O
    /// error, and resident pages are accounted by the OS rather than by the budget in
    /// [`OpenOptions`](crate::OpenOptions).
    #[derive(Debug)]
    pub struct MmapSource {
        map: memmap2::Mmap,
        path: String,
    }

    impl MmapSource {
        /// Maps `path` read-only.
        ///
        /// # Safety
        ///
        /// The caller must guarantee that the file is not modified or truncated by any
        /// process for as long as the returned source lives. This cannot be enforced
        /// by opening the file read-only.
        pub unsafe fn open(path: impl AsRef<std::path::Path>) -> Result<MmapSource> {
            let path = path.as_ref();
            let file = File::open(path)
                .map_err(|e| Error::io(format!("cannot open `{}`", path.display()), e))?;
            // SAFETY: delegated to this function's own contract.
            let map = unsafe { memmap2::Mmap::map(&file) }
                .map_err(|e| Error::io(format!("cannot map `{}`", path.display()), e))?;
            Ok(MmapSource {
                map,
                path: path.display().to_string(),
            })
        }
    }

    impl PageSource for MmapSource {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let offset = match usize::try_from(offset) {
                Ok(o) if o <= self.map.len() => o,
                _ => return Ok(0),
            };
            let available = &self.map[offset..];
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            Ok(n)
        }

        fn size(&self) -> io::Result<u64> {
            Ok(self.map.len() as u64)
        }

        fn describe(&self) -> String {
            format!("{} (mmap)", self.path)
        }
    }
}

#[cfg(feature = "mmap")]
pub use mmap_source::MmapSource;

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that returns one byte at a time, to exercise the short-read loop.
    struct Dribble(Vec<u8>);

    impl PageSource for Dribble {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let offset = offset as usize;
            if offset >= self.0.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[offset];
            Ok(1)
        }

        fn size(&self) -> io::Result<u64> {
            Ok(self.0.len() as u64)
        }
    }

    #[test]
    fn read_exact_at_handles_short_reads() {
        let src = Dribble(b"0123456789".to_vec());
        let mut buf = [0u8; 4];
        read_exact_at(&src, &mut buf, 3).unwrap();
        assert_eq!(&buf, b"3456");
    }

    #[test]
    fn read_exact_at_reports_eof() {
        let src = Dribble(b"0123".to_vec());
        let mut buf = [0u8; 8];
        let err = read_exact_at(&src, &mut buf, 0).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Io);
    }

    #[test]
    fn read_up_to_at_stops_at_eof() {
        let src = MemorySource::new(b"abcdef".to_vec());
        let mut buf = [0u8; 10];
        assert_eq!(read_up_to_at(&src, &mut buf, 2).unwrap(), 4);
        assert_eq!(&buf[..4], b"cdef");
    }

    #[test]
    fn memory_source_reads_past_end_as_eof() {
        let src = MemorySource::new(b"abc".to_vec());
        let mut buf = [0u8; 4];
        assert_eq!(src.read_at(&mut buf, 99).unwrap(), 0);
    }
}
