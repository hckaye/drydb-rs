//! That a reservation outlives the buffer it stands for.
//!
//! A reservation released while the memory it stood for is still held leaves the budget
//! reporting room that is not there, which a concurrent reservation would take. Rust
//! drops locals in reverse order of declaration, so a charge declared after the buffer it
//! covers is released first.
//!
//! The probe is the allocator itself: when a buffer of exactly the size the keys here
//! are is freed, it asks the budget what it thinks is charged. The allocator is global,
//! so this file holds one test, which walks the paths that copy a key in turn.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use std::ops::Bound;

use drydb::{
    AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order, VerifyOptions,
};

/// The buffer size to watch for, zero when nothing is being watched.
static TARGET: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
/// What the budget said was charged, beyond the baseline, the least it ever said at one
/// of those frees. `i64::MAX` when none has happened.
static LEAST: AtomicI64 = AtomicI64::new(i64::MAX);
static BASELINE: AtomicI64 = AtomicI64::new(0);

fn database() -> &'static Mutex<Option<Weak<Database>>> {
    static DB: OnceLock<Mutex<Option<Weak<Database>>>> = OnceLock::new();
    DB.get_or_init(|| Mutex::new(None))
}

fn sample(size: usize) {
    if !ARMED.load(Ordering::Relaxed) || size != TARGET.load(Ordering::Relaxed) {
        return;
    }
    // Off while the budget is read, so reading it cannot set the probe off again.
    ARMED.store(false, Ordering::Relaxed);
    if let Some(db) = database()
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|weak| weak.upgrade())
    {
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

const KEY_LEN: usize = 32_768;

/// Arms the probe, runs `body`, and returns the least the budget ever said was charged
/// at one of those frees.
fn watch(db: &Database, body: impl FnOnce()) -> i64 {
    watch_size(db, KEY_LEN, body)
}

/// The same, for a buffer of some other size.
fn watch_size(db: &Database, size: usize, body: impl FnOnce()) -> i64 {
    BASELINE.store(db.charged_bytes() as i64, Ordering::Relaxed);
    TARGET.store(size, Ordering::Relaxed);
    LEAST.store(i64::MAX, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    body();
    ARMED.store(false, Ordering::Relaxed);
    TARGET.store(0, Ordering::Relaxed);
    LEAST.load(Ordering::Relaxed)
}

#[test]
fn a_key_buffer_is_charged_until_it_is_freed() {
    let mut builder = DatabaseBuilder::new().page_size(100_000).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    // A unique index on the first byte, so verification resolves each reference back to
    // the record it names, copying the primary key to do it.
    builder
        .add_secondary_index(
            table,
            "by_lead",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key[..1].to_vec())),
        )
        .unwrap();
    for lead in *b"ab" {
        let mut key = vec![b'k'; KEY_LEN];
        key[0] = lead;
        builder.append(table, &key, b"v").unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(2 << 20)
            .cache_shards(1)
            .open_bytes(bytes)
            .unwrap(),
    );
    // Warm the pages, so what the walk reserves is the keys and not the file.
    let table = db.table("t").unwrap();
    let mut probe_key = vec![b'k'; KEY_LEN];
    probe_key[0] = b'a';
    assert!(table.get(&probe_key).unwrap().is_some());
    drop(probe_key);

    *database().lock().unwrap() = Some(Arc::downgrade(&db));

    let mut report = None;
    let least = watch(&db, || {
        report = Some(
            db.verify(VerifyOptions {
                check_pages: false,
                ..VerifyOptions::default()
            })
            .unwrap(),
        );
    });
    assert!(report.as_ref().is_some_and(|r| r.is_ok()), "{report:?}");
    assert_ne!(least, i64::MAX, "the walk copied no key of that size");
    assert!(
        least >= KEY_LEN as i64,
        "verification freed a {KEY_LEN} byte key buffer with only {least} bytes charged"
    );

    // A range whose ends are the wrong way round is refused, and the copies it made of
    // them have to stay charged until they go, on that path as much as on the one that
    // builds a cursor.
    let low = vec![b'a'; KEY_LEN];
    let high = vec![b'z'; KEY_LEN];
    let mut refused = None;
    let least = watch(&db, || {
        refused = Some(table.range(
            Bound::Included(&high[..]),
            Bound::Included(&low[..]),
            Order::Ascending,
        ));
    });
    assert!(refused.is_some_and(|r| r.is_err()), "the range is refused");
    assert_ne!(least, i64::MAX, "the range copied no bound of that size");
    assert!(
        least >= KEY_LEN as i64,
        "a refused range freed a {KEY_LEN} byte bound copy with only {least} bytes charged"
    );

    drop(table);
    drop(db);

    // A page that fails its checks is freed on the way out of the read, and the
    // reservation that paid for it has to outlive it there too. The two were named in
    // one `let`, where Rust drops the second first, so the reservation went first.
    let mut bytes = build_pages();
    let leaf = first_leaf(&bytes);
    let page_len = page_length(&bytes, leaf);
    // Damage the read finds with the buffer and its reservation both in hand: a node
    // kind the format does not have, which the page fails to parse on.
    let at = page_offset(&bytes, leaf) + 4;
    bytes[at..at + 4].copy_from_slice(&0x0000_7f00i32.to_le_bytes());

    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(2 << 20)
            .cache_shards(1)
            .open_bytes(bytes)
            .unwrap(),
    );
    *database().lock().unwrap() = Some(Arc::downgrade(&db));
    let table = db.table("t").unwrap();

    let mut outcome = None;
    let least = watch_size(&db, page_len, || {
        outcome = Some(table.get(&Int64Encoding::encode(0)));
    });
    assert!(
        outcome.is_some_and(|r| r.is_err()),
        "the page does not pass its checks"
    );
    assert_ne!(least, i64::MAX, "no page buffer of that size was freed");
    assert!(
        least >= page_len as i64,
        "a failed read freed a {page_len} byte page with only {least} bytes charged"
    );
}

/// A file whose leaves are full pages, so the buffer a read allocates is a size worth
/// watching for.
fn build_pages() -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..200i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[b'v'; 64])
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

fn page_offset(bytes: &[u8], ordinal: u64) -> usize {
    let directory = i64::from_le_bytes(bytes[18..26].try_into().unwrap()) as usize;
    let slot = directory + ordinal as usize * 8;
    i64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap()) as usize
}

fn page_length(bytes: &[u8], ordinal: u64) -> usize {
    let at = page_offset(bytes, ordinal);
    i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn first_leaf(bytes: &[u8]) -> u64 {
    let db = Database::open_bytes(bytes.to_vec()).unwrap();
    let table = db.table("t").unwrap();
    let mut cursor = table.scan(Order::Ascending).unwrap();
    cursor.advance().unwrap();
    cursor.current().unwrap().page().get()
}
