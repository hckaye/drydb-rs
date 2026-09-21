//! Temporary files for spooling and for publishing a built database atomically.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A file that deletes itself on drop unless it has been kept.
#[derive(Debug)]
pub(crate) struct TempFile {
    path: PathBuf,
    file: Option<File>,
    keep: bool,
}

impl TempFile {
    /// Creates a new file in `dir` that no other process holds.
    ///
    /// The name is unique per process, per call and per nanosecond, and the file is
    /// opened with `create_new`, so an existing file is never truncated.
    ///
    /// `stem` is this crate's own word for what the file is for, never a caller's: a
    /// table name can hold a path separator or be longer than the file system allows,
    /// and a build should not fail part way through sorting because of what a table was
    /// called.
    pub(crate) fn create_in(dir: &Path, stem: &str) -> Result<TempFile> {
        debug_assert!(
            stem.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && stem.len() <= 32,
            "temp file stems are fixed strings from this crate, not caller input"
        );
        let pid = std::process::id();
        for attempt in 0..64 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(".{stem}-{pid}-{seq}-{nanos}-{attempt}.tmp"));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(TempFile {
                        path,
                        file: Some(file),
                        keep: false,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(Error::io(
                        format!("cannot create a temporary file in `{}`", dir.display()),
                        e,
                    ))
                }
            }
        }
        Err(Error::io(
            format!(
                "cannot find an unused temporary file name in `{}`",
                dir.display()
            ),
            io::Error::from(io::ErrorKind::AlreadyExists),
        ))
    }

    /// The file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The open handle.
    pub(crate) fn file(&self) -> &File {
        self.file.as_ref().expect("temp file handle taken")
    }

    /// The open handle, mutably.
    pub(crate) fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("temp file handle taken")
    }

    /// Renames the file into place and stops the automatic delete.
    ///
    /// On success the data is flushed and `fsync`ed first, and the containing directory
    /// is `fsync`ed afterwards on Unix. What that guarantees after a crash is a property
    /// of the filesystem, not of this crate: see `docs/compatibility.md`.
    pub(crate) fn publish(mut self, destination: &Path) -> Result<()> {
        {
            let file = self.file.as_mut().expect("temp file handle taken");
            file.sync_all()
                .map_err(|e| Error::io("cannot flush the built database to disk", e))?;
        }
        drop(self.file.take());
        std::fs::rename(&self.path, destination).map_err(|e| {
            Error::io(
                format!(
                    "cannot move `{}` into place at `{}`",
                    self.path.display(),
                    destination.display()
                ),
                e,
            )
        })?;
        self.keep = true;

        #[cfg(unix)]
        if let Some(dir) = destination.parent() {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            if let Ok(handle) = File::open(dir) {
                // A failure here means the rename may not survive a crash; it does not
                // make the file that is already in place wrong.
                let _ = handle.sync_all();
            }
        }
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Where temporary files go: next to the output when possible, else the system temp
/// directory.
pub(crate) fn scratch_dir(output: Option<&Path>, override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Some(parent) = output.and_then(|p| p.parent()) {
        if !parent.as_os_str().is_empty() {
            return parent.to_path_buf();
        }
        return PathBuf::from(".");
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn temp_files_are_removed_on_drop() {
        let dir = std::env::temp_dir();
        let path = {
            let mut t = TempFile::create_in(&dir, "drydb-test").unwrap();
            t.file_mut().write_all(b"x").unwrap();
            t.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[test]
    fn publish_moves_the_file() {
        let dir = std::env::temp_dir();
        let destination = dir.join(format!("drydb-publish-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&destination);
        let mut t = TempFile::create_in(&dir, "drydb-test").unwrap();
        t.file_mut().write_all(b"hello").unwrap();
        let temp_path = t.path().to_path_buf();
        t.publish(&destination).unwrap();
        assert!(!temp_path.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"hello");
        std::fs::remove_file(&destination).unwrap();
    }
}
