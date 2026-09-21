//! Read-path benchmarks.
//!
//! The working set is varied deliberately: a database that fits the cache measures the
//! search, and one many times larger than the cache measures the search plus the page
//! path. Both are reported, because only quoting the first would describe a case most
//! callers are not in.

mod common;

use std::ops::Bound;
use std::sync::Arc;

use common::{environment, header, measure, measure_each, scale, CountingAllocator, Rng};
use drydb::{AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn build_i64(rows: i64, value_len: usize, page_size: usize, eytzinger: bool) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new()
        .page_size(page_size)
        .unwrap()
        .eytzinger_digests(eytzinger);
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

fn build_ascii(rows: i64, page_size: usize) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(page_size).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_head",
            false,
            Arc::new(AsciiEncoding),
            Box::new(|_k, v| Ok(v[..v.len().min(9)].to_vec())),
        )
        .unwrap();
    for i in 0..rows {
        let key = format!("common-prefix-{i:010}");
        let value = format!("group{:04}-payload-{i}", i % 64);
        builder
            .append(table, key.as_bytes(), value.as_bytes())
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

fn open(bytes: Vec<u8>, cache: u64) -> Database {
    OpenOptions::new()
        .memory_budget(cache.max(8 << 20) * 2)
        .cache_capacity(cache)
        .open_bytes(bytes)
        .unwrap()
}

fn point_lookups(db: &Database, rows: i64, probes: u64, label: &str) {
    let table = db.table("t").unwrap();
    let mut rng = Rng::new(0xBEEF_0001);
    let keys: Vec<[u8; 8]> = (0..probes)
        .map(|_| Int64Encoding::encode(rng.below(rows as u64) as i64))
        .collect();

    let mut checksum = 0u64;
    let mut report = measure_each(label, keys.len(), |i| {
        let value = table.get(&keys[i]).unwrap().expect("key present");
        checksum = checksum.wrapping_add(value.as_bytes()[0] as u64);
        1
    });
    let metrics = db.metrics();
    report.notes.push(format!(
        "hit rate {:.1}%",
        100.0 * metrics.cache_hits as f64
            / (metrics.cache_hits + metrics.cache_misses).max(1) as f64
    ));
    report.notes.push(format!("checksum {checksum:#x}"));
    report.print();
}

fn main() {
    environment();

    let rows = scale("DRYDB_BENCH_ROWS", 200_000) as i64;
    let probes = scale("DRYDB_BENCH_PROBES", 100_000);

    header(&format!("i64 point lookups, {rows} rows, 32 byte values"));
    for (page_size, eytzinger) in [(4096usize, false), (4096, true), (256, false)] {
        let bytes = build_i64(rows, 32, page_size, eytzinger);
        let size = bytes.len() as u64;
        for (cache_label, cache) in [
            ("cache holds the file", size * 2),
            ("cache holds a sixteenth", (size / 16).max(64 * 1024)),
        ] {
            let db = open(bytes.clone(), cache);
            // Warm the root and upper levels the way a running process would be.
            let table = db.table("t").unwrap();
            for i in (0..rows).step_by((rows / 1000).max(1) as usize) {
                let _ = table.get(&Int64Encoding::encode(i)).unwrap();
            }
            point_lookups(
                &db,
                rows,
                probes,
                &format!(
                    "page {page_size:>5}{}, {cache_label}",
                    if eytzinger {
                        ", eytzinger"
                    } else {
                        "           "
                    }
                ),
            );
        }
    }

    header("i64 scans and counts");
    let bytes = build_i64(rows, 32, 4096, false);
    let size = bytes.len() as u64;
    for (label, cache) in [
        ("warm", size * 2),
        ("cache holds a sixteenth", (size / 16).max(64 * 1024)),
    ] {
        let db = open(bytes.clone(), cache);
        let table = db.table("t").unwrap();
        {
            let mut cursor = table.scan(Order::Ascending).unwrap();
            while cursor.advance().unwrap() {}
        }

        let mut checksum = 0u64;
        measure(&format!("full ascending scan, {label}"), || {
            let mut cursor = table.scan(Order::Ascending).unwrap();
            let mut seen = 0;
            while cursor.advance().unwrap() {
                let entry = cursor.current().expect("positioned");
                checksum = checksum.wrapping_add(entry.value()[0] as u64);
                seen += 1;
            }
            seen
        })
        .print();

        measure(&format!("full descending scan, {label}"), || {
            let mut cursor = table.scan(Order::Descending).unwrap();
            let mut seen = 0;
            while cursor.advance().unwrap() {
                let entry = cursor.current().expect("positioned");
                checksum = checksum.wrapping_add(entry.value()[0] as u64);
                seen += 1;
            }
            seen
        })
        .print();

        let mut rng = Rng::new(0xBEEF_0002);
        measure_each(&format!("short range (100 rows), {label}"), 2000, |_| {
            let start = rng.below(rows as u64 - 100) as i64;
            let lower = Int64Encoding::encode(start);
            let upper = Int64Encoding::encode(start + 100);
            let mut cursor = table
                .range(
                    Bound::Included(&lower[..]),
                    Bound::Excluded(&upper[..]),
                    Order::Ascending,
                )
                .unwrap();
            let mut seen = 0;
            while cursor.advance().unwrap() {
                checksum = checksum.wrapping_add(cursor.current().unwrap().value()[0] as u64);
                seen += 1;
            }
            seen
        })
        .print();

        measure(&format!("count of every row, {label}"), || {
            assert_eq!(table.count().unwrap(), rows as u64);
            rows as u64
        })
        .print();
        std::hint::black_box(checksum);
    }

    header("ascii keys with a common prefix, and a non-unique index");
    let ascii_rows = (rows / 4).max(1000);
    let bytes = build_ascii(ascii_rows, 4096);
    let size = bytes.len() as u64;
    let db = open(bytes, size * 2);
    let table = db.table("t").unwrap();
    let mut rng = Rng::new(0xBEEF_0003);
    let keys: Vec<String> = (0..probes.min(50_000))
        .map(|_| format!("common-prefix-{:010}", rng.below(ascii_rows as u64)))
        .collect();
    let mut checksum = 0u64;
    measure_each(
        "point lookups, eight byte digest collisions",
        keys.len(),
        |i| {
            let value = table.get(keys[i].as_bytes()).unwrap().expect("key present");
            checksum = checksum.wrapping_add(value.as_bytes()[0] as u64);
            1
        },
    )
    .print();

    let index = table.index("by_head").unwrap();
    measure_each("index lookups, ~64 rows per key", 5000, |i| {
        let key = format!("group{:04}", i % 64);
        let mut cursor = index.lookup(key.as_bytes()).unwrap();
        let mut seen = 0;
        while cursor.advance().unwrap() {
            checksum = checksum.wrapping_add(cursor.value().unwrap().as_bytes()[0] as u64);
            seen += 1;
        }
        seen
    })
    .print();
    std::hint::black_box(checksum);

    header("memory");
    let report = db.memory_report();
    println!(
        "  budget {} bytes, charged {}, peak {}, cache resident {}",
        report.budget_limit, report.charged, report.peak_charged, report.cache_resident
    );
    let metrics = db.metrics();
    println!(
        "  page reads {}, bytes read {}, evictions {}, coalesced loads {}",
        metrics.page_reads, metrics.bytes_read, metrics.evictions, metrics.coalesced_loads
    );
}
