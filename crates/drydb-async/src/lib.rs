//! Async access to a [`drydb`] database.
//!
//! A page read is a blocking file read. This crate does not hide that behind an
//! `async fn`: a query first runs against the page cache on the calling thread, which
//! answers it with no I/O and no task spawn when the pages it needs are already
//! resident, and otherwise moves the whole query to a blocking thread with
//! [`tokio::task::spawn_blocking`].
//!
//! ```no_run
//! # async fn example() -> Result<(), drydb::Error> {
//! use drydb_async::AsyncDatabase;
//!
//! let db = AsyncDatabase::open("game.drydb").await?;
//! let table = db.table("items")?;
//! if let Some(value) = table.get(&1234i64.to_le_bytes()).await? {
//!     println!("{} bytes", value.as_bytes().len());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # What dropping a future does
//!
//! Dropping a future that is waiting on `spawn_blocking` does not stop the blocking
//! work; tokio runs it to completion on its pool. That is safe here: the closure owns
//! everything it touches, so a cancelled query still releases its page pins, its budget
//! reservation and its place in the page cache's de-duplicated load. What it does not do
//! is free the thread any sooner.
//!
//! # Scans
//!
//! There is no async cursor. Stepping one would mean either holding a page across an
//! await -- which pins it for as long as the consumer takes -- or a task hop per row.
//! Instead, [`AsyncTable::collect_range`] runs a bounded scan on the blocking pool and
//! returns the rows it read; for an unbounded scan, use the synchronous
//! [`Cursor`] on a thread you control.

#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use drydb::{
    CachedLookup, Charged, Cursor, Database, Error, ErrorKind, OpenOptions, Order, Result, Table,
    ValueGuard,
};

/// An open database, usable from async tasks.
#[derive(Debug, Clone)]
pub struct AsyncDatabase {
    inner: Arc<Database>,
}

impl AsyncDatabase {
    /// Opens a database file on a blocking thread.
    pub async fn open(path: impl AsRef<Path>) -> Result<AsyncDatabase> {
        AsyncDatabase::open_with(path, OpenOptions::new()).await
    }

    /// Opens a database file with explicit options, on a blocking thread.
    pub async fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> Result<AsyncDatabase> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let db = blocking(move || options.open(&path)).await?;
        Ok(AsyncDatabase {
            inner: Arc::new(db),
        })
    }

    /// Wraps an already-open database.
    pub fn from_database(db: Database) -> AsyncDatabase {
        AsyncDatabase {
            inner: Arc::new(db),
        }
    }

    /// The underlying synchronous database.
    pub fn sync(&self) -> &Database {
        &self.inner
    }

    /// Opens a table.
    pub fn table(&self, name: &str) -> Result<AsyncTable> {
        let table = self.inner.table(name)?;
        Ok(AsyncTable {
            table,
            _db: Arc::clone(&self.inner),
        })
    }

    /// Table names, in file order.
    ///
    /// Unlike [`Database::table_names`](drydb::Database::table_names), which borrows the
    /// names from the catalog, this copies them, so the copies are reserved before they
    /// are made and the reservation comes back with them.
    pub fn table_names(&self) -> Result<Charged<Vec<String>>> {
        let borrowed = self.inner.table_names()?;
        let bytes: u64 = borrowed
            .iter()
            .map(|name| name.len() as u64 + drydb::BUFFER_OVERHEAD)
            .sum::<u64>()
            + (borrowed.len() as u64) * std::mem::size_of::<String>() as u64
            + drydb::BUFFER_OVERHEAD;
        let charge = self.inner.reserve(bytes)?;
        let mut names = Vec::with_capacity(borrowed.len());
        names.extend(borrowed.iter().map(|name| name.to_string()));
        Ok(Charged::new(names, charge))
    }

    /// Runs a verification pass on a blocking thread.
    pub async fn verify(
        &self,
        options: drydb::VerifyOptions,
    ) -> Result<Charged<drydb::VerifyReport>> {
        let db = Arc::clone(&self.inner);
        blocking(move || db.verify(options)).await
    }
}

/// A table, usable from async tasks.
#[derive(Debug, Clone)]
pub struct AsyncTable {
    table: Table,
    /// Keeps the database alive for as long as the handle is.
    _db: Arc<Database>,
}

impl AsyncTable {
    /// The synchronous table handle.
    pub fn sync(&self) -> &Table {
        &self.table
    }

    /// Table name.
    pub fn name(&self) -> &str {
        self.table.name()
    }

    /// Looks one key up.
    ///
    /// Completes on the calling thread when every page it needs is cached. Otherwise the
    /// key is copied and the lookup runs on a blocking thread.
    ///
    /// The copy is reserved against the database's memory budget before it is made and
    /// released when the lookup returns: a key is as long as the caller makes it, and
    /// this one is held for the length of a thread hop.
    pub async fn get(&self, key: &[u8]) -> Result<Option<ValueGuard>> {
        match self.table.get_cached(key)? {
            CachedLookup::Value(value) => return Ok(Some(value)),
            CachedLookup::Missing => return Ok(None),
            CachedLookup::NotCached => {}
        }
        let charge = self
            .table
            .reserve(key.len() as u64 + drydb::BUFFER_OVERHEAD)?;
        let key = key.to_vec();
        let table = self.table.clone();
        // The reservation moves with the copy rather than staying with this future.
        // Dropping the future does not stop the blocking work, so a charge left behind
        // here would be released while the copy it stands for is still in use, and a
        // caller that starts and cancels lookups could spend any amount of memory.
        blocking(move || {
            let found = table.get(&key);
            // The copy goes before the reservation that stood for it: a charge released
            // while the bytes it covers are still there leaves the budget reporting room
            // that is not free, which a concurrent reservation would take.
            drop(key);
            drop(charge);
            found
        })
        .await
    }

    /// Looks one key up, taking ownership of the key so no copy is needed on the slow
    /// path.
    pub async fn get_owned(&self, key: Vec<u8>) -> Result<Option<ValueGuard>> {
        let table = self.table.clone();
        blocking(move || table.get(&key)).await
    }

    /// Counts the rows in a range, on a blocking thread. Reads no values.
    pub async fn count_range(&self, lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> Result<u64> {
        let table = self.table.clone();
        blocking(move || table.count_range(as_ref(&lower), as_ref(&upper))).await
    }

    /// Counts every row, on a blocking thread.
    pub async fn count(&self) -> Result<u64> {
        let table = self.table.clone();
        blocking(move || table.count()).await
    }

    /// Reads up to `limit` rows of a range on a blocking thread.
    ///
    /// Unlike a cursor, this materialises the rows it returns: `limit` is what bounds
    /// the memory it takes, so pick one your caller can hold.
    ///
    /// The rows are reserved against the database's memory budget as they are collected,
    /// so a `limit` the budget cannot hold comes back as
    /// [`ErrorKind::BudgetExceeded`](drydb::ErrorKind) rather than being allocated. The
    /// reservation is released when the returned value is dropped, which is why the rows
    /// come back inside a [`Charged`] rather than as a bare `Vec`: a list that has been
    /// emptied still holds its allocation.
    pub async fn collect_range(
        &self,
        lower: Bound<Vec<u8>>,
        upper: Bound<Vec<u8>>,
        order: Order,
        limit: usize,
    ) -> Result<Charged<Vec<Row>>> {
        let table = self.table.clone();
        blocking(move || {
            let mut cursor = table.range(as_ref(&lower), as_ref(&upper), order)?;
            collect(&table, &mut cursor, limit)
        })
        .await
    }

    /// Reads up to `limit` rows whose keys start with `prefix`, on a blocking thread.
    ///
    /// Reserved as [`AsyncTable::collect_range`] is.
    pub async fn collect_prefix(
        &self,
        prefix: Vec<u8>,
        order: Order,
        limit: usize,
    ) -> Result<Charged<Vec<Row>>> {
        let table = self.table.clone();
        blocking(move || {
            let mut cursor = table.prefix(&prefix, order)?;
            collect(&table, &mut cursor, limit)
        })
        .await
    }
}

/// One row read by a bounded async scan.
#[derive(Debug)]
pub struct Row {
    /// The entry key, copied out of the page.
    pub key: Vec<u8>,
    /// The entry value, still borrowed from its page.
    pub value: ValueGuard,
}

/// What one row occupies in the list it is collected into.
fn row_slot_bytes() -> u64 {
    std::mem::size_of::<Row>() as u64
}

fn collect(table: &Table, cursor: &mut Cursor, limit: usize) -> Result<Charged<Vec<Row>>> {
    // One reservation for the whole collection rather than one per row: emptying the
    // list would drop the rows and keep the list's allocation, and a reservation living
    // in the rows would go with them.
    let mut charge = table.reserve(0)?;
    let mut rows: Vec<Row> = Vec::new();
    while rows.len() < limit && cursor.advance()? {
        let entry = cursor.current().expect("positioned");
        // The row's place in the list and its copy of the key, reserved before either
        // is made. A collected scan is as long as the caller asks for.
        let more =
            table.reserve(row_slot_bytes() + entry.key().len() as u64 + drydb::BUFFER_OVERHEAD)?;
        charge.absorb(more);
        if rows.len() == rows.capacity() {
            // Growing the list holds the old allocation and the new one at once while
            // the rows move across, and it grows by one so it never holds room it is
            // not using.
            let _moving = table.reserve(rows.len() as u64 * row_slot_bytes())?;
            rows.reserve_exact(1);
        }
        rows.push(Row {
            key: entry.key_to_vec(),
            value: entry.to_guard()?,
        });
    }
    Ok(Charged::new(rows, charge))
}

fn as_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
    }
}

/// Runs `task` on tokio's blocking pool.
async fn blocking<F, R>(task: F) -> Result<R>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => Err(Error::new(
            ErrorKind::Io,
            "the blocking task running this query panicked",
        )),
        Err(e) => Err(Error::new(
            ErrorKind::Io,
            format!("blocking task failed: {e}"),
        )),
    }
}
