//! What the hot read paths allocate.
//!
//! The design goal is that a point lookup on cached pages, and each step of a scan over
//! inline values, allocate nothing at all: the value is borrowed from the page buffer
//! and the only bookkeeping is a reference count. This test measures that with a
//! counting allocator rather than asserting it in a comment.
//!
//! Cold reads are a different matter and are not covered here: a cache miss allocates
//! the page buffer, which is the whole point of the memory budget.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ops::Bound;
use std::sync::Arc;

use drydb::{AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order};

// Counters are per thread, because the test harness runs these tests in parallel in one
// process and a global counter would measure whatever the neighbours were doing.
thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE_BYTES: Cell<i64> = const { Cell::new(0) };
    static PEAK_LIVE_BYTES: Cell<i64> = const { Cell::new(0) };
}

fn record(size: usize) {
    // `try_with` because this can run while thread locals are being destroyed.
    let _ = ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
            let _ = ALLOCATED_BYTES.try_with(|n| n.set(n.get() + size as u64));
            let _ = LIVE_BYTES.try_with(|live| {
                let now = live.get() + size as i64;
                live.set(now);
                let _ = PEAK_LIVE_BYTES.try_with(|peak| {
                    if now > peak.get() {
                        peak.set(now);
                    }
                });
            });
        }
    });
}

fn record_free(size: usize) {
    let _ = ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = LIVE_BYTES.try_with(|live| live.set(live.get() - size as i64));
        }
    });
}

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; the counters are
// bookkeeping on the side and never affect the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_free(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Counted as a free of the old block and an allocation of the new one, so that
        // live bytes and the peak stay honest across a growing `Vec`.
        record_free(layout.size());
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `body` with allocation counting on, and reports what this thread allocated.
fn measure<R>(body: impl FnOnce() -> R) -> (R, u64, u64) {
    ALLOCATIONS.with(|n| n.set(0));
    ALLOCATED_BYTES.with(|n| n.set(0));
    ENABLED.with(|e| e.set(true));
    let result = body();
    ENABLED.with(|e| e.set(false));
    (
        result,
        ALLOCATIONS.with(|n| n.get()),
        ALLOCATED_BYTES.with(|n| n.get()),
    )
}

/// Runs `body` and reports the high-water mark of memory live at any one moment.
///
/// Counts only this thread's allocations, and reads slightly low when something frees
/// memory it allocated before the measurement started; a bound checked against it should
/// have room to spare.
fn measure_peak<R>(body: impl FnOnce() -> R) -> (R, u64) {
    LIVE_BYTES.with(|n| n.set(0));
    PEAK_LIVE_BYTES.with(|n| n.set(0));
    ENABLED.with(|e| e.set(true));
    let result = body();
    ENABLED.with(|e| e.set(false));
    (result, PEAK_LIVE_BYTES.with(|n| n.get()).max(0) as u64)
}

/// What is allocated and not yet freed, at the moment it is asked.
fn live_bytes() -> u64 {
    LIVE_BYTES.with(|n| n.get()).max(0) as u64
}

fn build(rows: i64) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..rows {
        builder
            .append(
                table,
                &Int64Encoding::encode(i),
                format!("value-{i:08}").as_bytes(),
            )
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

#[test]
fn a_cached_point_lookup_allocates_nothing() {
    let rows = 2000i64;
    let db = OpenOptions::new()
        .memory_budget(32 << 20)
        // Room for the whole file, so nothing is evicted mid-measurement.
        .cache_capacity(16 << 20)
        .open_bytes(build(rows))
        .unwrap();
    let table = db.table("t").unwrap();

    // Warm every page first; that part does allocate.
    for i in 0..rows {
        assert!(table.get(&Int64Encoding::encode(i)).unwrap().is_some());
    }

    let (found, allocations, bytes) = measure(|| {
        let mut found = 0u64;
        for i in 0..rows {
            if let Some(value) = table.get(&Int64Encoding::encode(i)).unwrap() {
                // Consume the value so the lookup cannot be optimised away.
                found += value.as_bytes()[0] as u64;
            }
        }
        found
    });
    assert!(found > 0);
    assert_eq!(
        allocations, 0,
        "{rows} cached lookups allocated {allocations} times ({bytes} bytes)"
    );
}

#[test]
fn a_warm_scan_step_allocates_nothing() {
    let rows = 2000i64;
    let db = OpenOptions::new()
        .memory_budget(32 << 20)
        .cache_capacity(16 << 20)
        .open_bytes(build(rows))
        .unwrap();
    let table = db.table("t").unwrap();

    // Warm the pages, then measure a scan that only steps over cached ones.
    {
        let mut cursor = table.scan(Order::Ascending).unwrap();
        while cursor.advance().unwrap() {}
    }

    let mut cursor = table.scan(Order::Ascending).unwrap();
    // The first step positions the cursor and grows its key buffer once.
    assert!(cursor.advance().unwrap());
    let _ = cursor.current().unwrap().key().len();

    let (seen, allocations, bytes) = measure(|| {
        let mut seen = 0u64;
        while cursor.advance().unwrap() {
            let entry = cursor.current().expect("positioned");
            seen += entry.key().len() as u64 + entry.value().len() as u64;
        }
        seen
    });
    assert!(seen > 0);
    assert_eq!(
        allocations, 0,
        "a warm scan allocated {allocations} times ({bytes} bytes)"
    );
}

#[test]
fn a_warm_count_allocates_nothing() {
    let db = OpenOptions::new()
        .memory_budget(32 << 20)
        .cache_capacity(16 << 20)
        .open_bytes(build(2000))
        .unwrap();
    let table = db.table("t").unwrap();
    assert_eq!(table.count().unwrap(), 2000);

    let (count, allocations, _) = measure(|| {
        table
            .count_range(
                Bound::Included(&Int64Encoding::encode(100)[..]),
                Bound::Excluded(&Int64Encoding::encode(1900)[..]),
            )
            .unwrap()
    });
    assert_eq!(count, 1800);
    assert_eq!(allocations, 0, "a warm count allocated {allocations} times");
}

#[test]
fn a_cold_lookup_allocates_one_page_at_a_time() {
    // The counterpart to the tests above: a miss does allocate, and what it allocates is
    // page sized rather than result sized.
    let db = OpenOptions::new()
        .memory_budget(32 << 20)
        .cache_capacity(8 * 1024)
        .open_bytes(build(4000))
        .unwrap();
    let table = db.table("t").unwrap();

    let (_, allocations, bytes) = measure(|| table.get(&Int64Encoding::encode(2000)).unwrap());
    assert!(allocations > 0, "a cold lookup has to read pages");
    assert!(
        bytes < 128 * 1024,
        "a cold lookup allocated {bytes} bytes, far more than the pages on its path"
    );
}

/// The sort buffer is meant to bound what a build holds, whatever the input looks like.
///
/// The merge used to keep one whole record per run, and a build whose values each
/// exceed the buffer spills one record per run, so the merge ended up holding the entire
/// input. Now it keeps one key per run and reads each value as its record is emitted,
/// and merges no more runs at once than the fan-in allows.
#[test]
fn a_build_with_a_small_sort_buffer_stays_bounded() {
    let dir = std::env::temp_dir().join(format!("drydb-build-bound-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.drydb");

    let rows = 128i64;
    let value_len = 64 * 1024;
    let sort_buffer = 64 * 1024;

    let (report, peak) = measure_peak(|| {
        let mut builder = DatabaseBuilder::new()
            .page_size(4096)
            .unwrap()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
        let value = vec![0x5Au8; value_len];
        for i in 0..rows {
            // Reverse order, so the sorter has real work to do.
            builder
                .append(table, &Int64Encoding::encode(rows - i), &value)
                .unwrap();
        }
        builder.build_to_file(&path).unwrap()
    });

    assert_eq!(report.tables[0].rows, rows as u64);
    let input_bytes = rows as u64 * value_len as u64;
    // Comfortably below the whole input, which is what the bound is about. The build
    // needs the sort buffer, a bounded number of pending keys, one value at a time and
    // the page it is filling.
    let ceiling = 2 * 1024 * 1024;
    assert!(
        peak < ceiling,
        "a build of {input_bytes} bytes of values peaked at {peak} live bytes, above {ceiling}"
    );

    let db = Database::open_bytes(std::fs::read(&path).unwrap()).unwrap();
    let table = db.table("t").unwrap();
    assert_eq!(table.count().unwrap(), rows as u64);
    assert_eq!(
        table.get(&Int64Encoding::encode(1)).unwrap().unwrap().len(),
        value_len
    );
    drop(db);
    std::fs::remove_dir_all(&dir).ok();
}

/// Opening used to reserve the catalog's memory *after* parsing it, so a file with more
/// descriptors than the budget allows allocated all of them before reporting the budget
/// error. The reservation now happens string by string, as the parse goes.
#[test]
fn a_catalog_larger_than_the_budget_fails_before_it_is_allocated() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    for i in 0..100 {
        builder
            .create_table(format!("{i:03}"), Arc::new(Int64Encoding))
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    // Built outside the measurement: the file is the input, not something the open
    // allocates.
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 16 * 1024u64;
    let (result, peak) = measure_peak(|| {
        OpenOptions::new()
            .memory_budget(budget)
            .cache_capacity(0)
            .open_source(Arc::clone(&source))
    });
    assert_eq!(
        result.unwrap_err().kind(),
        drydb::ErrorKind::BudgetExceeded,
        "a hundred descriptors should not fit in {budget} bytes"
    );
    assert!(
        peak <= budget,
        "the failed open allocated {peak} bytes inside a {budget} byte budget"
    );

    // With room the same file opens, and every table is there.
    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .open_source(Arc::clone(&source))
        .unwrap();
    assert_eq!(db.catalog().tables.len(), 100);
}

/// The page cache's shard array is allocated once per database and its size comes from a
/// caller's setting, so it used to spend memory the budget never heard about: a thousand
/// shards inside a sixteen kilobyte budget allocated well over a hundred kilobytes.
#[test]
fn the_cache_shard_array_stays_inside_the_budget() {
    // Small pages, so a sixteen kilobyte budget is enough to run a search in.
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..64i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[i as u8])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 16 * 1024u64;
    for shards in [1usize, 256, 1024] {
        let (db, peak) = measure_peak(|| {
            OpenOptions::new()
                .memory_budget(budget)
                .cache_shards(shards)
                .open_source(Arc::clone(&source))
                .expect("the database opens")
        });
        assert!(
            peak <= budget,
            "opening with {shards} shards allocated {peak} bytes inside a {budget} byte budget"
        );
        // The file still reads.
        assert!(db
            .table("t")
            .unwrap()
            .get(&Int64Encoding::encode(7))
            .unwrap()
            .is_some());
    }

    // A budget with room keeps the shards the caller asked for, so the cap is about the
    // budget and not a blanket limit.
    let (db, _) = measure_peak(|| {
        OpenOptions::new()
            .memory_budget(64 << 20)
            .cache_shards(1024)
            .open_source(Arc::clone(&source))
            .expect("the database opens")
    });
    assert!(db.memory_report().charged >= 1024 * 64);
}

/// A range bound is a caller's key of any length, and a query copies it to own it. That
/// copy used to happen outside the budget, so a query with a megabyte bound spent two
/// megabytes the memory report never mentioned.
#[test]
fn range_bounds_are_charged_to_the_budget() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..32u8 {
        let key = format!("k{i:03}");
        builder.append(table, key.as_bytes(), &[i]).unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 64 * 1024u64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .open_source(source)
        .unwrap();
    let table = db.table("t").unwrap();
    let huge = vec![b'k'; 1 << 20];

    let (result, peak) = measure_peak(|| {
        table.range(
            Bound::Included(&huge[..]),
            Bound::Included(&huge[..]),
            Order::Ascending,
        )
    });
    assert_eq!(
        result.unwrap_err().kind(),
        drydb::ErrorKind::BudgetExceeded,
        "a megabyte of bounds does not fit in a {budget} byte budget"
    );
    assert!(
        peak <= budget,
        "the refused query allocated {peak} bytes inside a {budget} byte budget"
    );

    // A bound the budget can hold is charged for as long as the cursor lives.
    let modest = vec![b'k'; 4096];
    let cursor = table
        .range(
            Bound::Included(&modest[..]),
            Bound::Unbounded,
            Order::Ascending,
        )
        .unwrap();
    assert!(db.memory_report().charged >= 4096);
    drop(cursor);
}

/// A secondary cursor keeps a copy of the current index key. That copy was outside the
/// budget, so a caller holding many cursors held memory the report never mentioned.
#[test]
fn index_cursor_keys_are_charged_to_the_budget() {
    let mut builder = DatabaseBuilder::new().page_size(32767).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_value",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|_key: &[u8], value: &[u8]| Ok(value.to_vec())),
        )
        .unwrap();
    // Two keys one byte apart in length, so a cursor that keeps the first has to grow
    // its buffer for the second.
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'k'; 8192])
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), &vec![b'k'; 8193])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();
    let db_bytes = bytes.clone();

    let db = OpenOptions::new()
        .memory_budget(4 << 20)
        .open_bytes(bytes)
        .unwrap();
    let index = db.table("t").unwrap().index("by_value").unwrap();

    // Warm the pages first, so what the loop below adds is the cursors' own memory.
    let mut warm = index.scan(Order::Ascending).unwrap();
    assert!(warm.advance().unwrap());
    drop(warm);
    let before = db.memory_report().charged;

    let (held, peak) = measure_peak(|| {
        let mut held = Vec::new();
        for _ in 0..32 {
            let mut cursor = index.scan(Order::Ascending).unwrap();
            assert!(cursor.advance().unwrap());
            assert_eq!(cursor.key().len(), 8192);
            // The second row is a byte longer, which is where a `Vec` would double.
            assert!(cursor.advance().unwrap());
            assert_eq!(cursor.key().len(), 8193);
            held.push(cursor);
        }
        held
    });
    let after = db.memory_report().charged;
    let charged = after - before;
    assert!(
        charged >= 32 * 8193,
        "32 cursors holding an 8 KiB key each only added {charged} bytes"
    );
    // What the cursors really hold, against what they were charged. A buffer that grows
    // by doubling allocates twice what its contents need, and charging the contents
    // instead of the capacity is how that memory used to escape.
    assert!(
        peak <= charged + 64 * 1024,
        "the cursors allocated {peak} bytes and were charged {charged}"
    );
    drop(held);

    // And a budget that cannot hold them says so rather than growing past it.
    let db = OpenOptions::new()
        .memory_budget(256 * 1024)
        .open_bytes(db_bytes)
        .unwrap();
    let index = db.table("t").unwrap().index("by_value").unwrap();
    let mut held = Vec::new();
    let refused = loop {
        let mut cursor = index.scan(Order::Ascending).unwrap();
        match cursor.advance().and_then(|_| cursor.advance()) {
            Ok(true) => held.push(cursor),
            Ok(false) => panic!("the index has rows"),
            Err(e) => break e,
        }
    };
    assert_eq!(refused.kind(), drydb::ErrorKind::BudgetExceeded);
    assert!(db.memory_report().charged <= 256 * 1024);
}

/// Verification rebuilds every key of a page that omits them, and keeps a copy of the
/// previous key to compare against. Both are a caller's keys of a caller's length, and
/// both used to be allocated outside the budget, so a pass over a file with large keys
/// ran past the limit and still reported success.
#[test]
fn verification_key_buffers_are_charged_to_the_budget() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(WideKey)).unwrap();
    builder.append(table, &vec![b'x'; 60_000], b"v").unwrap();
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 32 * 1024u64;
    let db = OpenOptions::new()
        .register_encoding(Arc::new(WideKey))
        .unwrap()
        .memory_budget(budget)
        .open_source(source)
        .unwrap();

    let (result, peak) = measure_peak(|| db.verify(Default::default()));
    assert!(
        result.is_err(),
        "a 60,000 byte key does not fit in a {budget} byte budget"
    );
    assert!(
        peak <= budget,
        "verification allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// A key encoding whose keys are runs of `x` as long as their digest says, so a page can
/// omit them and the rebuild produces a key of any size the file asks for.
#[derive(Debug)]
struct WideKey;

impl drydb::KeyEncoding for WideKey {
    fn id(&self) -> &str {
        "wide-key"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering, drydb::Error> {
        Ok(a.len().cmp(&b.len()))
    }

    fn digest(&self, key: &[u8]) -> Result<u64, drydb::Error> {
        Ok(key.len() as u64)
    }

    fn is_digest_exact(&self) -> bool {
        true
    }

    fn max_rebuilt_key_len(&self) -> Option<usize> {
        Some(u16::MAX as usize)
    }

    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<(), drydb::Error> {
        out.clear();
        out.resize(digest as usize, b'x');
        Ok(())
    }
}

/// `BlobReader::copy_to` allocates a buffer of the caller's chunk size, which used to
/// happen outside the budget: a megabyte chunk on a database opened with sixteen
/// kilobytes allocated the megabyte and reported nothing.
#[test]
fn a_blob_copy_buffer_is_charged_to_the_budget() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'v'; 1024])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 16 * 1024u64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .open_source(source)
        .unwrap();
    let table = db.table("t").unwrap();
    let reader = table
        .blob_reader(&Int64Encoding::encode(0))
        .unwrap()
        .expect("the value is on a blob page");

    let (result, peak) = measure_peak(|| reader.copy_to(&mut std::io::sink(), 1 << 20));
    assert_eq!(
        result.unwrap_err().kind(),
        drydb::ErrorKind::BudgetExceeded,
        "a megabyte chunk does not fit in a {budget} byte budget"
    );
    assert!(
        peak <= budget,
        "the refused copy allocated {peak} bytes inside a {budget} byte budget"
    );

    // A chunk the budget can hold still copies the whole value.
    let mut out = Vec::new();
    assert_eq!(reader.copy_to(&mut out, 512).unwrap(), 1024);
    assert_eq!(out, vec![b'v'; 1024]);
}

/// Verification follows an index entry whose reference went to a blob page, and a damaged
/// entry can send it at a page of any size. Copying that page's payload to look at the
/// sixteen bytes of a reference spent memory the budget never heard about.
#[test]
fn verification_does_not_copy_a_blob_to_read_a_reference() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'v'; 1_500_000])
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    drop(db);

    // Mark the index entry as an overflow, which makes its payload a page ordinal: the
    // first eight bytes of the reference, which name the page holding the record.
    let directory = i64::from_le_bytes(bytes[18..26].try_into().unwrap()) as usize;
    let page = {
        let slot = directory + index_root as usize * 8;
        i64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap()) as usize
    };
    let entries = i32::from_le_bytes(bytes[page + 8..page + 12].try_into().unwrap()) as usize;
    let meta = page + 28 + entries * 8;
    let slot = u16::from_le_bytes(bytes[meta..meta + 2].try_into().unwrap());
    bytes[meta..meta + 2].copy_from_slice(&(slot | 0x8000).to_le_bytes());

    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));
    let budget = 2 << 20;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_shards(1)
        .cache_capacity(0)
        .open_source(source)
        .unwrap();

    let (result, peak) = measure_peak(|| db.verify(Default::default()));
    let _ = result;
    assert!(
        peak <= budget,
        "verification allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// The cache's map and clock ring grow with the pages a shard holds and do not shrink on
/// their own, so a shard that filled up and was then evicted down kept the memory it grew
/// into while the per-page reservations that stood for it were gone.
#[test]
fn cache_bookkeeping_shrinks_with_the_pages_it_held() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..40_000u64 {
        builder.append(table, &i.to_be_bytes(), &[1]).unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 2 << 20;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_capacity(budget)
        .cache_shards(1)
        .directory_chunk_entries(1)
        .directory_chunks(1)
        .open_source(source)
        .unwrap();
    let table = db.table("t").unwrap();

    // A bound just under the budget, so the query only fits if the cache really gave
    // back what it stopped using.
    let bound = vec![b'k'; budget as usize - 10_000];
    let (_, peak) = measure_peak(|| {
        let mut cursor = table.scan(Order::Ascending).unwrap();
        while cursor.advance().unwrap() {}
        drop(cursor);
        let _ = table.range(
            Bound::Included(&bound[..]),
            Bound::Unbounded,
            Order::Ascending,
        );
    });
    assert!(
        peak <= budget,
        "the scan and the range allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// The report a verification pass builds grows while the pass runs, so it is reserved as
/// it grows. Bounding how many problems there are and how long each message is was not
/// enough: a hundred of them is tens of kilobytes, which a small budget does not have.
#[test]
fn a_verification_report_stays_inside_the_budget_while_it_is_built() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    // A long table name, which every problem message carries.
    let name = "t".repeat(400);
    let table = builder
        .create_table(name.clone(), Arc::new(Int64Encoding))
        .unwrap();
    for i in 0..1000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[b'v'; 60])
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    // Break every left sibling link, so every leaf reports.
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let pages = db.catalog().page_count;
    drop(db);
    let directory = i64::from_le_bytes(bytes[18..26].try_into().unwrap()) as usize;
    for ordinal in 0..pages {
        let slot = directory + ordinal as usize * 8;
        let page = i64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap()) as usize;
        bytes[page + 12..page + 20].copy_from_slice(&(-1i64).to_le_bytes());
    }
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 40_960u64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_shards(1)
        .directory_chunk_entries(1)
        .open_source(source)
        .unwrap();

    let (result, peak) = measure_peak(|| db.verify(Default::default()));
    // The pass either fits in the budget or says it does not. What it must not do is
    // run past it: a damaged file with a small budget is exactly where the report
    // competes with the walk that produces it.
    match result {
        Ok(report) => assert!(!report.is_ok(), "the file is damaged"),
        Err(e) => assert_eq!(e.kind(), drydb::ErrorKind::BudgetExceeded, "{e}"),
    }
    assert!(
        peak <= budget,
        "the pass allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// An error message that renders two keys in full is memory spent outside the budget
/// before anything can cut it back down, and a key is as long as the file says it is.
/// Two thirty kilobyte keys came to 660,767 bytes against a 300,000 byte budget.
#[test]
fn error_messages_do_not_render_whole_keys() {
    let mut builder = DatabaseBuilder::new().page_size(65535).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    let mut first = vec![0xffu8; 29_999];
    first.push(b'1');
    let mut second = vec![0xffu8; 29_999];
    second.push(b'2');
    builder.append(table, &first, b"a").unwrap();
    builder.append(table, &second, b"b").unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    drop(db);

    // Put the first key above the second. They share a digest, so each still matches its
    // own, and the order check reports it with both keys in the message.
    let at = bytes
        .windows(30_000)
        .position(|w| w[29_999] == b'1' && w[0] == 0xff)
        .expect("the first key is in the file");
    bytes[at + 29_999] = b'3';
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 300_000u64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .open_source(source)
        .unwrap();
    eprintln!("DBGV before={:?}", db.memory_report());
    let (report, peak) = measure_peak(|| match db.verify(Default::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("DBGV err={e} now={:?}", db.memory_report());
            panic!("{e}")
        }
    });
    assert!(!report.is_ok(), "the keys are out of order");
    assert!(
        peak <= budget,
        "the pass allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// A cache spread over many shards keeps a little bookkeeping in each, and a little each
/// adds up: with a floor of thirty-two entries per shard and two hundred and fifty-six
/// shards, an emptied cache still held room for eight thousand entries that nothing was
/// charged for.
#[test]
fn an_emptied_cache_gives_back_all_of_its_bookkeeping() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..6000u32 {
        builder
            .append(table, format!("{i:05}").as_bytes(), b"v")
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 4 << 20;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_capacity(0)
        .cache_shards(256)
        .open_source(source)
        .unwrap();
    let table = db.table("t").unwrap();

    let bound = vec![b'k'; budget as usize - 51_028];
    let (_, peak) = measure_peak(|| {
        let mut cursor = table.scan(Order::Ascending).unwrap();
        while cursor.advance().unwrap() {}
        drop(cursor);
        let _ = table.range(
            Bound::Included(&bound[..]),
            Bound::Unbounded,
            Order::Ascending,
        );
    });
    assert!(
        peak <= budget,
        "the scan and the range allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// A table name is as long as the file says it is, and a message that renders one in
/// full is memory spent outside the budget on the way to reporting that the file is
/// damaged.
#[test]
fn a_duplicate_name_is_reported_without_copying_the_name() {
    // Two descriptors with the same very long name, which the parser refuses.
    let name = "n".repeat(900_000);
    let mut descriptors = Vec::new();
    for _ in 0..2 {
        descriptors.extend_from_slice(&(name.len() as i32).to_le_bytes());
        descriptors.extend_from_slice(name.as_bytes());
        descriptors.extend_from_slice(
            &drydb::format::catalog::encode_index_descriptor(
                "pk",
                "i64",
                true,
                drydb::ValueKind::RawData,
                -1,
            )
            .unwrap(),
        );
        descriptors.extend_from_slice(&0u16.to_le_bytes());
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"DRY\0");
    bytes.push(1);
    bytes.push(4);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&128i32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    let directory_at = bytes.len();
    bytes.extend_from_slice(&0i64.to_le_bytes());
    bytes.extend_from_slice(&descriptors);
    let directory = bytes.len() as i64;
    bytes[directory_at..directory_at + 8].copy_from_slice(&directory.to_le_bytes());
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 4_200_000u64;
    let limits = drydb::Limits {
        max_catalog_bytes: 1_900_000,
        ..Default::default()
    };
    let (result, peak) = measure_peak(|| {
        OpenOptions::new()
            .memory_budget(budget)
            .limits(limits)
            .open_source(Arc::clone(&source))
    });
    let err = result.unwrap_err();
    assert!(err.to_string().len() < 1024, "{}", err.to_string().len());
    assert!(
        peak <= budget,
        "reporting the duplicate name allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// zstd allocates its decompression context through C's allocator, where nothing on the
/// Rust side can see it, so the filter is asked how much it needs and that is reserved
/// around the decode. Without it, a database opened inside a sixteen kilobyte budget
/// quietly used another ninety-six.
#[cfg(feature = "zstd")]
#[test]
fn a_page_filter_reserves_what_it_needs_for_itself() {
    use drydb::PageFilter;

    let mut builder = DatabaseBuilder::new()
        .page_size(256)
        .unwrap()
        .page_filter(Arc::new(drydb::ZstdFilter::default()));
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"x")
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let working = drydb::ZstdFilter::default().working_set_bytes();
    assert!(working >= 64 * 1024, "a decompression context is not free");

    // A budget too small for the filter's own memory is refused at open, with the
    // reason, rather than exceeded at the first read.
    let limits = drydb::Limits {
        max_decoded_page_bytes: 1024,
        ..Default::default()
    };
    let err = OpenOptions::new()
        .memory_budget(16 * 1024)
        .limits(limits)
        .register_filter(Arc::new(drydb::ZstdFilter::default()))
        .open_source(Arc::clone(&source))
        .unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::InvalidArgument);
    assert!(
        err.to_string().contains("the filter needs for itself"),
        "{err}"
    );

    // With room for it, the read works and stays inside the budget.
    let budget = 1 << 20;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .limits(limits)
        .register_filter(Arc::new(drydb::ZstdFilter::default()))
        .open_source(source)
        .unwrap();
    let table = db.table("t").unwrap();
    assert_eq!(
        table
            .get(&Int64Encoding::encode(0))
            .unwrap()
            .unwrap()
            .as_ref(),
        b"x"
    );
    assert!(db.memory_report().peak_charged >= working);
}

/// Verification copies the key of the record an index reference names and then looks it
/// up, which reads more pages while the copy is still held. The reservation used to be
/// released as soon as the copy was made.
#[test]
fn the_key_a_reference_check_copies_stays_charged_while_it_is_used() {
    let mut builder = DatabaseBuilder::new().page_size(65_536).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "i",
            true,
            Arc::new(Int64Encoding),
            Box::new(|_key: &[u8], value: &[u8]| Ok(value.to_vec())),
        )
        .unwrap();
    for i in 0..1000i64 {
        // Thirty kilobyte keys, so the copy is worth accounting for.
        let mut key = format!("{i:06}").into_bytes();
        key.resize(30_000, b'k');
        builder.append(table, &key, &i.to_le_bytes()).unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 300_000u64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_shards(1)
        .open_source(source)
        .unwrap();

    let (result, peak) = measure_peak(|| db.verify(Default::default()));
    // Either the pass fits in the budget or it says it does not; what it must not do is
    // run past it and report success.
    if let Ok(report) = result {
        assert!(
            peak <= budget,
            "verification allocated {peak} bytes inside a {budget} byte budget and \
             reported {} problems",
            report.problems.len()
        );
    }
}

/// The bits that record where a table's overflowed values live are one per page of the
/// file, and only a table with an index needs them. Reserving them for every table made
/// verification of a large file with no index fail on a budget that reads it fine.
#[test]
fn verifying_a_table_without_an_index_needs_no_room_for_reference_bookkeeping() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..20_000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[b'v'; 64])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    // Just above what a search of this file needs, so a bitmap of one bit per page does
    // not fit alongside it.
    let db = OpenOptions::new()
        .memory_budget(14_336)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    assert!(table.get(&Int64Encoding::encode(19_999)).unwrap().is_some());

    let report = db
        .verify(drydb::VerifyOptions {
            check_pages: false,
            ..Default::default()
        })
        .expect("a table with no index needs no page bitmap to verify");
    assert!(report.is_ok(), "{:?}", report.problems);
}

/// A cache spread over many shards keeps a whole table and a whole ring per shard, and
/// the per-page allowance cannot stand in for that when a shard holds one or two pages.
/// Charging a constant per page left the cache holding more than the budget allowed.
#[test]
fn the_cache_charges_its_shard_tables_by_what_they_hold() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..6_000u64 {
        builder.append(table, &i.to_be_bytes(), b"x").unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    // The file and the caller's own copy of the bound are the caller's memory, not the
    // budget's, so they are made before anything is counted.
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));
    let bound = vec![0xffu8; 1_940_908];

    let budget = 2 << 20;
    let (charged, peak) = measure_peak(|| {
        let db = OpenOptions::new()
            .memory_budget(budget)
            .cache_capacity(budget)
            // Many shards holding few pages each: the regime where one table per shard
            // costs far more than the pages in it would suggest.
            .cache_shards(1024)
            .directory_chunk_entries(16)
            .open_source(Arc::clone(&source))
            .unwrap();
        let table = db.table("t").unwrap();
        for i in (0..6_000u64).step_by(60) {
            let _ = table.get(&i.to_be_bytes());
        }
        // A bound just under the budget, held while the budget is read, so nothing it
        // reserved has been given back.
        let cursor = table.range(
            Bound::Included(&bound[..]),
            Bound::Unbounded,
            Order::Ascending,
        );
        let charged = db.memory_report().charged;
        drop(cursor);
        charged
    });
    assert!(
        charged <= budget,
        "the budget says {charged} bytes of {budget} are charged"
    );
    assert!(
        peak <= budget,
        "the cache and the query allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// A `BlobReader` keeps the page store alive after the database is gone. The tables the
/// store keeps for its own bookkeeping used to hold their slots after the entries were
/// removed, charged to nobody, with nothing left that could ever give them back.
#[test]
fn a_store_outliving_its_database_keeps_nothing_uncharged() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    for i in 0..128u32 {
        let table = builder
            .create_table(format!("t{i}"), Arc::new(Int64Encoding))
            .unwrap();
        builder
            .append(table, &Int64Encoding::encode(0), &vec![b'v'; 512])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let source: Arc<dyn drydb::PageSource> = Arc::new(drydb::MemorySource::new(bytes));

    let budget = 1 << 20;
    let (copied, peak) = measure_peak(|| {
        let db = OpenOptions::new()
            .memory_budget(budget)
            .cache_capacity(budget)
            .cache_shards(1)
            .open_source(Arc::clone(&source))
            .unwrap();
        // Every table, so every tree keeps a root, and then all of them go at once.
        let mut readers = Vec::new();
        for i in 0..128u32 {
            let table = db.table(&format!("t{i}")).unwrap();
            readers.push(
                table
                    .blob_reader(&Int64Encoding::encode(0))
                    .unwrap()
                    .expect("the row is there"),
            );
        }
        let kept = readers.swap_remove(0);
        drop(readers);
        drop(db);

        let mut sink = std::io::sink();
        kept.copy_to(&mut sink, budget as usize - 1024).unwrap()
    });
    assert_eq!(copied, 512);
    assert!(
        peak <= budget,
        "a store outliving its database allocated {peak} bytes inside a {budget} byte budget"
    );
}

/// One entry goes into the list of spooled runs per spill, and the list used to be
/// merged down only at the end, so a sort buffer too small for the input grew it in
/// proportion to the input.
#[test]
fn the_list_of_spooled_runs_does_not_grow_with_the_input() {
    let dir = std::env::temp_dir().join(format!("drydb-spool-runs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Every row spills on its own, which is the worst the list can be asked to do.
    let value_len = 64 * 1024;
    let sort_buffer = 64 * 1024;
    let held = |rows: i64| -> u64 {
        let (_, peak) = measure_peak(|| {
            let mut builder = DatabaseBuilder::new()
                .page_size(256)
                .unwrap()
                .sort_buffer(sort_buffer)
                .temp_dir(&dir);
            let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
            let value = vec![0x5Au8; value_len];
            for i in 0..rows {
                builder
                    .append(table, &Int64Encoding::encode(i), &value)
                    .unwrap();
            }
            // Dropped without building, so what is measured is what appending left.
            drop(builder);
        });
        peak
    };

    let small = held(1_024);
    let large = held(8_192);
    // Eight times the rows, and what it took to hold them barely moves. The value and
    // the buffer are the same in both, so any growth here is the list of runs.
    assert!(
        large < small + 64 * 1024,
        "1024 rows took {small} bytes and 8192 took {large}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Asking a filter what it needs can allocate, which is why the open path reserves the
/// question before asking it. Loading a page asked without reserving, so a filter that
/// measures each time it is asked allocated outside the budget on every cache miss.
#[test]
fn asking_a_filter_while_loading_a_page_stays_inside_the_budget() {
    const PROBE: u64 = 16 * 1024;

    #[derive(Debug)]
    struct MeasuringFilter;

    impl drydb::PageFilter for MeasuringFilter {
        fn id(&self) -> &str {
            "test.measuring"
        }
        fn encode(&self, input: &[u8], out: &mut Vec<u8>) -> drydb::Result<()> {
            out.extend_from_slice(input);
            Ok(())
        }
        fn decode(&self, input: &[u8], out: &mut Vec<u8>) -> drydb::Result<()> {
            out.extend_from_slice(input);
            Ok(())
        }
        fn working_set_bytes(&self) -> u64 {
            // Measured by allocating, the way the zstd filter measures its context.
            let probe = vec![0u8; PROBE as usize];
            std::hint::black_box(&probe);
            1024
        }
        fn probe_bytes(&self) -> u64 {
            PROBE
        }
        fn decoded_size_hint(&self, input: &[u8]) -> Option<usize> {
            Some(input.len())
        }
    }

    let filter = Arc::new(MeasuringFilter) as Arc<dyn drydb::PageFilter>;
    let mut builder = DatabaseBuilder::new()
        .page_size(256)
        .unwrap()
        .page_filter(Arc::clone(&filter));
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..64i64 {
        builder
            .append(table, &Int64Encoding::encode(i), b"v")
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let budget = 32_768;
    let limits = drydb::Limits {
        max_decoded_page_bytes: 4096,
        ..Default::default()
    };
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_shards(1)
        .limits(limits)
        .register_filter(Arc::clone(&filter))
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    // Almost all of the budget is spoken for, leaving room for a page but not for the
    // question. What is left has to cover whatever the load allocates.
    let free = 2048;
    let _held = db.reserve(budget - db.charged_bytes() - free).unwrap();

    let (result, peak) = measure_peak(|| table.get(&Int64Encoding::encode(63)));
    assert_eq!(
        result.unwrap_err().kind(),
        drydb::ErrorKind::BudgetExceeded,
        "a budget that cannot hold the question refuses rather than asking anyway"
    );
    assert!(
        peak < PROBE,
        "the load allocated {peak} bytes with {free} bytes of budget left"
    );
}

/// The sort buffer bounds a build whatever the input looks like, and a record costs
/// memory even when its key and value are empty. The spill decision counted the payload
/// alone, so empty records never reached the buffer's size and the sorter grew with the
/// input instead of spilling.
#[test]
fn a_build_of_empty_records_stays_inside_the_sort_buffer() {
    let dir = std::env::temp_dir().join(format!("drydb-empty-records-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let rows = 200_000;
    let sort_buffer = 64 * 1024;

    let (outcome, peak) = measure_peak(|| {
        let mut builder = DatabaseBuilder::new()
            .page_size(4096)
            .unwrap()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
        for _ in 0..rows {
            builder.append(table, b"", b"").unwrap();
        }
        // Every key is the same, so the build refuses; the memory it took to get here is
        // what this test is about.
        builder.build_to_vec()
    });

    assert!(outcome.is_err(), "duplicate keys cannot build a file");
    // The sort buffer, the file writer's own buffer, and room to spare. Without the
    // bookkeeping counted, 200,000 empty records held ten megabytes.
    let ceiling = 2 * 1024 * 1024;
    assert!(
        peak < ceiling,
        "{rows} empty records peaked at {peak} live bytes with a {sort_buffer} byte sort \
         buffer, above {ceiling}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The sort buffer is what a build may hold, not what each table may hold. Every table
/// had a sorter with the whole figure to itself, so what a build held before it started
/// writing grew with the number of tables: sixty-four of them held a megabyte and a half
/// where the caller had asked for sixty-four kilobytes.
#[test]
fn many_tables_share_one_sort_buffer() {
    let dir = std::env::temp_dir().join(format!("drydb-many-tables-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("many.drydb");

    let tables = 64;
    let rows = 128i64;
    let sort_buffer = 64 * 1024;

    let ((held, outcome), _peak) = measure_peak(|| {
        let mut builder = DatabaseBuilder::new()
            .page_size(256)
            .unwrap()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let handles: Vec<_> = (0..tables)
            .map(|t| {
                builder
                    .create_table(format!("t{t}"), Arc::new(Int64Encoding))
                    .unwrap()
            })
            .collect();
        let value = vec![0x33u8; 128];
        // Round robin, so every table is holding rows at the same time.
        for i in 0..rows {
            for handle in &handles {
                builder
                    .append(*handle, &Int64Encoding::encode(i), &value)
                    .unwrap();
            }
        }
        // What the sorters are holding, before the build allocates anything of its own.
        let held = live_bytes();
        (held, builder.build_to_file(&path))
    });

    outcome.unwrap();
    // The shared buffer and what each table costs to have at all, which is under a
    // kilobyte apiece. A buffer per table came to a megabyte and a half.
    let ceiling = 4 * sort_buffer as u64;
    assert!(
        held < ceiling,
        "{tables} tables held {held} bytes with a {sort_buffer} byte sort buffer, above \
         {ceiling}"
    );

    let db = Database::open_bytes(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(db.table("t0").unwrap().count().unwrap(), rows as u64);
    assert_eq!(db.table("t63").unwrap().count().unwrap(), rows as u64);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The sort buffer covers a table's secondary indexes too. Each index had a sorter with
/// the whole figure to itself, so a table with sixty-four indexes held sixty-four
/// buffers while its tree was written.
#[test]
fn many_secondary_indexes_share_one_sort_buffer() {
    let dir = std::env::temp_dir().join(format!("drydb-many-indexes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("indexes.drydb");

    let indexes = 64;
    let rows = 2_000i64;
    let sort_buffer = 64 * 1024;

    let (outcome, peak) = measure_peak(|| {
        let mut builder = DatabaseBuilder::new()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
        for i in 0..indexes {
            builder
                .add_secondary_index(
                    table,
                    format!("i{i}"),
                    false,
                    Arc::new(Int64Encoding),
                    Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
                )
                .unwrap();
        }
        for i in 0..rows {
            builder
                .append(table, &Int64Encoding::encode(i), b"v")
                .unwrap();
        }
        builder.build_to_file(&path)
    });

    outcome.unwrap();
    // What is left is the cost of having sixty-four sorters at all: their temporary
    // files, the writer's buffer and the merge each one runs, which does not grow with
    // the input. A buffer per index did grow with it, from three and a half megabytes at
    // five hundred rows to over four at two thousand.
    let ceiling = 3 * 1024 * 1024;
    assert!(
        peak < ceiling,
        "{indexes} indexes peaked at {peak} live bytes with a {sort_buffer} byte sort \
         buffer, above {ceiling}"
    );

    let db = Database::open_bytes(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(db.table("t").unwrap().count().unwrap(), rows as u64);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A secondary index key cannot be handed to its sorter until the leaf page the row
/// landed on is written, because the sorter stores where it landed. Every key of every
/// index therefore waited for the page, and nothing bounded what they came to: sixteen
/// indexes with a kilobyte of key apiece held megabytes before the first page was full.
#[test]
fn index_keys_waiting_for_a_page_stay_inside_the_sort_buffer() {
    let dir = std::env::temp_dir().join(format!("drydb-pending-keys-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pending.drydb");

    let indexes = 16;
    let rows = 256i64;
    let sort_buffer = 64 * 1024;

    let (outcome, peak) = measure_peak(|| {
        let mut builder = DatabaseBuilder::new()
            .page_size(4096)
            .unwrap()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
        for i in 0..indexes {
            builder
                .add_secondary_index(
                    table,
                    format!("i{i}"),
                    false,
                    Arc::new(AsciiEncoding),
                    Box::new(|_key: &[u8], _value: &[u8]| Ok(vec![b'a'; 1024])),
                )
                .unwrap();
        }
        for i in 0..rows {
            builder
                .append(table, &Int64Encoding::encode(i), b"")
                .unwrap();
        }
        builder.build_to_file(&path)
    });

    outcome.unwrap();
    // The keys waiting for a page, the sorters they go into, and what writing the file
    // costs. Holding every key until the page filled came to five and a half megabytes.
    let ceiling = 3 * 1024 * 1024;
    assert!(
        peak < ceiling,
        "{indexes} indexes peaked at {peak} live bytes with a {sort_buffer} byte sort \
         buffer, above {ceiling}"
    );

    let db = Database::open_bytes(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(db.table("t").unwrap().count().unwrap(), rows as u64);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Lowering the sort buffer after the rows have gone in used to change the number and
/// nothing else: the rows already buffered stayed where they were, and the build sorted
/// all of them in memory. The figure is asked again when the build starts, so what is
/// over it is written out first. The measurement is taken from inside the index key
/// function, which runs while the primary tree is written.
#[test]
fn a_sort_buffer_lowered_after_the_rows_went_in_is_obeyed() {
    let dir = std::env::temp_dir().join(format!("drydb-late-buffer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("late.drydb");

    let rows = 10_000i64;
    let sort_buffer = 64 * 1024;
    let during = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let probe = Arc::clone(&during);

    let (outcome, _peak) = measure_peak(move || {
        let mut builder = DatabaseBuilder::new().temp_dir(&dir);
        let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
        builder
            .add_secondary_index(
                table,
                "i",
                false,
                Arc::new(Int64Encoding),
                Box::new(move |key: &[u8], _value: &[u8]| {
                    probe.fetch_max(live_bytes(), std::sync::atomic::Ordering::Relaxed);
                    Ok(key.to_vec())
                }),
            )
            .unwrap();
        let value = vec![0x11u8; 128];
        for i in 0..rows {
            builder
                .append(table, &Int64Encoding::encode(i), &value)
                .unwrap();
        }
        // Lowered once every row is in.
        let builder = builder.sort_buffer(sort_buffer);
        let outcome = builder.build_to_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
        outcome
    });

    outcome.unwrap();
    let held = during.load(std::sync::atomic::Ordering::Relaxed);
    // The records being merged, the page being filled and the index sorter. All ten
    // thousand rows in memory came to four megabytes.
    let ceiling = 2 * 1024 * 1024;
    assert!(
        held < ceiling,
        "the build held {held} bytes with a {sort_buffer} byte sort buffer, above {ceiling}"
    );
}

/// A table whose rows all fitted in the sort buffer hands them out from memory, and the
/// sorters of its secondary indexes fill up beside them. Only the tables still waiting
/// were taken off what the indexes were given, so the two together went over the figure
/// the caller set.
#[test]
fn a_primary_sort_held_in_memory_leaves_room_for_the_index_sorters() {
    let dir = std::env::temp_dir().join(format!("drydb-primary-share-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("share.drydb");

    let rows = 12_000i64;
    let sort_buffer = 1024 * 1024;
    let during = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let probe = Arc::clone(&during);

    let (outcome, _peak) = measure_peak(move || {
        let mut builder = DatabaseBuilder::new()
            .page_size(256)
            .unwrap()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
        builder
            .add_secondary_index(
                table,
                "i",
                true,
                Arc::new(AsciiEncoding),
                Box::new(move |key: &[u8], _value: &[u8]| {
                    probe.fetch_max(live_bytes(), std::sync::atomic::Ordering::Relaxed);
                    let mut index_key = vec![b'a'; 24];
                    index_key.extend_from_slice(key);
                    Ok(index_key)
                }),
            )
            .unwrap();
        for i in 0..rows {
            builder
                .append(table, &Int64Encoding::encode(i), &Int64Encoding::encode(i))
                .unwrap();
        }
        let outcome = builder.build_to_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
        outcome
    });

    outcome.unwrap();
    let held = during.load(std::sync::atomic::Ordering::Relaxed);
    // The reading takes in everything the build has allocated, the file writer's own
    // buffers included, so it is not the sort buffer itself. What it does show is the
    // difference: the primary buffer held beside the index sorter added nine hundred
    // kilobytes to it.
    let ceiling = 2 * 1024 * 1024;
    assert!(
        held < ceiling,
        "the build held {held} bytes with a {sort_buffer} byte sort buffer, above {ceiling}"
    );
}

/// Sorting the record buffer used to ask the allocator for a copy of the slot array,
/// which is as much again as the records and was not part of what the sort buffer stands
/// for. The measurement is taken from inside the comparison, which is where the sort is.
#[test]
fn sorting_the_record_buffer_allocates_nothing_of_its_own() {
    #[derive(Debug)]
    struct Watching {
        inner: Int64Encoding,
        peak: Arc<std::sync::atomic::AtomicU64>,
    }

    impl drydb::KeyEncoding for Watching {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn compare(&self, a: &[u8], b: &[u8]) -> drydb::Result<std::cmp::Ordering> {
            self.peak
                .fetch_max(live_bytes(), std::sync::atomic::Ordering::Relaxed);
            self.inner.compare(a, b)
        }
        fn digest(&self, key: &[u8]) -> drydb::Result<u64> {
            self.inner.digest(key)
        }
        fn fixed_key_len(&self) -> Option<usize> {
            self.inner.fixed_key_len()
        }
    }

    let dir = std::env::temp_dir().join(format!("drydb-sort-scratch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sorted.drydb");

    let rows = 32_768i64;
    let sort_buffer = 4 * 1024 * 1024;
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let encoding = Arc::new(Watching {
        inner: Int64Encoding,
        peak: Arc::clone(&peak),
    });

    let (outcome, _peak) = measure_peak(move || {
        let mut builder = DatabaseBuilder::new()
            .page_size(4096)
            .unwrap()
            .sort_buffer(sort_buffer)
            .temp_dir(&dir);
        let table = builder.create_table("t", encoding).unwrap();
        for i in 0..rows {
            // Out of order, so the sort has work to do.
            let key = (i * 65_537 + 7_919) % rows;
            builder
                .append(table, &Int64Encoding::encode(key), b"")
                .unwrap();
        }
        let outcome = builder.build_to_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
        outcome
    });

    outcome.unwrap();
    let held = peak.load(std::sync::atomic::Ordering::Relaxed);
    // The records and their slots, and what the build needs beside them. The copy the
    // stable sort asked for added the whole slot array again.
    let ceiling = 3 * 1024 * 1024;
    assert!(
        held < ceiling,
        "sorting {rows} records held {held} bytes with a {sort_buffer} byte sort buffer, \
         above {ceiling}"
    );
}
