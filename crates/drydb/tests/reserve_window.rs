//! That nothing is allocated before it is reserved.
//!
//! The budget's promise is not only that what is held stays inside it, but that the
//! reservation comes first: memory taken and paid for afterwards is memory another
//! thread could have been given in the meantime.
//!
//! The probe is the allocator. While it is armed it weighs what has been allocated
//! against what the budget says is charged, on every allocation large enough to matter,
//! and keeps the worst the first has ever run ahead of the second. The allocator is
//! global, so this file holds one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use drydb::{Database, DatabaseBuilder, Int64Encoding, MemorySource, OpenOptions, PageSource};

/// Allocations below this are the small change of running a query; the structures this
/// watches are all larger.
const WATCH_FROM: usize = 4096;

static ARMED: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicI64 = AtomicI64::new(0);
static EXCESS: AtomicI64 = AtomicI64::new(0);
static BASELINE: AtomicI64 = AtomicI64::new(0);
/// How many allocations the probe has weighed, so a run that watched nothing says so.
static WEIGHED: AtomicI64 = AtomicI64::new(0);

/// The database the probe asks. Leaked, so the reference outlives the allocator's view.
static DB: OnceLock<&'static Database> = OnceLock::new();

fn weigh(bytes: i64, size: usize) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if size < WATCH_FROM || bytes <= 0 {
        return;
    }
    // Off while the budget is read, so reading it cannot set the probe off again.
    ARMED.store(false, Ordering::Relaxed);
    if let Some(db) = DB.get() {
        let charged = db.charged_bytes() as i64 - BASELINE.load(Ordering::Relaxed);
        EXCESS.fetch_max(live - charged, Ordering::Relaxed);
        WEIGHED.fetch_add(1, Ordering::Relaxed);
    }
    ARMED.store(true, Ordering::Relaxed);
}

struct Probe;

// SAFETY: every method forwards to the system allocator unchanged; the weighing is
// bookkeeping on the side and never affects the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        weigh(layout.size() as i64, layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        weigh(-(layout.size() as i64), 0);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Counted as the new block arriving beside the old one, which is what happens
        // for as long as the move takes.
        weigh(new_size as i64 - layout.size() as i64, new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Probe = Probe;

#[test]
fn the_cache_reserves_its_tables_before_it_grows_them() {
    let mut builder = DatabaseBuilder::new().page_size(60).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..10_000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), b"v")
            .unwrap();
    }
    // Built before the probe is armed: the file is the caller's memory, not the budget's.
    let source: Arc<dyn PageSource> = Arc::new(MemorySource::new(builder.build_to_vec().unwrap()));

    let budget = 4 << 20;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_capacity(budget)
        // One shard, so every page goes in the same table and ring and they grow through
        // several sizes.
        .cache_shards(1)
        .open_source(source)
        .unwrap();
    let db: &'static Database = Box::leak(Box::new(db));
    let _ = DB.set(db);
    let table = db.table("t").unwrap();

    LIVE.store(0, Ordering::Relaxed);
    EXCESS.store(0, Ordering::Relaxed);
    WEIGHED.store(0, Ordering::Relaxed);
    BASELINE.store(db.charged_bytes() as i64, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    // Read until the budget is full, and then keep reading: what follows has to be
    // refused rather than served by growing a table that nothing has paid for.
    for i in 0..10_000i64 {
        let _ = table.get(&Int64Encoding::encode(i));
    }
    // Everything the reads left, so the ones that follow find the budget full and have
    // to be refused rather than served by growing a table nothing has paid for.
    let _rest = db.reserve(budget - db.charged_bytes());
    for i in 0..10_000i64 {
        let _ = table.get(&Int64Encoding::encode(i));
    }
    ARMED.store(false, Ordering::Relaxed);

    let weighed = WEIGHED.load(Ordering::Relaxed);
    assert!(weighed > 0, "the probe watched nothing");
    let excess = EXCESS.load(Ordering::Relaxed);
    assert!(
        excess < WATCH_FROM as i64,
        "{excess} bytes were allocated before anything was charged for them"
    );
}
