//! Opening a database, and the table API.

use std::collections::HashMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use crate::blob::BlobReader;
use crate::btree::{resolve_value, resolve_value_cached, Tree};
use crate::budget::Budget;
use crate::cache::PageCache;
use crate::directory::PageDirectory;
use crate::encoding::{EncodingRegistry, KeyEncoding};
use crate::error::{Error, ErrorKind, Result};
use crate::filter::{FilterRegistry, PageFilter};
use crate::format::catalog::{Catalog, CatalogParse, IndexDescriptor, TableDescriptor, ValueKind};
use crate::format::header::Header;
use crate::format::HEADER_LEN;
use crate::index::{Index, IndexInner};
use crate::io::{read_up_to_at, FileSource, MemorySource, PageSource};
use crate::metrics::{MemoryReport, Metrics, MetricsSnapshot};
use crate::page::ValueGuard;
use crate::query::{count_range, prefix_range, Cursor, KeyRange, Order};
use crate::store::{Limits, PageStore, PageStoreParts};

/// Default managed memory budget.
pub const DEFAULT_MEMORY_BUDGET: u64 = 64 << 20;
/// Default number of page directory slots read per chunk.
pub const DEFAULT_DIRECTORY_CHUNK_ENTRIES: u64 = 512;
/// Default number of page directory chunks kept resident.
pub const DEFAULT_DIRECTORY_CHUNKS: usize = 8;

/// How to open a database.
///
/// The memory settings are the interesting ones: `memory_budget` is a hard cap on what
/// this crate allocates on your behalf, and `cache_capacity` is the softer limit the
/// page cache evicts against. See [`Database::memory_report`] for what is and is not
/// counted.
#[derive(Clone)]
pub struct OpenOptions {
    memory_budget: u64,
    cache_capacity: Option<u64>,
    cache_shards: usize,
    directory_chunk_entries: u64,
    directory_chunks: usize,
    limits: Limits,
    encodings: EncodingRegistry,
    filters: FilterRegistry,
    validate_digests: bool,
}

impl std::fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOptions")
            .field("memory_budget", &self.memory_budget)
            .field("cache_capacity", &self.cache_capacity)
            .field("cache_shards", &self.cache_shards)
            .field("limits", &self.limits)
            .finish()
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            memory_budget: DEFAULT_MEMORY_BUDGET,
            cache_capacity: None,
            cache_shards: default_shards(),
            directory_chunk_entries: DEFAULT_DIRECTORY_CHUNK_ENTRIES,
            directory_chunks: DEFAULT_DIRECTORY_CHUNKS,
            limits: Limits::default(),
            encodings: EncodingRegistry::new(),
            filters: FilterRegistry::new(),
            validate_digests: true,
        }
    }
}

fn default_shards() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() * 4)
        .unwrap_or(16)
        .clamp(1, 256)
}

impl OpenOptions {
    /// Default options.
    pub fn new() -> OpenOptions {
        OpenOptions::default()
    }

    /// Hard cap on bytes this crate allocates for pages, scratch and directory chunks.
    pub fn memory_budget(mut self, bytes: u64) -> Self {
        self.memory_budget = bytes;
        self
    }

    /// Soft cap the page cache evicts against. Defaults to three quarters of the
    /// budget, leaving room for scratch and live guards.
    pub fn cache_capacity(mut self, bytes: u64) -> Self {
        self.cache_capacity = Some(bytes);
        self
    }

    /// Number of cache shards.
    ///
    /// Rounded up to a power of two and capped at 1024. Opening also lowers it until the
    /// shard array itself fits in a small share of the memory budget, the same way
    /// [`OpenOptions::cache_capacity`] is capped by the budget: shards are a concurrency
    /// setting, and a tight budget is better spent on pages.
    pub fn cache_shards(mut self, shards: usize) -> Self {
        self.cache_shards = shards.max(1);
        self
    }

    /// Page directory slots per chunk read.
    pub fn directory_chunk_entries(mut self, entries: u64) -> Self {
        self.directory_chunk_entries = entries.max(1);
        self
    }

    /// Page directory chunks kept resident.
    pub fn directory_chunks(mut self, chunks: usize) -> Self {
        self.directory_chunks = chunks.max(1);
        self
    }

    /// Structural limits applied to pages and trees.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Whether a page's digests are checked against its keys before a tree uses it.
    ///
    /// On by default. Every search uses the digest array to decide where in a page the
    /// key comparison starts, so a digest that disagrees with its key makes the search
    /// skip rows without any bound being violated: the result is quietly short rather
    /// than an error. The check costs one digest computation per entry the first time a
    /// given tree reaches the page, not once per lookup, so it sits in the shadow of the
    /// read that brought the page in.
    ///
    /// Turning it off is for callers who produced the file themselves and measured that
    /// the check matters. It does not apply to pages that omit key bytes, where the
    /// digest is the key column and there is nothing to compare it against.
    pub fn validate_digests(mut self, validate: bool) -> Self {
        self.validate_digests = validate;
        self
    }

    /// Registers a custom key encoding for this database.
    pub fn register_encoding(mut self, encoding: Arc<dyn KeyEncoding>) -> Result<Self> {
        self.encodings.register(encoding)?;
        Ok(self)
    }

    /// Registers a custom page filter for this database.
    pub fn register_filter(mut self, filter: Arc<dyn PageFilter>) -> Self {
        self.filters.register(filter);
        self
    }

    /// Opens a database file.
    pub fn open(&self, path: impl AsRef<Path>) -> Result<Database> {
        let source = Arc::new(FileSource::open(path)?);
        self.open_source(source)
    }

    /// Opens a database held entirely in memory.
    ///
    /// The image becomes the database's, and is not charged to the memory budget: it is
    /// what is being read, the way a file on disk is, and the budget bounds the reading.
    /// A few kilobytes of budget over an image of a few megabytes is a normal thing to
    /// ask for and is not refused.
    ///
    /// Whether the bytes are copied on the way in is decided by what is handed over.
    /// An `Arc<[u8]>` is taken as it stands; a `Vec<u8>`, a `Box<[u8]>` or a slice is
    /// copied into one, which for a large image means holding it twice over for as long
    /// as the caller keeps their own. Build the [`MemorySource`] yourself and go through
    /// [`OpenOptions::open_source`] to decide that for yourself.
    pub fn open_bytes(&self, bytes: impl Into<Arc<[u8]>>) -> Result<Database> {
        self.open_source(Arc::new(MemorySource::new(bytes)))
    }

    /// Opens a database over any [`PageSource`].
    pub fn open_source(&self, source: Arc<dyn PageSource>) -> Result<Database> {
        // The 26-byte header comes first, on the stack, because the checks that decide
        // whether this file can be opened at all need its page size.
        let header = read_header(source.as_ref())?;

        let minimum_budget = minimum_budget(header.page_size, self.directory_chunk_entries);
        if self.memory_budget < minimum_budget {
            return Err(Error::invalid(format!(
                "memory budget of {} bytes is below the {minimum_budget} bytes a search of a \
                 {} byte page file needs",
                self.memory_budget, header.page_size
            )));
        }

        // Everything after the header is charged, including the buffer it is read into
        // and the descriptors parsed out of it.
        let budget = Budget::new(self.memory_budget);
        let (mut catalog_charge, catalog) = read_catalog(
            source.as_ref(),
            &header,
            self.limits.max_catalog_bytes,
            &budget,
        )?;

        let file_size = source
            .size()
            .map_err(|e| Error::io("cannot determine the file size", e))?;
        let (dir_start, dir_end) = {
            let header_span = Header {
                major_version: 1,
                minor_version: 4,
                page_filter_count: catalog.filters.len() as u16,
                page_size: catalog.page_size,
                table_count: catalog.tables.len() as u16,
                page_count: catalog.page_count,
                page_directory_position: catalog.directory_position,
            };
            header_span.directory_span()?
        };
        if dir_end > file_size {
            return Err(Error::corrupt(format!(
                "page directory spans {dir_start}..{dir_end} but the file is {file_size} bytes"
            )));
        }
        if (catalog.descriptor_end) > dir_start {
            return Err(Error::corrupt(format!(
                "table descriptors end at {} but the page directory starts at {dir_start}",
                catalog.descriptor_end
            )));
        }

        let filter = match catalog.filters.len() {
            0 => None,
            1 => Some(self.filters.resolve(&catalog.filters[0])?),
            n => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "the file declares {n} page filters; upstream 1.4 can neither write \
                         nor read a multi-filter file, so this reader rejects it"
                    ),
                ))
            }
        };

        if let Some(filter) = filter.as_ref() {
            // Asking the filter what it needs costs memory in its own right: the zstd
            // filter measures its decompression context by making one. Reserve the
            // question before asking it, so a budget that cannot afford the filter is
            // refused before anything has been allocated for it.
            let probe = budget.try_reserve(filter.probe_bytes());
            let working = match &probe {
                Ok(_) => Some(filter.working_set_bytes()),
                Err(_) => None,
            };
            drop(probe);
            let fixed = self.limits.max_decoded_page_bytes as u64
                + crate::budget::BUFFER_OVERHEAD
                + catalog.page_size as u64;
            if working.is_none_or(|working| fixed + working > self.memory_budget) {
                return Err(Error::invalid(format!(
                    "this file is page filtered, so decoding one page can reserve up to the \
                     {} byte decoded page limit plus the {} bytes the filter needs for \
                     itself; that does not fit in a {} byte memory budget. Raise the budget \
                     or lower `Limits::max_decoded_page_bytes`.",
                    self.limits.max_decoded_page_bytes,
                    match working {
                        Some(working) => working.to_string(),
                        None => format!("up to {}", filter.probe_bytes()),
                    },
                    self.memory_budget
                )));
            }
        }

        let metrics = Arc::new(Metrics::default());
        let cache_capacity = self
            .cache_capacity
            .unwrap_or(self.memory_budget / 4 * 3)
            .min(self.memory_budget);
        // Reserved before the shard array exists, not after: the number of shards comes
        // from the caller and the array is allocated once per database.
        let shards = affordable_shards(self.cache_shards, self.memory_budget);
        let store_charge = budget.try_reserve(PageCache::bookkeeping_bytes(shards))?;
        let cache = PageCache::new(
            cache_capacity,
            shards,
            Arc::clone(&metrics),
            Arc::clone(&budget),
        );
        let directory = PageDirectory::new(
            Arc::clone(&source),
            catalog.directory_position,
            catalog.page_count,
            self.directory_chunk_entries,
            self.directory_chunks,
            Arc::clone(&budget),
            Arc::clone(&metrics),
        );
        let store = Arc::new(PageStore::new(PageStoreParts {
            source: Arc::clone(&source),
            directory,
            cache,
            filter,
            budget: Arc::clone(&budget),
            limits: self.limits,
            metrics: Arc::clone(&metrics),
            directory_base: catalog.directory_position,
            charge: store_charge,
        }));

        // Reserved before the handles are built, not after: a catalog that fits only
        // once must not be copied three more times first.
        catalog_charge.grow(handle_footprint(&catalog))?;
        let catalog_charge = Arc::new(catalog_charge);

        let catalog = Arc::new(catalog);
        let mut tables = HashMap::with_capacity(catalog.tables.len());
        for descriptor in &catalog.tables {
            let table = TableInner::build(
                descriptor,
                &store,
                &self.encodings,
                self.validate_digests,
                &catalog_charge,
            )?;
            tables.insert(descriptor.name.clone(), Arc::new(table));
        }

        Ok(Database {
            catalog,
            store,
            tables,
            budget,
            metrics,
            _catalog_charge: catalog_charge,
        })
    }
}

/// The share of the budget the cache's shard array may take.
const SHARD_BUDGET_SHARE: u64 = 64;

/// Shards whose fixed cost fits in a small share of the budget.
///
/// Halved until it fits, down to one. A shard count is a concurrency setting, so trimming
/// it costs contention under load and nothing else, while spending a tight budget on an
/// array of mutexes costs the pages a search needs.
fn affordable_shards(requested: usize, memory_budget: u64) -> usize {
    let mut shards = PageCache::shard_count(requested);
    let affordable = memory_budget / SHARD_BUDGET_SHARE;
    while shards > 1 && PageCache::bookkeeping_bytes(shards) > affordable {
        shards /= 2;
    }
    shards
}

/// The smallest budget a search can complete in: a few pages at once, one page
/// directory chunk, and slack for the cache's own bookkeeping.
fn minimum_budget(page_size: usize, directory_chunk_entries: u64) -> u64 {
    let pages = (page_size as u64)
        .saturating_add(crate::budget::BUFFER_OVERHEAD)
        .saturating_mul(4);
    let directory = directory_chunk_entries
        .saturating_mul(crate::format::DIRECTORY_SLOT_LEN as u64)
        .saturating_add(crate::budget::BUFFER_OVERHEAD);
    pages.saturating_add(directory).saturating_add(8 << 10)
}

/// Reads and validates the 26-byte file header.
fn read_header(source: &dyn PageSource) -> Result<Header> {
    let mut bytes = [0u8; HEADER_LEN];
    let filled = read_up_to_at(source, &mut bytes, 0)?;
    if filled < HEADER_LEN {
        return Err(Error::new(
            ErrorKind::UnsupportedFormat,
            "file is shorter than the 26-byte header",
        ));
    }
    Header::parse(&bytes)
}

/// Reads the descriptor section, charging every buffer it allocates.
///
/// Starts small and doubles, so a file with a few short table names does not reserve the
/// whole catalog limit, and a budget too small for the descriptors fails here rather than
/// silently allocating outside it.
///
/// The read buffer and the parsed descriptors are charged separately, so the buffer's
/// share is released with the buffer and the returned charge covers the catalog alone.
/// The caller grows it further for the copies the table handles keep.
fn read_catalog(
    source: &dyn PageSource,
    header: &Header,
    max_bytes: usize,
    budget: &Arc<Budget>,
) -> Result<(crate::budget::Charge, Catalog)> {
    let mut capacity = (8 * 1024usize).min(max_bytes).max(HEADER_LEN);
    loop {
        let buffer_charge = budget.try_reserve(capacity as u64 + crate::budget::BUFFER_OVERHEAD)?;
        let mut catalog_charge = budget.try_reserve(0)?;
        let mut buf = vec![0u8; capacity];
        let filled = read_up_to_at(source, &mut buf, 0)?;
        buf.truncate(filled);
        if filled < HEADER_LEN {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                "file is shorter than the 26-byte header",
            ));
        }
        match crate::format::catalog::parse(
            &buf,
            header.page_filter_count,
            header.table_count,
            header.page_size,
            header.page_count,
            header.page_directory_position,
            &mut catalog_charge,
        )? {
            CatalogParse::Complete(catalog) => {
                drop(buf);
                drop(buffer_charge);
                return Ok((catalog_charge, *catalog));
            }
            CatalogParse::NeedMore(needed) => {
                if filled < capacity {
                    return Err(Error::corrupt(
                        "table descriptors are truncated at the end of the file",
                    ));
                }
                if capacity >= max_bytes {
                    return Err(Error::corrupt(format!(
                        "table descriptors need more than the {max_bytes} byte catalog limit"
                    )));
                }
                capacity = needed.max(capacity * 2).min(max_bytes);
                // The buffer goes before the reservation that stood for it: between the
                // two the budget would say the memory is free while it is still held.
                drop(buf);
                drop(catalog_charge);
                drop(buffer_charge);
            }
        }
    }
}

/// What one copy of a descriptor tree costs in memory.
fn descriptor_copy_bytes(catalog: &Catalog) -> u64 {
    fn index_bytes(index: &IndexDescriptor) -> u64 {
        index.name.len() as u64
            + index.key_encoding_id.len() as u64
            + 2 * crate::budget::BUFFER_OVERHEAD
    }
    let mut total = 0u64;
    for table in &catalog.tables {
        total = total
            .saturating_add(table.name.len() as u64)
            .saturating_add(crate::budget::BUFFER_OVERHEAD)
            .saturating_add(index_bytes(&table.primary))
            .saturating_add(
                (std::mem::size_of::<IndexDescriptor>() as u64)
                    .saturating_mul(table.secondaries.len() as u64),
            )
            .saturating_add(crate::budget::BUFFER_OVERHEAD);
        for index in &table.secondaries {
            total = total.saturating_add(index_bytes(index));
        }
    }
    total
}

/// What the table and index handles cost on top of the parsed catalog.
///
/// Each table handle clones its whole descriptor, each index handle clones its own, and
/// both maps key by a copy of a name. Three copies of the descriptor tree is an upper
/// bound on that, and the per-entry term covers the two hash maps.
fn handle_footprint(catalog: &Catalog) -> u64 {
    let entries = catalog.tables.len() as u64
        + catalog
            .tables
            .iter()
            .map(|t| t.secondaries.len() as u64)
            .sum::<u64>();
    descriptor_copy_bytes(catalog)
        .saturating_mul(3)
        .saturating_add(
            entries
                .saturating_add(2)
                .saturating_mul(crate::budget::BUFFER_OVERHEAD * 4),
        )
}

/// An open, read-only database.
///
/// Cheap to share: [`Table`] and [`Index`] handles are counted references into the same
/// page cache, and every query path is `&self`. Dropping the `Database` does not
/// invalidate a [`ValueGuard`] that is still alive.
pub struct Database {
    catalog: Arc<Catalog>,
    store: Arc<PageStore>,
    tables: HashMap<String, Arc<TableInner>>,
    budget: Arc<Budget>,
    metrics: Arc<Metrics>,
    /// The catalog's own memory.
    ///
    /// Shared with every table and index handle, because those keep copies of the
    /// descriptors and outlive the `Database` they came from. The reservation is released
    /// when the last of them is dropped.
    _catalog_charge: Arc<crate::budget::Charge>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field(
                "tables",
                &self
                    .catalog
                    .tables
                    .iter()
                    .map(|t| &t.name)
                    .collect::<Vec<_>>(),
            )
            .field("page_count", &self.catalog.page_count)
            .field("page_size", &self.catalog.page_size)
            .finish()
    }
}

impl Database {
    /// Opens a database file with default options.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        OpenOptions::new().open(path)
    }

    /// Opens an in-memory image with default options.
    pub fn open_bytes(bytes: impl Into<Arc<[u8]>>) -> Result<Database> {
        OpenOptions::new().open_bytes(bytes)
    }

    /// The catalog behind its counted reference, for a caller that has to hold it while
    /// borrowing `self` elsewhere.
    pub(crate) fn catalog_arc(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    /// The decoded catalog.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Table names, in file order.
    ///
    /// The list is the caller's memory, so it is reserved before it is built and comes
    /// back with the reservation that paid for it. The names themselves are borrowed
    /// from the catalog, which is charged already.
    pub fn table_names(&self) -> Result<crate::budget::Charged<Vec<&str>>> {
        let tables = &self.catalog.tables;
        let charge = self.reserve(
            (tables.len() as u64).saturating_mul(std::mem::size_of::<&str>() as u64)
                + crate::budget::BUFFER_OVERHEAD,
        )?;
        let mut names = Vec::with_capacity(tables.len());
        names.extend(tables.iter().map(|t| t.name.as_str()));
        Ok(crate::budget::Charged::new(names, charge))
    }

    /// Opens a table by name.
    pub fn table(&self, name: &str) -> Result<Table> {
        self.tables
            .get(name)
            .map(|inner| Table {
                inner: Arc::clone(inner),
            })
            .ok_or_else(|| {
                // The name is the caller's, of the caller's length, and rendering it
                // whole would spend more memory reporting the mistake than the lookup
                // that failed ever asked for.
                Error::invalid(format!(
                    "no table named `{}`",
                    crate::format::catalog::describe_name(name)
                ))
            })
    }

    /// Reserves `bytes` of the database's memory budget, reclaiming cached pages if it
    /// has to.
    ///
    /// For a caller that holds memory on this database's behalf, so that it is counted
    /// where everything else this crate allocates is counted. The reservation lasts as
    /// long as the returned [`Charge`](crate::Charge).
    pub fn reserve(&self, bytes: u64) -> Result<crate::budget::Charge> {
        self.store.reserve(bytes)
    }

    /// Page path counters since the database was opened.
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// What the managed memory budget currently holds.
    ///
    /// `charged` counts live page buffers, decompression scratch and page directory
    /// chunks. It does not count allocator overhead, values the caller copied out, the
    /// OS page cache, or pages an `mmap` source made resident.
    pub fn memory_report(&self) -> MemoryReport {
        MemoryReport {
            budget_limit: self.budget.limit(),
            charged: self.budget.in_use(),
            peak_charged: self.budget.peak(),
            cache_resident: self.store.cache().resident_bytes(),
            cache_capacity: self.store.cache().capacity(),
        }
    }

    /// The memory budget this database charges against.
    ///
    /// Shared, so an adapter that allocates on the caller's behalf can charge the same
    /// budget rather than a separate one: what the caller set is then the whole of what
    /// reading costs, however many layers are involved.
    pub fn budget(&self) -> &Arc<crate::budget::Budget> {
        &self.budget
    }

    /// Bytes currently charged to the budget.
    ///
    /// The same figure as [`MemoryReport::charged`], read on its own. Reading the whole
    /// report walks the cache to total what it holds, which takes every shard's lock;
    /// this reads one counter and takes none, so it can be asked at any moment, including
    /// from inside an allocator.
    pub fn charged_bytes(&self) -> u64 {
        self.budget.in_use()
    }

    /// Pages currently held by the cache.
    pub fn cached_pages(&self) -> usize {
        self.store.cache().len()
    }

    pub(crate) fn store(&self) -> &Arc<PageStore> {
        &self.store
    }
}

pub(crate) struct TableInner {
    descriptor: TableDescriptor,
    primary: Arc<Tree>,
    secondaries: HashMap<String, Arc<IndexInner>>,
    /// Keeps the catalog's reservation alive for as long as this handle holds a copy of
    /// its descriptor, which can be past the end of the `Database`.
    _catalog_charge: Arc<crate::budget::Charge>,
}

impl TableInner {
    fn build(
        descriptor: &TableDescriptor,
        store: &Arc<PageStore>,
        encodings: &EncodingRegistry,
        validate_digests: bool,
        catalog_charge: &Arc<crate::budget::Charge>,
    ) -> Result<TableInner> {
        if descriptor.primary.value_kind != ValueKind::RawData {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!(
                    "primary index of table `{}` stores {:?}, expected RawData",
                    descriptor.name, descriptor.primary.value_kind
                ),
            ));
        }
        let encoding = encodings.resolve(&descriptor.primary.key_encoding_id)?;
        let primary = Arc::new(Tree::new(
            Arc::clone(store),
            descriptor.primary.root,
            encoding,
            validate_digests,
            Arc::clone(catalog_charge),
        ));

        let mut secondaries = HashMap::with_capacity(descriptor.secondaries.len());
        for index in &descriptor.secondaries {
            secondaries.insert(
                index.name.clone(),
                Arc::new(IndexInner::build(
                    index,
                    store,
                    encodings,
                    validate_digests,
                    catalog_charge,
                )?),
            );
        }
        Ok(TableInner {
            descriptor: descriptor.clone(),
            primary,
            secondaries,
            _catalog_charge: Arc::clone(catalog_charge),
        })
    }
}

/// The outcome of a lookup that was not allowed to perform I/O.
#[derive(Debug)]
pub enum CachedLookup {
    /// The key was found and its value is cached.
    Value(ValueGuard),
    /// Every page on the path was cached, and the key is not in the table.
    Missing,
    /// A page on the path is not cached, so the answer is unknown.
    NotCached,
}

impl CachedLookup {
    /// The value, if the lookup completed and found one.
    pub fn value(self) -> Option<ValueGuard> {
        match self {
            CachedLookup::Value(v) => Some(v),
            _ => None,
        }
    }

    /// Whether the lookup completed without needing I/O.
    pub fn is_resolved(&self) -> bool {
        !matches!(self, CachedLookup::NotCached)
    }
}

impl crate::budget::Reserve for Database {
    fn reserve(&self, bytes: u64) -> Result<crate::budget::Charge> {
        Database::reserve(self, bytes)
    }
}

/// A table: one primary key tree plus its secondary indexes.
#[derive(Clone)]
pub struct Table {
    inner: Arc<TableInner>,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table")
            .field("name", &self.inner.descriptor.name)
            .field("encoding", &self.inner.primary.encoding().id())
            .field(
                "indexes",
                &self
                    .inner
                    .descriptor
                    .secondaries
                    .iter()
                    .map(|i| &i.name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl crate::budget::Reserve for Table {
    fn reserve(&self, bytes: u64) -> Result<crate::budget::Charge> {
        Table::reserve(self, bytes)
    }
}

impl Table {
    /// Table name.
    pub fn name(&self) -> &str {
        &self.inner.descriptor.name
    }

    /// Reserves `bytes` of the database's memory budget, like
    /// [`Database::reserve`](crate::Database::reserve).
    pub fn reserve(&self, bytes: u64) -> Result<crate::budget::Charge> {
        self.inner.primary.store().reserve(bytes)
    }

    /// The primary key encoding.
    pub fn key_encoding(&self) -> &Arc<dyn KeyEncoding> {
        self.inner.primary.encoding()
    }

    /// The table's descriptor.
    pub fn descriptor(&self) -> &TableDescriptor {
        &self.inner.descriptor
    }

    /// Secondary index names.
    ///
    /// Reserved as [`Database::table_names`] is.
    pub fn index_names(&self) -> Result<crate::budget::Charged<Vec<&str>>> {
        let secondaries = &self.inner.descriptor.secondaries;
        let charge = self.reserve(
            (secondaries.len() as u64).saturating_mul(std::mem::size_of::<&str>() as u64)
                + crate::budget::BUFFER_OVERHEAD,
        )?;
        let mut names = Vec::with_capacity(secondaries.len());
        names.extend(secondaries.iter().map(|i| i.name.as_str()));
        Ok(crate::budget::Charged::new(names, charge))
    }

    /// Looks one key up.
    pub fn get(&self, key: &[u8]) -> Result<Option<ValueGuard>> {
        let tree = &self.inner.primary;
        tree.encoding().validate_key(key)?;
        let Some((pin, index)) = tree.find(key)? else {
            return Ok(None);
        };
        let entry = pin.view()?.leaf_entry(index)?;
        let (value_pin, range) = resolve_value(tree.store(), &pin, &entry)?;
        Ok(Some(value_pin.into_guard(range)?))
    }

    /// Looks one key up, but only against pages that are already cached.
    ///
    /// Never performs I/O, so it is safe to call from a context that must not block.
    /// [`CachedLookup::NotCached`] means the answer is unknown, not that the key is
    /// absent; fall back to [`Table::get`] on another thread for that.
    pub fn get_cached(&self, key: &[u8]) -> Result<CachedLookup> {
        let tree = &self.inner.primary;
        tree.encoding().validate_key(key)?;
        let Some(found) = tree.find_cached(key)? else {
            return Ok(CachedLookup::NotCached);
        };
        let Some((pin, index)) = found else {
            return Ok(CachedLookup::Missing);
        };
        let entry = pin.view()?.leaf_entry(index)?;
        match resolve_value_cached(tree.store(), &pin, &entry)? {
            None => Ok(CachedLookup::NotCached),
            Some((value_pin, range)) => Ok(CachedLookup::Value(value_pin.into_guard(range)?)),
        }
    }

    /// Whether a key is present, without materialising its value.
    pub fn contains_key(&self, key: &[u8]) -> Result<bool> {
        let tree = &self.inner.primary;
        tree.encoding().validate_key(key)?;
        Ok(tree.find(key)?.is_some())
    }

    /// A cursor over a key range.
    pub fn range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>, order: Order) -> Result<Cursor> {
        let charge = crate::query::reserve_bounds(&self.inner.primary, lower, upper, 0)?;
        Cursor::new(
            Arc::clone(&self.inner.primary),
            charge,
            KeyRange::new(lower, upper),
            order,
        )
    }

    /// A cursor over every entry.
    pub fn scan(&self, order: Order) -> Result<Cursor> {
        let charge = crate::query::reserve_bounds(
            &self.inner.primary,
            Bound::Unbounded,
            Bound::Unbounded,
            0,
        )?;
        Cursor::new(
            Arc::clone(&self.inner.primary),
            charge,
            KeyRange::all(),
            order,
        )
    }

    /// A cursor over every key starting with `prefix`.
    ///
    /// Only defined for encodings whose order is byte-lexicographic; `i64` and
    /// `uuidv7` keys report [`ErrorKind::InvalidArgument`] rather than returning a
    /// range that happens to be wrong.
    pub fn prefix(&self, prefix: &[u8], order: Order) -> Result<Cursor> {
        let encoding = self.inner.primary.encoding();
        if !encoding.is_byte_lexicographic() {
            return Err(Error::invalid(format!(
                "encoding `{}` does not order keys by their bytes, so a byte prefix does not \
                 describe a key range; use `range` instead",
                encoding.id()
            )));
        }
        let charge = crate::query::reserve_bounds(
            &self.inner.primary,
            Bound::Included(prefix),
            Bound::Included(prefix),
            0,
        )?;
        Cursor::for_prefix(
            Arc::clone(&self.inner.primary),
            charge,
            prefix_range(prefix),
            order,
        )
    }

    /// Counts entries in a range without reading any value.
    pub fn count_range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<u64> {
        count_range(&self.inner.primary, lower, upper)
    }

    /// Counts every entry.
    pub fn count(&self) -> Result<u64> {
        count_range(&self.inner.primary, Bound::Unbounded, Bound::Unbounded)
    }

    /// Opens a secondary index.
    pub fn index(&self, name: &str) -> Result<Index> {
        self.inner
            .secondaries
            .get(name)
            .map(|inner| Index::new(Arc::clone(inner)))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "table `{}` has no index named `{}`",
                    crate::format::catalog::describe_name(&self.inner.descriptor.name),
                    crate::format::catalog::describe_name(name)
                ))
            })
    }

    /// A streaming reader over a value stored on its own blob page.
    ///
    /// Returns `Ok(None)` when the key is missing, and
    /// [`ErrorKind::InvalidArgument`] when the value is stored inline (use
    /// [`Table::get`], which does not copy either). Blob streaming reads straight from
    /// the file and therefore needs an unfiltered database.
    pub fn blob_reader(&self, key: &[u8]) -> Result<Option<BlobReader>> {
        let tree = &self.inner.primary;
        tree.encoding().validate_key(key)?;
        let Some((pin, index)) = tree.find(key)? else {
            return Ok(None);
        };
        let entry = pin.view()?.leaf_entry(index)?;
        match entry.value {
            crate::format::node::LeafValue::Inline { .. } => Err(Error::invalid(
                "this value is stored inline in its leaf page; read it with `get`",
            )),
            crate::format::node::LeafValue::Overflow { page } => {
                BlobReader::open(tree.store(), page).map(Some)
            }
        }
    }

    pub(crate) fn tree(&self) -> &Arc<Tree> {
        &self.inner.primary
    }
}
