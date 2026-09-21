//! The async adapter over a real database.

use std::ops::Bound;
use std::sync::Arc;

use drydb::{Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order};
use drydb_async::AsyncDatabase;

fn build(rows: i64) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    for i in 0..rows {
        builder
            .append(
                table,
                &Int64Encoding::encode(i),
                format!("value-{i}").as_bytes(),
            )
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

fn open(rows: i64) -> AsyncDatabase {
    let db = OpenOptions::new()
        .memory_budget(2 << 20)
        .cache_capacity(8 * 1024)
        .open_bytes(build(rows))
        .unwrap();
    AsyncDatabase::from_database(db)
}

#[tokio::test]
async fn point_lookups_work_hot_and_cold() {
    let db = open(2000);
    let table = db.table("items").unwrap();

    for i in 0..2000i64 {
        let value = table.get(&Int64Encoding::encode(i)).await.unwrap().unwrap();
        assert_eq!(value.as_bytes(), format!("value-{i}").as_bytes());
    }
    // A second pass hits the cache for at least some keys, which is the path that
    // completes without spawning.
    for i in (0..2000i64).rev().take(50) {
        let value = table.get(&Int64Encoding::encode(i)).await.unwrap().unwrap();
        assert_eq!(value.as_bytes(), format!("value-{i}").as_bytes());
    }
    assert!(table
        .get(&Int64Encoding::encode(99_999))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn counts_and_bounded_scans() {
    let db = open(500);
    let table = db.table("items").unwrap();

    assert_eq!(table.count().await.unwrap(), 500);
    assert_eq!(
        table
            .count_range(
                Bound::Included(Int64Encoding::encode(100).to_vec()),
                Bound::Excluded(Int64Encoding::encode(200).to_vec()),
            )
            .await
            .unwrap(),
        100
    );

    let rows = table
        .collect_range(Bound::Unbounded, Bound::Unbounded, Order::Ascending, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[0].value.as_bytes(), b"value-0");

    let rows = table
        .collect_range(Bound::Unbounded, Bound::Unbounded, Order::Descending, 3)
        .await
        .unwrap();
    assert_eq!(rows[0].value.as_bytes(), b"value-499");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_tasks_share_one_database() {
    let db = open(3000);
    let mut handles = Vec::new();
    for thread in 0..8u64 {
        let table = db.table("items").unwrap();
        handles.push(tokio::spawn(async move {
            let mut state = thread.wrapping_mul(0x9E37_79B9) | 1;
            for _ in 0..500 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let key = ((state >> 33) % 3000) as i64;
                let value = table
                    .get(&Int64Encoding::encode(key))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(value.as_bytes(), format!("value-{key}").as_bytes());
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn opening_and_verifying_run_off_the_async_thread() {
    let dir = std::env::temp_dir().join(format!("drydb-async-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.drydb");
    std::fs::write(&path, build(200)).unwrap();

    let db = AsyncDatabase::open(&path).await.unwrap();
    assert_eq!(*db.table_names().unwrap(), vec!["items".to_string()]);
    let report = db.verify(Default::default()).await.unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);

    assert!(AsyncDatabase::open(dir.join("missing.drydb"))
        .await
        .is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn guards_outlive_the_handles_that_produced_them() {
    let guard = {
        let db = open(100);
        let table = db.table("items").unwrap();
        table
            .get(&Int64Encoding::encode(42))
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(guard.as_bytes(), b"value-42");
}

#[tokio::test]
async fn dropping_a_query_future_leaves_the_database_usable() {
    let db = open(2000);
    let table = db.table("items").unwrap();

    // Start a cold lookup and drop it immediately. The blocking task still finishes, so
    // whatever it reserved is released.
    for i in 0..200i64 {
        let future = table.get_owned(Int64Encoding::encode(i * 7).to_vec());
        drop(future);
    }
    // Give the pool a moment to drain, then check the database still answers.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    for i in 0..100i64 {
        let value = table.get(&Int64Encoding::encode(i)).await.unwrap().unwrap();
        assert_eq!(value.as_bytes(), format!("value-{i}").as_bytes());
    }
    let report = db.sync().memory_report();
    assert!(report.charged <= report.budget_limit);
}

#[tokio::test]
async fn the_synchronous_api_is_still_reachable() {
    let db = open(50);
    let table = db.table("items").unwrap();
    let mut cursor = table.sync().scan(Order::Ascending).unwrap();
    let mut seen = 0;
    while cursor.advance().unwrap() {
        seen += 1;
    }
    assert_eq!(seen, 50);
    assert!(db.sync().metrics().page_reads > 0);
    let _: &Database = db.sync();
}

/// A key that is not cached is copied so it can move to a blocking thread, and a key is
/// as long as the caller makes it. That copy is reserved against the database's budget
/// before it is made, so a key the budget cannot hold is refused rather than allocated.
#[tokio::test]
async fn an_oversized_key_is_refused_rather_than_copied() {
    // `ascii` keys vary in length, so the width check passes and the reservation is what
    // decides.
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("items", Arc::new(drydb::AsciiEncoding))
        .unwrap();
    for i in 0..200u32 {
        builder
            .append(table, format!("{i:04}").as_bytes(), b"v")
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(64 * 1024)
        .cache_capacity(8 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let db = AsyncDatabase::from_database(db);
    let table = db.table("items").unwrap();

    assert!(table.get(b"0007").await.unwrap().is_some());

    let huge = vec![b'9'; 1 << 20];
    let err = table.get(&huge).await.unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::BudgetExceeded, "{err}");

    // And the database still works afterwards.
    assert!(table.get(b"0008").await.unwrap().is_some());
}

/// The copy a lookup makes travels to a blocking thread, and dropping the future does
/// not stop that thread. A reservation left with the future would be released while the
/// copy it stands for is still in use, so a caller that starts and cancels lookups could
/// spend any amount of memory.
#[test]
fn a_cancelled_lookup_keeps_its_key_charged_until_the_work_ends() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};

    /// A source that parks every reader until the test lets them go.
    struct Parked {
        inner: drydb::MemorySource,
        park: AtomicBool,
        gate: Mutex<bool>,
        open: Condvar,
    }

    impl drydb::PageSource for Parked {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
            if self.park.load(Ordering::Relaxed) {
                let mut open = self.gate.lock().unwrap();
                while !*open {
                    open = self.open.wait(open).unwrap();
                }
            }
            self.inner.read_at(buf, offset)
        }

        fn size(&self) -> std::io::Result<u64> {
            self.inner.size()
        }
    }

    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("items", Arc::new(drydb::AsciiEncoding))
        .unwrap();
    for i in 0..200u32 {
        builder
            .append(table, format!("{i:04}").as_bytes(), b"v")
            .unwrap();
    }
    let source = Arc::new(Parked {
        inner: drydb::MemorySource::new(builder.build_to_vec().unwrap()),
        park: AtomicBool::new(false),
        gate: Mutex::new(false),
        open: Condvar::new(),
    });

    let raw = OpenOptions::new()
        .memory_budget(131_072)
        .cache_capacity(0)
        .open_source(Arc::clone(&source) as Arc<dyn drydb::PageSource>)
        .unwrap();
    let db = AsyncDatabase::from_database(raw);

    // One blocking thread, so a lookup that parks it leaves the rest queued with their
    // copies still alive.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();

    let refused = runtime.block_on(async {
        let table = db.table("items").unwrap();
        source.park.store(true, Ordering::Relaxed);
        let key = vec![b'9'; 100_000];
        let mut refused = 0usize;
        for _ in 0..20 {
            let future = table.get(&key);
            tokio::pin!(future);
            tokio::select! {
                result = &mut future => {
                    if result.is_err() {
                        refused += 1;
                    }
                }
                _ = tokio::task::yield_now() => {}
            }
        }
        refused
    });

    // Let the parked readers go and shut the runtime down so the queue drains.
    {
        let mut open = source.gate.lock().unwrap();
        *open = true;
        source.open.notify_all();
    }
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));

    assert!(
        refused > 0,
        "twenty 100 kB copies do not fit in a 128 KiB budget; a cancelled lookup must \
         keep its reservation until the work holding the copy has finished"
    );
}

/// A collected scan hands the caller rows, and rows cost memory: the list, the copies of
/// the keys, and the guards over the pages. None of it was reserved, so a large enough
/// `limit` spent whatever it liked on a database opened with a small budget.
#[tokio::test]
async fn a_collected_scan_stays_inside_the_budget() {
    let budget = 65_536;
    let rows = 3_000i64;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .open_bytes(build(rows))
        .unwrap();
    let db = AsyncDatabase::from_database(db);
    let table = db.table("items").unwrap();

    // Room for the list of every row is not there, and asking for it says so rather than
    // allocating it.
    let err = table
        .collect_range(
            Bound::Unbounded,
            Bound::Unbounded,
            Order::Ascending,
            rows as usize,
        )
        .await
        .expect_err("the whole table does not fit in the budget");
    assert_eq!(err.kind(), drydb::ErrorKind::BudgetExceeded);

    // A scan that does fit works, and what the rows cost comes back when they are
    // dropped. The pages the scan read stay cached and charged, which is why this
    // compares against what is charged while the rows are held rather than before.
    let collected = table
        .collect_range(Bound::Unbounded, Bound::Unbounded, Order::Ascending, 16)
        .await
        .unwrap();
    assert_eq!(collected.len(), 16);
    let held = db.sync().charged_bytes();
    drop(collected);
    let released = db.sync().charged_bytes();
    assert!(
        released < held,
        "holding the rows charged {held} bytes and dropping them left {released}"
    );
}

/// What pays for the list cannot live in the rows. A list that is emptied drops its rows
/// and keeps its allocation, so reservations held by the rows were given back while the
/// memory was still there. The collection carries its own.
#[tokio::test]
async fn emptying_a_collected_scan_does_not_give_back_what_the_list_holds() {
    let rows = 1_000i64;
    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .open_bytes(build(rows))
        .unwrap();
    let db = AsyncDatabase::from_database(db);
    let table = db.table("items").unwrap();

    let collected = table
        .collect_range(
            Bound::Unbounded,
            Bound::Unbounded,
            Order::Ascending,
            rows as usize,
        )
        .await
        .unwrap();
    assert_eq!(collected.len(), rows as usize);

    let (mut list, charge) = collected.into_inner();
    let held = db.sync().charged_bytes();
    list.clear();
    assert_eq!(
        db.sync().charged_bytes(),
        held,
        "the list still holds room for {} rows",
        list.capacity()
    );

    drop(list);
    drop(charge);
    assert!(
        db.sync().charged_bytes() < held,
        "dropping the list and its reservation gives the memory back"
    );
}

/// The async adapter copies the table names, where the synchronous one borrows them.
/// The copies were made without reserving them, so a database opened with a budget that
/// had nothing left still handed them over.
#[tokio::test]
async fn copying_the_table_names_is_charged() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    for i in 0..2 {
        // Long names, so the copies cost more than the budget has left.
        let name = format!("{}{i}", "n".repeat(20_000));
        let table = builder.create_table(name, Arc::new(Int64Encoding)).unwrap();
        builder
            .append(table, &Int64Encoding::encode(0), b"v")
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let budget = 512 * 1024;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .open_bytes(bytes)
        .unwrap();
    let db = AsyncDatabase::from_database(db);
    assert_eq!(db.table_names().unwrap().len(), 2);

    // With the budget spoken for, the copies are refused rather than made.
    let _held = db
        .sync()
        .reserve(budget - db.sync().charged_bytes() - 1024)
        .unwrap();
    assert_eq!(
        db.table_names().unwrap_err().kind(),
        drydb::ErrorKind::BudgetExceeded
    );
}
