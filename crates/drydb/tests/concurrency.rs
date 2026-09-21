//! Concurrent reads, eviction under pressure, and what happens when the budget runs out.
//!
//! The properties under test are the ones a read-only store has to hold to be usable
//! from more than one thread: readers never observe a torn or freed page, a page that
//! several threads miss at once is read once, running out of budget is an error rather
//! than a wait, and a database that hit its budget keeps working once the memory comes
//! back.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use drydb::{
    Database, DatabaseBuilder, ErrorKind, Int64Encoding, MemorySource, OpenOptions, Order,
    PageSource, ValueGuard,
};

fn build(rows: i64, value_len: usize) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..rows {
        let value: Vec<u8> = (0..value_len)
            .map(|n| (i as u8).wrapping_add(n as u8))
            .collect();
        builder
            .append(table, &Int64Encoding::encode(i), &value)
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

fn expected_value(i: i64, value_len: usize) -> Vec<u8> {
    (0..value_len)
        .map(|n| (i as u8).wrapping_add(n as u8))
        .collect()
}

#[test]
fn many_threads_read_the_same_database() {
    let rows = 4000i64;
    let value_len = 24;
    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(4 << 20)
            // Small enough that the working set does not fit, so eviction runs
            // constantly while other threads are reading.
            .cache_capacity(64 * 1024)
            .cache_shards(8)
            .open_bytes(build(rows, value_len))
            .unwrap(),
    );

    let barrier = Arc::new(Barrier::new(8));
    std::thread::scope(|scope| {
        for thread in 0..8u64 {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let table = db.table("t").unwrap();
                barrier.wait();
                let mut state = thread.wrapping_mul(0x9E37_79B9) | 1;
                for _ in 0..4000 {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let key = ((state >> 33) % rows as u64) as i64;
                    let value = table
                        .get(&Int64Encoding::encode(key))
                        .unwrap_or_else(|e| panic!("lookup of {key} failed: {e}"))
                        .unwrap_or_else(|| panic!("key {key} went missing"));
                    assert_eq!(value.as_bytes(), expected_value(key, value_len).as_slice());
                }
            });
        }
    });

    let metrics = db.metrics();
    assert!(metrics.cache_hits > 0);
    assert!(metrics.evictions > 0, "the cache should have had to evict");
    assert!(db.memory_report().charged <= 4 << 20);
}

/// A page source that makes every read slow, so that a race the scheduler would
/// otherwise decide is guaranteed to happen.
struct SlowSource {
    inner: MemorySource,
    delay: Duration,
    reads: AtomicUsize,
}

impl PageSource for SlowSource {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(self.delay);
        self.inner.read_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }
}

#[test]
fn concurrent_misses_of_the_same_page_are_coalesced() {
    // Every read takes 20ms, and all sixteen threads ask for the same key at the same
    // moment. Without de-duplication each of them would read the same pages.
    let source = Arc::new(SlowSource {
        inner: MemorySource::new(build(2000, 16)),
        delay: Duration::from_millis(20),
        reads: AtomicUsize::new(0),
    });
    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(4 << 20)
            .cache_shards(1)
            .open_source(Arc::clone(&source) as Arc<dyn PageSource>)
            .unwrap(),
    );

    let reads_after_open = source.reads.load(Ordering::Relaxed);
    let barrier = Arc::new(Barrier::new(16));
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let table = db.table("t").unwrap();
                barrier.wait();
                let value = table.get(&Int64Encoding::encode(1234)).unwrap().unwrap();
                assert_eq!(value.as_bytes(), expected_value(1234, 16).as_slice());
            });
        }
    });

    let metrics = db.metrics();
    assert!(
        metrics.coalesced_loads > 0,
        "sixteen threads asking for one key at once should have waited on each other"
    );
    // The path is a handful of pages, and each was read once however many threads
    // wanted it. The `+ 1` allows for the page length probe that precedes each read.
    let reads = source.reads.load(Ordering::Relaxed) - reads_after_open;
    assert!(
        reads <= (metrics.page_reads * 2 + 2) as usize,
        "{reads} source reads for {} pages",
        metrics.page_reads
    );
    assert!(
        metrics.page_reads <= 8,
        "one key should not have needed {} page reads",
        metrics.page_reads
    );
}

#[test]
fn scans_and_lookups_interleave_safely() {
    let rows = 3000i64;
    let value_len = 32;
    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(2 << 20)
            .cache_capacity(48 * 1024)
            .open_bytes(build(rows, value_len))
            .unwrap(),
    );
    let seen = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for thread in 0..6 {
            let db = Arc::clone(&db);
            let seen = Arc::clone(&seen);
            scope.spawn(move || {
                let table = db.table("t").unwrap();
                if thread % 2 == 0 {
                    for _ in 0..3 {
                        let mut cursor = table.scan(Order::Ascending).unwrap();
                        let mut count = 0i64;
                        while cursor.advance().unwrap() {
                            let entry = cursor.current().unwrap();
                            assert_eq!(
                                entry.key(),
                                &Int64Encoding::encode(count),
                                "scan produced keys out of order"
                            );
                            assert_eq!(entry.value(), expected_value(count, value_len).as_slice());
                            count += 1;
                        }
                        assert_eq!(count, rows);
                        seen.fetch_add(count as usize, Ordering::Relaxed);
                    }
                } else {
                    for key in (0..rows).step_by(7) {
                        let value = table.get(&Int64Encoding::encode(key)).unwrap().unwrap();
                        assert_eq!(value.as_bytes(), expected_value(key, value_len).as_slice());
                    }
                }
            });
        }
    });
    assert_eq!(seen.load(Ordering::Relaxed), (rows as usize) * 9);
}

#[test]
fn guards_stay_valid_while_the_cache_churns() {
    let rows = 2000i64;
    let value_len = 40;
    let db = OpenOptions::new()
        .memory_budget(8 << 20)
        .cache_capacity(16 * 1024)
        .open_bytes(build(rows, value_len))
        .unwrap();
    let table = db.table("t").unwrap();

    // Hold guards from all over the file, then force the cache to turn over completely.
    let held: Vec<(i64, ValueGuard)> = (0..rows)
        .step_by(131)
        .map(|key| {
            (
                key,
                table.get(&Int64Encoding::encode(key)).unwrap().unwrap(),
            )
        })
        .collect();
    for key in 0..rows {
        let _ = table.get(&Int64Encoding::encode(key)).unwrap();
    }
    for (key, guard) in &held {
        assert_eq!(
            guard.as_bytes(),
            expected_value(*key, value_len).as_slice(),
            "guard for key {key} was invalidated by eviction"
        );
    }

    // The pages those guards hold are still charged, even though the cache dropped them.
    let report = db.memory_report();
    assert!(report.charged >= report.cache_resident.min(report.charged));
    drop(held);
}

#[test]
fn running_out_of_budget_is_an_error_and_recovers() {
    let rows = 4000i64;
    let value_len = 40;
    let bytes = build(rows, value_len);
    // Room for a handful of pages: enough to search, not enough to hold many guards.
    let db = OpenOptions::new()
        .memory_budget(70 * 1024)
        .cache_capacity(8 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    let mut guards = Vec::new();
    let mut exhausted = None;
    for key in (0..rows).step_by(5) {
        match table.get(&Int64Encoding::encode(key)) {
            Ok(Some(guard)) => guards.push(guard),
            Ok(None) => panic!("key {key} went missing"),
            Err(e) => {
                exhausted = Some(e);
                break;
            }
        }
    }

    let error = exhausted.expect("holding guards under a tight budget should exhaust it");
    assert_eq!(error.kind(), ErrorKind::BudgetExceeded);
    assert!(
        error.is_retryable(),
        "releasing guards would make room, so this is retryable"
    );
    let info = error
        .budget_info()
        .expect("budget errors carry their accounting");
    assert!(info.in_use <= info.limit);
    assert!(info.requested > 0);

    // The guards taken before the budget ran out are still valid.
    for (n, guard) in guards.iter().enumerate() {
        let key = (n * 5) as i64;
        assert_eq!(guard.as_bytes(), expected_value(key, value_len).as_slice());
    }

    // And once they go away, queries work again.
    drop(guards);
    for key in (0..rows).step_by(211) {
        let value = table
            .get(&Int64Encoding::encode(key))
            .unwrap_or_else(|e| panic!("query after recovery failed: {e}"))
            .unwrap();
        assert_eq!(value.as_bytes(), expected_value(key, value_len).as_slice());
    }
}

#[test]
fn a_request_larger_than_the_whole_budget_is_not_retryable() {
    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), &vec![0u8; 400_000])
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(2), b"small")
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(64 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    // The small value still reads.
    assert_eq!(
        table
            .get(&Int64Encoding::encode(2))
            .unwrap()
            .unwrap()
            .as_bytes(),
        b"small"
    );

    // The 400 kB blob page cannot fit in a 64 kB budget, and no amount of eviction will
    // change that.
    let error = table.get(&Int64Encoding::encode(1)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BudgetExceeded);
    assert!(!error.is_retryable());

    // Streaming it does fit, because it never materialises the page.
    let reader = table
        .blob_reader(&Int64Encoding::encode(1))
        .unwrap()
        .unwrap();
    assert_eq!(reader.len(), 400_000);
    let mut buf = vec![0u8; 8192];
    let mut total = 0u64;
    while total < reader.len() {
        let n = reader.read_at(&mut buf, total).unwrap();
        assert!(n > 0);
        total += n as u64;
    }
    assert_eq!(total, 400_000);
}

#[test]
fn a_database_can_be_dropped_while_guards_are_alive() {
    let guard = {
        let db = Database::open_bytes(build(500, 16)).unwrap();
        let table = db.table("t").unwrap();
        table.get(&Int64Encoding::encode(42)).unwrap().unwrap()
    };
    assert_eq!(guard.as_bytes(), expected_value(42, 16).as_slice());

    // Guards are `Send`: one can be handed to another thread after the database is gone.
    let handle = std::thread::spawn(move || guard.as_bytes().to_vec());
    assert_eq!(handle.join().unwrap(), expected_value(42, 16));
}
