//! That the budget never says memory is free while it is still held.
//!
//! Every buffer this crate makes on a caller's behalf is reserved before it is allocated
//! and released after it is freed. Getting the second half backwards opens a window in
//! which the budget reports room that is not there, and a concurrent reservation taken in
//! that window puts the two of them together over the limit.
//!
//! The probe here is a key encoding, because comparing the two ends of a range is the
//! first thing a cursor does with them: it runs inside the window, where it can weigh
//! what the budget says against what has actually been allocated. The allocator is
//! global, so this file holds one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use drydb::{
    Database, DatabaseBuilder, KeyEncoding, MemorySource, OpenOptions, Order, PageSource, Result,
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicI64 = AtomicI64::new(0);

fn add(bytes: i64) {
    if ENABLED.load(Ordering::Relaxed) {
        LIVE.fetch_add(bytes, Ordering::Relaxed);
    }
}

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; the counter is
// bookkeeping on the side and never affects the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as i64);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        add(-(layout.size() as i64));
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size as i64 - layout.size() as i64);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Byte-ordered keys of any length, which weighs the budget against what is allocated
/// every time it is asked to compare two of them.
#[derive(Debug, Default)]
struct Probe {
    db: Mutex<Option<Weak<Database>>>,
    /// The most the allocations have ever run ahead of the budget, in bytes.
    excess: AtomicI64,
    base_live: AtomicI64,
    base_charged: AtomicI64,
}

impl Probe {
    fn arm(&self, db: &Arc<Database>) {
        *self.db.lock().unwrap() = Some(Arc::downgrade(db));
        LIVE.store(0, Ordering::Relaxed);
        self.base_live.store(0, Ordering::Relaxed);
        self.base_charged
            .store(db.memory_report().charged as i64, Ordering::Relaxed);
        self.excess.store(0, Ordering::Relaxed);
        ENABLED.store(true, Ordering::Relaxed);
    }

    fn sample(&self) {
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }
        let Some(db) = self
            .db
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|weak| weak.upgrade())
        else {
            return;
        };
        let live = LIVE.load(Ordering::Relaxed) - self.base_live.load(Ordering::Relaxed);
        let charged = db.memory_report().charged as i64 - self.base_charged.load(Ordering::Relaxed);
        self.excess.fetch_max(live - charged, Ordering::Relaxed);
    }
}

impl KeyEncoding for Probe {
    fn id(&self) -> &str {
        "test.probe"
    }

    fn validate_key(&self, _key: &[u8]) -> Result<()> {
        Ok(())
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering> {
        self.sample();
        Ok(a.cmp(b))
    }

    fn digest(&self, key: &[u8]) -> Result<u64> {
        let mut out = [0u8; 8];
        let take = key.len().min(8);
        out[..take].copy_from_slice(&key[..take]);
        Ok(u64::from_be_bytes(out))
    }

    fn is_byte_lexicographic(&self) -> bool {
        true
    }
}

#[test]
fn a_prefix_scan_never_holds_more_than_it_has_reserved() {
    let probe = Arc::new(Probe::default());

    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder
        .create_table("t", Arc::clone(&probe) as Arc<dyn KeyEncoding>)
        .unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::clone(&probe) as Arc<dyn KeyEncoding>,
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    for i in 0..8u8 {
        let mut key = vec![b'k'; 64];
        key[0] = i;
        builder.append(table, &key, b"v").unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn PageSource> = Arc::new(MemorySource::new(bytes));

    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(8 << 20)
            .register_encoding(Arc::clone(&probe) as Arc<dyn KeyEncoding>)
            .unwrap()
            .open_source(source)
            .unwrap(),
    );
    let table = db.table("t").unwrap();
    let index = table.index("by_key").unwrap();

    // Long enough that a copy of it left unaccounted for stands out against everything
    // else a cursor does.
    let prefix = vec![0u8; 200_000];

    probe.arm(&db);
    let _cursor = index.prefix(&prefix, Order::Ascending).unwrap();
    let _table_cursor = table.prefix(&prefix, Order::Ascending).unwrap();
    ENABLED.store(false, Ordering::Relaxed);

    let excess = probe.excess.load(Ordering::Relaxed);
    assert!(
        excess < 50_000,
        "the budget was short of what was allocated by {excess} bytes while a prefix \
         cursor was being built"
    );
}
