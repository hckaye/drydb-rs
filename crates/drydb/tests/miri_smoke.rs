//! A small end-to-end pass, sized to run under Miri.
//!
//! The suites next door cover behaviour; this one exists to put the whole stack --
//! builder, page store, cursor, guards -- under an interpreter that checks for undefined
//! behaviour, aliasing violations and memory leaks, without taking hours to do it.
//!
//! ```text
//! MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p drydb --test miri_smoke
//! ```
//!
//! Isolation has to be off because the builder writes its output through a temporary
//! file.

use std::ops::Bound;
use std::sync::Arc;

use drydb::{AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order};

fn tiny_database(eytzinger: bool) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new()
        .page_size(128)
        .unwrap()
        .eytzinger_digests(eytzinger)
        // Keep the sorter in memory: spilling would add file I/O for no extra coverage.
        .sort_buffer(1 << 20);
    let items = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .add_secondary_index(
            items,
            "by_head",
            false,
            Arc::new(AsciiEncoding),
            Box::new(|_k, v| Ok(v[..v.len().min(9)].to_vec())),
        )
        .unwrap();
    for i in 0..12i64 {
        builder
            .append(
                items,
                &Int64Encoding::encode(i),
                format!("head-{:04}-{i}", i % 3).as_bytes(),
            )
            .unwrap();
    }
    // One value that has to live on its own page.
    builder
        .append(items, &Int64Encoding::encode(99), &vec![7u8; 300])
        .unwrap();
    builder.build_to_vec().unwrap()
}

#[test]
fn end_to_end() {
    for eytzinger in [false, true] {
        let bytes = tiny_database(eytzinger);
        let db = OpenOptions::new()
            .memory_budget(256 * 1024)
            // Small enough that reads evict each other while guards are alive.
            .cache_capacity(2 * 1024)
            .cache_shards(2)
            .open_bytes(bytes)
            .unwrap();
        let table = db.table("items").unwrap();

        assert_eq!(table.count().unwrap(), 13);
        let held = table.get(&Int64Encoding::encode(0)).unwrap().unwrap();

        for i in 0..12i64 {
            let value = table.get(&Int64Encoding::encode(i)).unwrap().unwrap();
            assert_eq!(
                value.as_bytes(),
                format!("head-{:04}-{i}", i % 3).as_bytes()
            );
        }
        assert_eq!(
            table
                .get(&Int64Encoding::encode(99))
                .unwrap()
                .unwrap()
                .len(),
            300
        );
        assert!(table.get(&Int64Encoding::encode(1000)).unwrap().is_none());

        // The first guard survived every eviction in between.
        assert_eq!(held.as_bytes(), b"head-0000-0");

        for order in [Order::Ascending, Order::Descending] {
            let mut cursor = table
                .range(
                    Bound::Included(&Int64Encoding::encode(2)[..]),
                    Bound::Excluded(&Int64Encoding::encode(9)[..]),
                    order,
                )
                .unwrap();
            let mut seen = 0;
            while cursor.advance().unwrap() {
                let entry = cursor.current().unwrap();
                let _ = entry.to_guard().unwrap();
                seen += 1;
            }
            assert_eq!(seen, 7);
        }

        let index = table.index("by_head").unwrap();
        let mut cursor = index.lookup(b"head-0000").unwrap();
        let mut seen = 0;
        while cursor.advance().unwrap() {
            assert_eq!(cursor.key(), b"head-0000");
            seen += 1;
        }
        assert_eq!(seen, 4);

        let report = db.verify(Default::default()).unwrap();
        assert!(report.is_ok(), "{:?}", report.problems);
    }
}

#[test]
fn two_threads_share_one_database() {
    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(256 * 1024)
            .cache_capacity(2 * 1024)
            .open_bytes(tiny_database(false))
            .unwrap(),
    );
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let db = Arc::clone(&db);
            scope.spawn(move || {
                let table = db.table("items").unwrap();
                for _ in 0..3 {
                    for i in 0..12i64 {
                        let value = table.get(&Int64Encoding::encode(i)).unwrap().unwrap();
                        assert_eq!(value.len(), format!("head-{:04}-{i}", i % 3).len());
                    }
                }
            });
        }
    });
}

#[test]
fn a_corrupt_page_is_rejected_without_undefined_behaviour() {
    let mut bytes = tiny_database(false);
    // Flip a byte in the middle of the page area.
    let target = 64 + bytes.len() / 4;
    bytes[target] ^= 0xFF;
    if let Ok(db) = Database::open_bytes(bytes) {
        if let Ok(table) = db.table("items") {
            let _ = table.count();
            let _ = table.get(&Int64Encoding::encode(3));
            if let Ok(mut cursor) = table.scan(Order::Ascending) {
                let mut steps = 0;
                while steps < 100 && matches!(cursor.advance(), Ok(true)) {
                    steps += 1;
                }
            }
        }
    }
}
