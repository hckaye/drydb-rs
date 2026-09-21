//! A read-only embedded key/value store that reads and writes the
//! [DryDB](https://github.com/hadashiA/DryDB) 1.4 storage format.
//!
//! A database is built once, then opened read-only. Opening it reads the header and the
//! table descriptors; everything else -- B+Tree pages, the page directory, values -- is
//! read on demand and kept in a bounded cache. Nothing is loaded eagerly, nothing is
//! deserialised up front, and no index is rebuilt at startup.
//!
//! ```no_run
//! use std::ops::Bound;
//! use drydb::{Database, Order};
//!
//! let db = Database::open("game.drydb")?;
//! let table = db.table("items")?;
//!
//! if let Some(value) = table.get(&1234i64.to_le_bytes())? {
//!     // Borrowed straight from the cached page; nothing was copied.
//!     println!("{} bytes", value.as_bytes().len());
//! }
//!
//! let mut cursor = table.range(
//!     Bound::Included(&100i64.to_le_bytes()[..]),
//!     Bound::Excluded(&200i64.to_le_bytes()[..]),
//!     Order::Ascending,
//! )?;
//! while cursor.advance()? {
//!     let entry = cursor.current().expect("positioned");
//!     let _ = (entry.key(), entry.value());
//! }
//! # Ok::<(), drydb::Error>(())
//! ```
//!
//! # What this crate guarantees
//!
//! * **Memory.** [`OpenOptions::memory_budget`] caps what the crate allocates for pages,
//!   scratch and directory chunks. Exceeding it is an error, never an unbounded
//!   allocation and never a deadlock. [`Database::memory_report`] states exactly what
//!   the number covers.
//! * **Safety on bad input.** Nothing from the file is reinterpreted as a Rust struct.
//!   A corrupt or hostile file yields [`ErrorKind::CorruptData`], not a panic and not
//!   undefined behaviour.
//! * **Lifetimes.** A [`ValueGuard`] keeps its page alive on its own. Eviction, other
//!   queries and dropping the [`Database`] cannot invalidate bytes you still hold.
//!
//! # Compatibility
//!
//! The target is upstream commit `6b175929491793948e63430c20c2d6f58300d97f`, storage
//! format 1.4. `docs/compatibility.md` records the field layouts, what has been checked
//! against the C# implementation, and every place the two deliberately disagree.

#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod builder;
pub mod encoding;
pub mod filter;
pub mod format;
pub mod io;

mod blob;
mod btree;
mod budget;
mod cache;
mod db;
mod directory;
mod error;
mod index;
mod metrics;
mod page;
mod query;
mod store;
mod verify;

pub use blob::BlobReader;
pub use budget::{Budget, Charge, Charged, Reserve, BUFFER_OVERHEAD};
pub use builder::{
    BuildReport, DatabaseBuilder, IndexReport, SecondaryKeyFn, TableId, TableReport,
    DEFAULT_PAGE_SIZE, DEFAULT_SORT_BUFFER, MIN_PAGE_SIZE,
};
pub use db::{
    CachedLookup, Database, OpenOptions, Table, DEFAULT_DIRECTORY_CHUNKS,
    DEFAULT_DIRECTORY_CHUNK_ENTRIES, DEFAULT_MEMORY_BUDGET,
};
pub use encoding::{
    AsciiEncoding, EncodingRegistry, Int64Encoding, KeyEncoding, UlidEncoding, Uuidv7Encoding,
};
pub use error::{BudgetInfo, Error, ErrorKind, Location, Result};
pub use filter::{FilterRegistry, PageFilter, ZSTD_FILTER_ID};
pub use format::catalog::{Catalog, IndexDescriptor, TableDescriptor, ValueKind};
pub use format::pageref::PageRef;
pub use format::{FileOffset, PageLocalOffset, PageOrdinal};
pub use index::{Index, IndexCursor};
pub use io::{FileSource, MemorySource, PageSource};
pub use metrics::{MemoryReport, MetricsSnapshot};
pub use page::{PagePin, ValueGuard};
pub use query::{Cursor, EntryRef, KeyRange, Order};
pub use store::Limits;
pub use verify::{VerifyOptions, VerifyProblem, VerifyReport};

#[cfg(feature = "zstd")]
pub use filter::ZstdFilter;

#[cfg(feature = "mmap")]
pub use io::MmapSource;
