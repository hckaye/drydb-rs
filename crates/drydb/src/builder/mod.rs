//! Building a DryDB 1.4 file.
//!
//! Input arrives in any order through [`DatabaseBuilder::append`], is sorted with a
//! bounded memory buffer that spills to temporary files, and is written out in one
//! forward pass. Secondary index record ids run from zero in primary key order within
//! each index key, matching what the C# builder produces. Secondary index records are spooled while the primary tree is written,
//! because that is when each record's [`PageRef`] becomes known, and then sorted and
//! written the same way. A build of a dataset far larger than memory stays inside the
//! sort buffer you configured.
//!
//! The result is written to a temporary file and renamed into place only after it is
//! complete, so a failed or cancelled build never leaves a half-written database where
//! the old one was.

mod sink;
mod spool;
mod temp;
mod tree;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::encoding::{DuplicateKeyEncoding, KeyEncoding};
use crate::error::{Error, ErrorKind, Result};
use crate::filter::PageFilter;
use crate::format::catalog::{encode_index_descriptor, ValueKind};
use crate::format::header::Header;
use crate::format::pageref::PageRef;
use crate::format::{FileOffset, PAGE_COUNT_FIELD_OFFSET};

use sink::{DirectorySpool, PageSink};
use spool::RecordSpool;
use temp::{scratch_dir, TempFile};
use tree::{check_key_fits, choose_layout, TreeWriter};

pub use tree::MIN_PAGE_SIZE;

/// Default page size, matching the upstream builder.
pub const DEFAULT_PAGE_SIZE: usize = 4096;
/// Default number of record bytes buffered before the sorter spills to disk.
pub const DEFAULT_SORT_BUFFER: usize = 64 << 20;
/// Default number of page directory bytes buffered before spilling.
const DIRECTORY_SPILL_THRESHOLD: usize = 1 << 20;

/// Derives a secondary index key from a record.
pub type SecondaryKeyFn = Box<dyn Fn(&[u8], &[u8]) -> Result<Vec<u8>> + Send + Sync>;

/// Identifies a table inside a [`DatabaseBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableId(usize);

struct SecondarySpec {
    name: String,
    unique: bool,
    encoding: Arc<dyn KeyEncoding>,
    key_fn: SecondaryKeyFn,
}

struct TableSpec {
    name: String,
    encoding: Arc<dyn KeyEncoding>,
    secondaries: Vec<SecondarySpec>,
    spool: RecordSpool,
}

/// What a set of tables is holding in their sorters.
fn buffered_bytes(tables: &std::collections::VecDeque<TableSpec>) -> usize {
    tables
        .iter()
        .map(|table| table.spool.buffered_bytes())
        .sum()
}

/// Keeps what a set of sorters holds between them inside one buffer, by writing out
/// whichever holds the most. The buffer is what the build may hold, not what each sorter
/// may hold.
fn keep_inside(spools: &mut [RecordSpool], budget: usize) -> Result<()> {
    loop {
        let total: usize = spools.iter().map(|spool| spool.buffered_bytes()).sum();
        if total <= budget {
            return Ok(());
        }
        let Some(fullest) = spools.iter_mut().max_by_key(|spool| spool.buffered_bytes()) else {
            return Ok(());
        };
        if fullest.buffered_bytes() == 0 {
            return Ok(());
        }
        fullest.flush()?;
    }
}

/// Builds a database file.
pub struct DatabaseBuilder {
    page_size: usize,
    /// What every table's sorter holds between them, kept inside `sort_buffer`.
    buffered: usize,
    eytzinger_digests: bool,
    filter: Option<Arc<dyn PageFilter>>,
    sort_buffer: usize,
    temp_dir: Option<PathBuf>,
    tables: Vec<TableSpec>,
}

impl std::fmt::Debug for DatabaseBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseBuilder")
            .field("page_size", &self.page_size)
            .field("eytzinger_digests", &self.eytzinger_digests)
            .field("filter", &self.filter.as_ref().map(|f| f.id().to_string()))
            .field(
                "tables",
                &self.tables.iter().map(|t| &t.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for DatabaseBuilder {
    fn default() -> Self {
        DatabaseBuilder {
            page_size: DEFAULT_PAGE_SIZE,
            buffered: 0,
            eytzinger_digests: false,
            filter: None,
            sort_buffer: DEFAULT_SORT_BUFFER,
            temp_dir: None,
            tables: Vec::new(),
        }
    }
}

impl DatabaseBuilder {
    /// A builder with default settings.
    pub fn new() -> DatabaseBuilder {
        DatabaseBuilder::default()
    }

    /// Sets the page size.
    ///
    /// At or below 32767 bytes the builder uses the compact metadata layout, and with
    /// an exact-digest encoding it also drops key bytes from the pages. Larger pages
    /// fall back to the classic fixed-size metadata records.
    pub fn page_size(mut self, bytes: usize) -> Result<Self> {
        if bytes < MIN_PAGE_SIZE {
            return Err(Error::invalid(format!(
                "page size {bytes} is below the {MIN_PAGE_SIZE} byte minimum"
            )));
        }
        if bytes > i32::MAX as usize {
            return Err(Error::invalid(
                "page size does not fit in the format's i32 field",
            ));
        }
        self.page_size = bytes;
        Ok(self)
    }

    /// Stores each page's digest array as a padded complete binary tree in Eytzinger
    /// order instead of sorted order.
    ///
    /// It costs up to twice the digest area, and it turns off the key-omitting layout
    /// (enumeration needs digests in sorted order), so it is a trade, not a free win.
    pub fn eytzinger_digests(mut self, enabled: bool) -> Self {
        self.eytzinger_digests = enabled;
        self
    }

    /// Applies a page filter to every page payload.
    pub fn page_filter(mut self, filter: Arc<dyn PageFilter>) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Bytes of records buffered before the sorters spill to a temporary file.
    ///
    /// The figure is for the whole build, shared by every table: the table holding the
    /// most gives its buffer up when the total goes over. It covers what a record costs
    /// to hold, not only its key and value, so a build of empty records spills as
    /// readily as one of large ones.
    pub fn sort_buffer(mut self, bytes: usize) -> Self {
        self.sort_buffer = bytes.max(64 * 1024);
        // Tables already declared hold a sorter of their own, so the new figure has to
        // reach them: a setting that governed only the tables declared after it would
        // quietly do nothing to the ones already there.
        for table in &mut self.tables {
            table.spool.set_budget(self.sort_buffer);
        }
        self
    }

    /// Where temporary files go. Defaults to the system temporary directory for sort
    /// spills, and to the output file's directory for the file being built.
    ///
    /// Tables already declared use the new directory for the file they have yet to
    /// create. A table that has already spilled keeps the file it opened, since its
    /// records are in it, so set this before appending rows if the location matters.
    pub fn temp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(dir.into());
        let dir = scratch_dir(None, self.temp_dir.as_deref());
        for table in &mut self.tables {
            table.spool.set_dir(dir.clone());
        }
        self
    }

    /// Declares a table with the given primary key encoding.
    pub fn create_table(
        &mut self,
        name: impl Into<String>,
        encoding: Arc<dyn KeyEncoding>,
    ) -> Result<TableId> {
        let name = name.into();
        if name.is_empty() {
            return Err(Error::invalid("table names cannot be empty"));
        }
        if self.tables.iter().any(|t| t.name == name) {
            return Err(Error::invalid(format!(
                "table `{name}` is already declared"
            )));
        }
        let dir = scratch_dir(None, self.temp_dir.as_deref());
        // A fixed stem, not the table's name: a name can hold a path separator or be
        // longer than the file system allows a file name to be, and the temporary file
        // is an implementation detail either way.
        let spool = RecordSpool::new(Arc::clone(&encoding), dir, "drydb-rows", self.sort_buffer);
        self.tables.push(TableSpec {
            name,
            encoding,
            secondaries: Vec::new(),
            spool,
        });
        Ok(TableId(self.tables.len() - 1))
    }

    /// Declares a secondary index on a table.
    ///
    /// `key_fn` receives the record's key and value and returns the index key. It runs
    /// once per record while the primary tree is written.
    pub fn add_secondary_index(
        &mut self,
        table: TableId,
        name: impl Into<String>,
        unique: bool,
        encoding: Arc<dyn KeyEncoding>,
        key_fn: SecondaryKeyFn,
    ) -> Result<()> {
        let name = name.into();
        let spec = self
            .tables
            .get_mut(table.0)
            .ok_or_else(|| Error::invalid("unknown table handle"))?;
        if spec.secondaries.iter().any(|s| s.name == name) {
            return Err(Error::invalid(format!(
                "table `{}` already has an index named `{name}`",
                spec.name
            )));
        }
        if spec.spool.len() > 0 {
            return Err(Error::invalid(
                "secondary indexes have to be declared before the first row is appended",
            ));
        }
        spec.secondaries.push(SecondarySpec {
            name,
            unique,
            encoding,
            key_fn,
        });
        Ok(())
    }

    /// Appends a row. Rows may arrive in any order.
    pub fn append(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()> {
        let spec = self
            .tables
            .get_mut(table.0)
            .ok_or_else(|| Error::invalid("unknown table handle"))?;
        if key.len() > u16::MAX as usize {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "key is {} bytes; the format stores key lengths as u16",
                    key.len()
                ),
            ));
        }
        // The same check the tree writer makes at build time, done here so the caller
        // learns which row is the problem.
        let layout = choose_layout(
            self.page_size,
            self.eytzinger_digests,
            &*spec.encoding,
            true,
        );
        check_key_fits(self.page_size, layout, key.len())?;
        let before = spec.spool.buffered_bytes();
        let pushed = spec.spool.push(key, value);
        let after = spec.spool.buffered_bytes();
        self.buffered = self.buffered.saturating_sub(before).saturating_add(after);
        pushed?;
        self.keep_inside_the_sort_buffer()
    }

    /// Spills until every table's sorter together holds no more than the sort buffer.
    ///
    /// The figure is what the build may hold, not what each table may hold: giving every
    /// table a buffer of its own meant a build of sixty-four tables held sixty-four
    /// times the number the caller set.
    fn keep_inside_the_sort_buffer(&mut self) -> Result<()> {
        while self.buffered > self.sort_buffer {
            let Some(fullest) = self
                .tables
                .iter_mut()
                .max_by_key(|table| table.spool.buffered_bytes())
            else {
                break;
            };
            if fullest.spool.buffered_bytes() == 0 {
                break;
            }
            fullest.spool.flush()?;
            self.buffered = self
                .tables
                .iter()
                .map(|table| table.spool.buffered_bytes())
                .sum();
        }
        Ok(())
    }

    /// Builds the database and moves it into place at `path`.
    pub fn build_to_file(self, path: impl AsRef<Path>) -> Result<BuildReport> {
        let path = path.as_ref();
        let dir = scratch_dir(Some(path), self.temp_dir.as_deref());
        let temp = TempFile::create_in(&dir, "drydb-build")?;
        let file = temp
            .file()
            .try_clone()
            .map_err(|e| Error::io("cannot duplicate the output file handle", e))?;
        let report = self.build_into(file, dir)?;
        temp.publish(path)?;
        Ok(report)
    }

    /// Builds the database into memory. Intended for tests and small fixtures.
    pub fn build_to_vec(self) -> Result<Vec<u8>> {
        let dir = scratch_dir(None, self.temp_dir.as_deref());
        let temp = TempFile::create_in(&dir, "drydb-build")?;
        let file = temp
            .file()
            .try_clone()
            .map_err(|e| Error::io("cannot duplicate the output file handle", e))?;
        self.build_into(file, dir)?;
        std::fs::read(temp.path()).map_err(|e| Error::io("cannot read back the built database", e))
    }

    fn build_into(mut self, file: std::fs::File, scratch: PathBuf) -> Result<BuildReport> {
        let directory = DirectorySpool::new(scratch.clone(), DIRECTORY_SPILL_THRESHOLD);
        let mut sink = PageSink::new(file, self.filter.clone(), directory);

        let header = Header {
            major_version: crate::format::MAJOR_VERSION,
            minor_version: crate::format::MINOR_VERSION,
            page_filter_count: u16::from(self.filter.is_some()),
            page_size: self.page_size,
            table_count: u16::try_from(self.tables.len())
                .map_err(|_| Error::invalid("more than 65535 tables"))?,
            page_count: 0,
            page_directory_position: FileOffset::new(0),
        };
        sink.write_raw(&header.encode())?;

        if let Some(filter) = &self.filter {
            let id = filter.id().as_bytes();
            if id.len() > u8::MAX as usize {
                return Err(Error::invalid("page filter ids must be at most 255 bytes"));
            }
            sink.write_raw(&[id.len() as u8])?;
            sink.write_raw(id)?;
        }

        // Descriptors first, exactly as upstream lays them out: every root ordinal is a
        // placeholder that the tree writer patches once the tree exists.
        let mut roots: Vec<TableRoots> = Vec::with_capacity(self.tables.len());
        for spec in &self.tables {
            let name = spec.name.as_bytes();
            sink.write_raw(&(name.len() as i32).to_le_bytes())?;
            sink.write_raw(name)?;

            let primary = write_index_descriptor(
                &mut sink,
                &format!("{}_pk", spec.name),
                spec.encoding.id(),
                true,
                ValueKind::RawData,
            )?;

            sink.write_raw(
                &u16::try_from(spec.secondaries.len())
                    .map_err(|_| Error::invalid("more than 65535 secondary indexes"))?
                    .to_le_bytes(),
            )?;

            let mut secondaries = Vec::with_capacity(spec.secondaries.len());
            for index in &spec.secondaries {
                secondaries.push(write_index_descriptor(
                    &mut sink,
                    &index.name,
                    index.encoding.id(),
                    index.unique,
                    ValueKind::PageRef,
                )?);
            }
            roots.push(TableRoots {
                primary,
                secondaries,
            });
        }

        let mut report = BuildReport {
            page_count: 0,
            file_size: 0,
            tables: Vec::new(),
        };

        let mut tables: std::collections::VecDeque<TableSpec> =
            std::mem::take(&mut self.tables).into();
        // The figure can have been lowered after the rows went in, in which case what
        // the tables are holding is over it before a single page is written.
        let mut held = buffered_bytes(&tables);
        while held > self.sort_buffer {
            let Some(fullest) = tables
                .iter_mut()
                .max_by_key(|table| table.spool.buffered_bytes())
            else {
                break;
            };
            if fullest.spool.buffered_bytes() == 0 {
                break;
            }
            fullest.spool.flush()?;
            held = buffered_bytes(&tables);
        }
        let mut roots = roots.into_iter();
        while let Some(spec) = tables.pop_front() {
            // The tables still waiting are holding buffers of their own, and the sorters
            // of the table about to be written share the sort buffer with them. A table
            // that would leave the sorters less than half of it is written out first:
            // its rows have to be sorted and read back in any case.
            let mut waiting = buffered_bytes(&tables);
            while waiting > self.sort_buffer / 2 {
                let Some(fullest) = tables
                    .iter_mut()
                    .max_by_key(|table| table.spool.buffered_bytes())
                else {
                    break;
                };
                if fullest.spool.buffered_bytes() == 0 {
                    break;
                }
                fullest.spool.flush()?;
                waiting = buffered_bytes(&tables);
            }
            let root_offsets = roots.next().expect("one set of roots per table");
            let table_report =
                self.build_table(&mut sink, spec, root_offsets, &scratch, waiting)?;
            report.tables.push(table_report);
        }

        let (directory_position, page_count) = sink.finish_directory()?;
        let mut patch = [0u8; 12];
        patch[0..4].copy_from_slice(
            &i32::try_from(page_count)
                .map_err(|_| Error::invalid("more than 2^31-1 pages"))?
                .to_le_bytes(),
        );
        patch[4..12].copy_from_slice(&(directory_position as i64).to_le_bytes());
        sink.patch_at(PAGE_COUNT_FIELD_OFFSET, &patch)?;
        sink.flush()?;

        report.page_count = page_count;
        report.file_size = sink.pos();
        Ok(report)
    }

    /// `waiting` is what the tables not yet written are holding: the sorters made here
    /// share the sort buffer with them.
    fn build_table(
        &self,
        sink: &mut PageSink,
        spec: TableSpec,
        roots: TableRoots,
        scratch: &Path,
        waiting: usize,
    ) -> Result<TableReport> {
        let TableSpec {
            name,
            encoding,
            secondaries,
            spool,
        } = spec;
        let rows = spool.len();

        // One spool per secondary index, filled while the primary tree is written.
        let mut index_spools: Vec<RecordSpool> = secondaries
            .iter()
            .map(|index| {
                RecordSpool::new(
                    Arc::clone(&index.encoding),
                    scratch.to_path_buf(),
                    "drydb-index",
                    self.sort_buffer,
                )
            })
            .collect();

        let primary_root = {
            // The records the primary sort hands out are in memory when the whole table
            // fitted in the buffer, and the index sorters fill up beside them. A table
            // with indexes therefore keeps at most half the buffer in memory and writes
            // the rest out, so that what the indexes are given is room that exists.
            let mut spool = spool;
            let share = self.sort_buffer.saturating_sub(waiting);
            if !secondaries.is_empty() && spool.buffered_bytes() > share / 2 {
                spool.flush()?;
            }
            let mut sorted = spool.into_sorted()?;
            let mut writer = TreeWriter::<Vec<Vec<u8>>>::new(
                sink,
                self.page_size,
                self.eytzinger_digests,
                &*encoding,
                true,
            )?;
            // Half of what is left for the index keys waiting for the leaf page they
            // belong to, half for the sorters they are handed to once it is written.
            // What the primary sort is holding comes off the top: it is memory in use
            // for as long as the rows are being read.
            let index_budget = share.saturating_sub(sorted.held_bytes());
            let spool_budget = index_budget - index_budget / 2;
            if !secondaries.is_empty() {
                writer.limit_pending_extra(index_budget / 2);
            }
            let mut on_value_ref = |keys: Vec<Vec<u8>>, reference: PageRef| -> Result<()> {
                let encoded = reference.encode();
                for (i, key) in keys.into_iter().enumerate() {
                    index_spools[i].push(&key, &encoded)?;
                }
                // As with the tables: the figure is for the build, not for each sorter,
                // so the index holding the most gives its buffer up when the total goes
                // over rather than every index keeping one of its own.
                keep_inside(&mut index_spools, spool_budget)
            };

            let mut previous: Option<Vec<u8>> = None;
            while let Some(record) = sorted.next_record()? {
                if let Some(previous) = &previous {
                    if encoding.compare(previous, &record.key)? != std::cmp::Ordering::Less {
                        return Err(Error::invalid(format!(
                            "table `{name}` was given the same primary key twice: {}",
                            encoding.format_key(&record.key)
                        )));
                    }
                }
                let mut index_keys = Vec::with_capacity(secondaries.len());
                for index in &secondaries {
                    index_keys.push((index.key_fn)(&record.key, &record.value)?);
                }
                writer.push(&record.key, &record.value, index_keys, &mut on_value_ref)?;
                previous = Some(record.key);
            }
            writer.finish(&mut on_value_ref)?
        };

        sink.patch_at(roots.primary, &(primary_root.get() as i64).to_le_bytes())?;

        let mut index_reports = Vec::with_capacity(secondaries.len());
        for ((index, index_spool), root_offset) in
            secondaries.iter().zip(index_spools).zip(roots.secondaries)
        {
            let entries = index_spool.len();
            let root = build_index_tree(
                sink,
                self.page_size,
                self.eytzinger_digests,
                index,
                index_spool,
                &name,
            )?;
            sink.patch_at(root_offset, &(root.get() as i64).to_le_bytes())?;
            index_reports.push(IndexReport {
                name: index.name.clone(),
                entries,
            });
        }

        Ok(TableReport {
            name,
            rows,
            indexes: index_reports,
        })
    }
}

fn build_index_tree(
    sink: &mut PageSink,
    page_size: usize,
    eytzinger: bool,
    index: &SecondarySpec,
    spool: RecordSpool,
    table_name: &str,
) -> Result<crate::format::PageOrdinal> {
    let mut sorted = spool.into_sorted()?;
    let mut discard = |_: (), _: PageRef| -> Result<()> { Ok(()) };

    if index.unique {
        let mut writer = TreeWriter::<()>::new(sink, page_size, eytzinger, &*index.encoding, true)?;
        let mut previous: Option<Vec<u8>> = None;
        while let Some(record) = sorted.next_record()? {
            if let Some(previous) = &previous {
                if index.encoding.compare(previous, &record.key)? != std::cmp::Ordering::Less {
                    return Err(Error::invalid(format!(
                        "unique index `{}` on table `{table_name}` was given the key {} twice",
                        index.name,
                        index.encoding.format_key(&record.key)
                    )));
                }
            }
            writer.push(&record.key, &record.value, (), &mut discard)?;
            previous = Some(record.key);
        }
        writer.finish(&mut discard)
    } else {
        // Duplicate keys are made unique by appending a record id, assigned in append
        // order inside each run of equal keys.
        let composite: Arc<dyn KeyEncoding> =
            Arc::new(DuplicateKeyEncoding::new(Arc::clone(&index.encoding)));
        // Keys stay on the page: the digest of a composite key covers only its source
        // key, so omitting the bytes would erase the record id that orders duplicates.
        let mut writer = TreeWriter::<()>::new(sink, page_size, eytzinger, &*composite, false)?;
        let mut previous: Option<Vec<u8>> = None;
        let mut rid: i32 = 0;
        while let Some(record) = sorted.next_record()? {
            match &previous {
                Some(previous_key)
                    if index.encoding.compare(previous_key, &record.key)?
                        == std::cmp::Ordering::Equal =>
                {
                    rid = rid.checked_add(1).ok_or_else(|| {
                        Error::new(
                            ErrorKind::ValueTooLarge,
                            format!(
                                "index `{}` on table `{table_name}` has more than 2^31-1 rows \
                                 for one key",
                                index.name
                            ),
                        )
                    })?;
                }
                _ => rid = 0,
            }
            let key = DuplicateKeyEncoding::encode(&record.key, rid);
            writer.push(&key, &record.value, (), &mut discard)?;
            previous = Some(record.key);
        }
        writer.finish(&mut discard)
    }
}

struct TableRoots {
    primary: u64,
    secondaries: Vec<u64>,
}

/// Writes an index descriptor and returns the file offset of its root ordinal field.
fn write_index_descriptor(
    sink: &mut PageSink,
    name: &str,
    encoding_id: &str,
    unique: bool,
    value_kind: ValueKind,
) -> Result<u64> {
    let start = sink.pos();
    let bytes = encode_index_descriptor(name, encoding_id, unique, value_kind, 0)?;
    let root_field = start + (bytes.len() - 8) as u64;
    // Upstream writes the offset just past the descriptor as the placeholder; the value
    // is overwritten either way, so the placeholder only matters for byte-comparing
    // an unfinished file.
    let mut bytes = bytes;
    let placeholder = (start + bytes.len() as u64) as i64;
    let len = bytes.len();
    bytes[len - 8..].copy_from_slice(&placeholder.to_le_bytes());
    sink.write_raw(&bytes)?;
    Ok(root_field)
}

/// What a build produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// Pages written, i.e. page directory slots.
    pub page_count: u64,
    /// Size of the finished file.
    pub file_size: u64,
    /// Per-table detail.
    pub tables: Vec<TableReport>,
}

/// Per-table build detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReport {
    /// Table name.
    pub name: String,
    /// Rows written.
    pub rows: u64,
    /// Secondary indexes built.
    pub indexes: Vec<IndexReport>,
}

/// Per-index build detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReport {
    /// Index name.
    pub name: String,
    /// Index entries written.
    pub entries: u64,
}
