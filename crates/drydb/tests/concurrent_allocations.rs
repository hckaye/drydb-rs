//! What concurrent reads leave behind.
//!
//! The counting allocator here is global rather than per thread, because what it measures
//! happens on threads the test does not own: a wave of page loads, each of which puts a
//! marker in the cache while it runs. That makes this file single-threaded, so it holds
//! one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};

use drydb::{DatabaseBuilder, Int64Encoding, MemorySource, OpenOptions, PageSource};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicI64 = AtomicI64::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; the counter is
// bookkeeping on the side and never affects the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ENABLED.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            LIVE.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A source that parks every reader until enough of them are waiting, then refuses.
///
/// Parking is what makes the markers overlap: without it each load finishes before the
/// next begins and the cache never holds more than one at a time.
struct Gate {
    inner: MemorySource,
    fail: AtomicBool,
    waiting: Mutex<usize>,
    released: Condvar,
    target: usize,
}

impl PageSource for Gate {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        if !self.fail.load(Ordering::Relaxed) {
            return self.inner.read_at(buf, offset);
        }
        let mut waiting = self.waiting.lock().unwrap();
        *waiting += 1;
        if *waiting >= self.target {
            self.released.notify_all();
        } else {
            // Bounded, so a wave that never reaches the target still ends.
            let (guard, _) = self
                .released
                .wait_timeout(waiting, std::time::Duration::from_millis(50))
                .unwrap();
            waiting = guard;
        }
        drop(waiting);
        Err(std::io::Error::other("refused"))
    }

    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }
}

/// Each concurrent load puts a marker in the cache's map, one per caller, and nothing
/// else stands for it until the page arrives. Those markers are reserved against the
/// budget and the map is shrunk when a failed load removes one.
///
/// What this test establishes is that a wave of concurrent failures leaves the process
/// where it started. It does not, on its own, pin either half of that: with the
/// reservation removed it still passes, because the map is also shrunk when reclaiming
/// evicts from it. A fixture that separates the two has not been written.
#[test]
fn a_wave_of_failed_loads_leaves_nothing_behind() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..256i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[b'v'; 80])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let threads = 64usize;

    let source = Arc::new(Gate {
        inner: MemorySource::new(bytes),
        fail: AtomicBool::new(false),
        waiting: Mutex::new(0),
        released: Condvar::new(),
        target: threads,
    });
    let budget = 16 * 1024u64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        // Room for the upper levels, so the threads get past them and each fails on a
        // different leaf, which is what puts many markers in the map at once.
        .cache_capacity(8 * 1024)
        .cache_shards(1)
        .open_source(Arc::clone(&source) as Arc<dyn PageSource>)
        .unwrap();
    let table = db.table("t").unwrap();
    // Warm the pages above the leaves, so the failures below them are spread out.
    for i in 0..256i64 {
        let _ = table.get(&Int64Encoding::encode(i));
    }

    source.fail.store(true, Ordering::Relaxed);
    let barrier = Arc::new(Barrier::new(threads));
    LIVE.store(0, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
    std::thread::scope(|s| {
        for i in 0..threads {
            let table = &table;
            let barrier = Arc::clone(&barrier);
            s.spawn(move || {
                barrier.wait();
                let _ = table.get(&Int64Encoding::encode(i as i64 * 2));
            });
        }
    });
    ENABLED.store(false, Ordering::Relaxed);
    source.fail.store(false, Ordering::Relaxed);

    let residue = LIVE.load(Ordering::Relaxed);
    // Nothing succeeded, so nothing should be resident: what the markers took is what
    // this measures.
    assert!(
        residue <= 2048,
        "{threads} failed loads left {residue} bytes behind"
    );
    let _ = budget;
    assert!(table.get(&Int64Encoding::encode(0)).unwrap().is_some());
}
