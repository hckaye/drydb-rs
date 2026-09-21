//! That an asynchronous lookup's copy of the key stays charged until it is freed.
//!
//! The copy is made on the caller's thread and used on a blocking one, with the
//! reservation moved across with it. Rust drops locals in reverse order of declaration,
//! so a reservation taken into the closure ahead of the key was released first, leaving
//! the budget reporting room the key was still using.
//!
//! The probe is the allocator itself: when a buffer of exactly the key's length is freed,
//! it asks the budget what it thinks is charged. The allocator is global, so this file
//! holds one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use drydb::{AsciiEncoding, Database, DatabaseBuilder, OpenOptions};
use drydb_async::AsyncDatabase;

static TARGET: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static LEAST: AtomicI64 = AtomicI64::new(i64::MAX);
static BASELINE: AtomicI64 = AtomicI64::new(0);

/// The database the probe asks, set once the test has opened it and leaked it so the
/// reference outlives the allocator's view of it.
static DB: OnceLock<&'static Database> = OnceLock::new();

fn sample(size: usize) {
    if !ARMED.load(Ordering::Relaxed) || size != TARGET.load(Ordering::Relaxed) {
        return;
    }
    // Off while the budget is read, so reading it cannot set the probe off again.
    ARMED.store(false, Ordering::Relaxed);
    if let Some(db) = DB.get() {
        let charged = db.charged_bytes() as i64 - BASELINE.load(Ordering::Relaxed);
        LEAST.fetch_min(charged, Ordering::Relaxed);
    }
    ARMED.store(true, Ordering::Relaxed);
}

struct Probe;

// SAFETY: every method forwards to the system allocator unchanged; the sampling is
// bookkeeping on the side and never affects the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        sample(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Probe = Probe;

const KEY_LEN: usize = 200_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lookup_key_copy_is_charged_until_it_is_freed() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..8u8 {
        builder.append(table, &[b'a', i], b"v").unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(2 << 20)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    // Leaked so the probe can reach it from inside the allocator without a lock.
    let async_db: &'static AsyncDatabase = Box::leak(Box::new(AsyncDatabase::from_database(db)));
    let _ = DB.set(async_db.sync());
    let db = async_db.sync();

    let table = async_db.table("t").unwrap();
    // Long, absent, and not cached, so the lookup takes the slow path and copies it.
    let key = vec![b'z'; KEY_LEN];

    BASELINE.store(db.charged_bytes() as i64, Ordering::Relaxed);
    TARGET.store(KEY_LEN, Ordering::Relaxed);
    LEAST.store(i64::MAX, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    let found = table.get(&key).await.unwrap();
    ARMED.store(false, Ordering::Relaxed);
    TARGET.store(0, Ordering::Relaxed);
    assert!(found.is_none());

    let least = LEAST.load(Ordering::Relaxed);
    assert_ne!(least, i64::MAX, "no copy of the key was made");
    assert!(
        least >= KEY_LEN as i64,
        "a {KEY_LEN} byte key copy was freed with only {least} bytes charged for it"
    );
}
