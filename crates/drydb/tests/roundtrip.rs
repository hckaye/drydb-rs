//! Build a database, then read it back through the public API.

use std::ops::Bound;
use std::sync::Arc;

use drydb::{AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order};

fn i64_key(v: i64) -> [u8; 8] {
    Int64Encoding::encode(v)
}

fn collect(cursor: &mut drydb::Cursor) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    while cursor.advance().unwrap() {
        let entry = cursor.current().expect("positioned");
        out.push((entry.key().to_vec(), entry.value().to_vec()));
    }
    out
}

struct Built {
    bytes: Vec<u8>,
    rows: Vec<(i64, Vec<u8>)>,
}

fn build_i64_table(count: i64, page_size: usize, eytzinger: bool, value_len: usize) -> Built {
    let mut builder = DatabaseBuilder::new()
        .page_size(page_size)
        .unwrap()
        .eytzinger_digests(eytzinger);
    let table = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    let mut rows = Vec::new();
    // Append out of order on purpose: the builder sorts.
    for i in (0..count).rev() {
        let key = i * 3 - count;
        let value: Vec<u8> = (0..value_len)
            .map(|n| (key as u8).wrapping_add(n as u8))
            .collect();
        builder.append(table, &i64_key(key), &value).unwrap();
        rows.push((key, value));
    }
    rows.sort_by_key(|(k, _)| *k);
    Built {
        bytes: builder.build_to_vec().unwrap(),
        rows,
    }
}

#[test]
fn i64_table_round_trips_across_layouts() {
    for (page_size, eytzinger, value_len) in [
        (256usize, false, 16usize),
        (256, true, 16),
        (4096, false, 32),
        (4096, true, 32),
        // Above 32767 the builder falls back to the classic metadata layout.
        (40_000, false, 64),
        (40_000, true, 64),
    ] {
        let built = build_i64_table(500, page_size, eytzinger, value_len);
        let db = Database::open_bytes(built.bytes.clone()).unwrap();
        let table = db.table("items").unwrap();

        assert_eq!(table.count().unwrap(), built.rows.len() as u64);

        for (key, value) in &built.rows {
            let got = table
                .get(&i64_key(*key))
                .unwrap()
                .unwrap_or_else(|| panic!("missing key {key} at page size {page_size}"));
            assert_eq!(got.as_bytes(), value.as_slice());
        }
        // Keys that were never inserted are absent.
        for probe in [i64::MIN, i64::MAX, 1, -1] {
            if built.rows.iter().all(|(k, _)| *k != probe) {
                assert!(table.get(&i64_key(probe)).unwrap().is_none());
            }
        }

        let mut cursor = table.scan(Order::Ascending).unwrap();
        let scanned = collect(&mut cursor);
        assert_eq!(scanned.len(), built.rows.len());
        for ((got_key, got_value), (key, value)) in scanned.iter().zip(&built.rows) {
            assert_eq!(got_key.as_slice(), &i64_key(*key));
            assert_eq!(got_value, value);
        }

        let mut cursor = table.scan(Order::Descending).unwrap();
        let descending = collect(&mut cursor);
        let mut expected = scanned.clone();
        expected.reverse();
        assert_eq!(
            descending, expected,
            "descending scan at page size {page_size}"
        );

        let verified = db.verify(Default::default()).unwrap();
        assert!(verified.is_ok(), "verify reported {:?}", verified.problems);
    }
}

#[test]
fn range_bounds_match_a_reference_implementation() {
    let built = build_i64_table(200, 512, false, 8);
    let db = Database::open_bytes(built.bytes).unwrap();
    let table = db.table("items").unwrap();
    let keys: Vec<i64> = built.rows.iter().map(|(k, _)| *k).collect();

    let probes: Vec<i64> = vec![
        i64::MIN,
        keys[0] - 1,
        keys[0],
        keys[1],
        keys[keys.len() / 2],
        keys[keys.len() - 1],
        keys[keys.len() - 1] + 1,
        i64::MAX,
    ];

    for &lo in &probes {
        for &hi in &probes {
            if lo > hi {
                continue;
            }
            for lo_ex in [false, true] {
                for hi_ex in [false, true] {
                    let lo_key = i64_key(lo);
                    let hi_key = i64_key(hi);
                    let lower = if lo_ex {
                        Bound::Excluded(&lo_key[..])
                    } else {
                        Bound::Included(&lo_key[..])
                    };
                    let upper = if hi_ex {
                        Bound::Excluded(&hi_key[..])
                    } else {
                        Bound::Included(&hi_key[..])
                    };
                    let expected: Vec<i64> = keys
                        .iter()
                        .copied()
                        .filter(|k| if lo_ex { *k > lo } else { *k >= lo })
                        .filter(|k| if hi_ex { *k < hi } else { *k <= hi })
                        .collect();

                    let mut cursor = table.range(lower, upper, Order::Ascending).unwrap();
                    let got: Vec<i64> = collect(&mut cursor)
                        .into_iter()
                        .map(|(k, _)| Int64Encoding::decode(&k).unwrap())
                        .collect();
                    assert_eq!(got, expected, "ascending [{lo},{hi}] ex=({lo_ex},{hi_ex})");

                    let mut cursor = table.range(lower, upper, Order::Descending).unwrap();
                    let got: Vec<i64> = collect(&mut cursor)
                        .into_iter()
                        .map(|(k, _)| Int64Encoding::decode(&k).unwrap())
                        .collect();
                    let mut reversed = expected.clone();
                    reversed.reverse();
                    assert_eq!(got, reversed, "descending [{lo},{hi}] ex=({lo_ex},{hi_ex})");

                    assert_eq!(
                        table.count_range(lower, upper).unwrap(),
                        expected.len() as u64,
                        "count [{lo},{hi}] ex=({lo_ex},{hi_ex})"
                    );
                }
            }
        }
    }
}

#[test]
fn ascii_keys_support_prefix_queries() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("words", Arc::new(AsciiEncoding))
        .unwrap();
    let words = ["ab", "abc", "abd", "b", "ba", "", "a", "zzzz"];
    for word in words {
        builder
            .append(table, word.as_bytes(), word.as_bytes())
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("words").unwrap();

    let mut cursor = table.prefix(b"ab", Order::Ascending).unwrap();
    let got: Vec<String> = collect(&mut cursor)
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect();
    assert_eq!(got, vec!["ab", "abc", "abd"]);

    // The empty key is a key, not an open end.
    assert!(table.get(b"").unwrap().is_some());
    let mut cursor = table
        .range(
            Bound::Excluded(&b""[..]),
            Bound::Unbounded,
            Order::Ascending,
        )
        .unwrap();
    let got: Vec<String> = collect(&mut cursor)
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect();
    assert_eq!(got.len(), words.len() - 1);
    assert!(!got.contains(&String::new()));

    // Every key, in order.
    let mut cursor = table.scan(Order::Ascending).unwrap();
    let all: Vec<String> = collect(&mut cursor)
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect();
    let mut sorted: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    sorted.sort();
    assert_eq!(all, sorted);
}

#[test]
fn prefix_is_rejected_for_encodings_that_are_not_byte_ordered() {
    let built = build_i64_table(4, 256, false, 4);
    let db = Database::open_bytes(built.bytes).unwrap();
    let table = db.table("items").unwrap();
    let err = table.prefix(b"\x00", Order::Ascending).unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::InvalidArgument);
}

#[test]
fn large_values_move_to_blob_pages_and_stream() {
    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder
        .create_table("blobs", Arc::new(Int64Encoding))
        .unwrap();
    let sizes = [0usize, 1, 100, 400, 65_534, 65_535, 65_536, 200_000];
    for (i, size) in sizes.iter().enumerate() {
        let value: Vec<u8> = (0..*size).map(|n| (n % 251) as u8).collect();
        builder.append(table, &i64_key(i as i64), &value).unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(8 << 20)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("blobs").unwrap();
    for (i, size) in sizes.iter().enumerate() {
        let expected: Vec<u8> = (0..*size).map(|n| (n % 251) as u8).collect();
        let got = table.get(&i64_key(i as i64)).unwrap().unwrap();
        assert_eq!(
            got.as_bytes(),
            expected.as_slice(),
            "value {i} of {size} bytes"
        );
    }

    // The 200 kB value cannot be inline in a 512 byte page, so it streams.
    let mut reader = table.blob_reader(&i64_key(7)).unwrap().unwrap();
    assert_eq!(reader.len(), 200_000);
    let mut streamed = Vec::new();
    std::io::copy(&mut reader, &mut streamed).unwrap();
    let expected: Vec<u8> = (0..200_000).map(|n| (n % 251) as u8).collect();
    assert_eq!(streamed, expected);

    // A small value is inline, and asking to stream it says so.
    assert_eq!(
        table.blob_reader(&i64_key(1)).unwrap_err().kind(),
        drydb::ErrorKind::InvalidArgument
    );
}

#[test]
fn secondary_indexes_resolve_records() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("people", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .add_secondary_index(
            table,
            "by_city",
            false,
            Arc::new(AsciiEncoding),
            Box::new(|_key, value| Ok(value[..3].to_vec())),
        )
        .unwrap();
    builder
        .add_secondary_index(
            table,
            "by_id_text",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|key, _value| {
                Ok(format!("id-{:04}", Int64Encoding::decode(key).unwrap()).into_bytes())
            }),
        )
        .unwrap();

    let cities = ["NYC", "LON", "TYO"];
    for i in 0..60i64 {
        let value = format!("{}#{i}", cities[(i as usize) % cities.len()]);
        builder
            .append(table, &i64_key(i), value.as_bytes())
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("people").unwrap();

    let unique = table.index("by_id_text").unwrap();
    assert!(unique.is_unique());
    let value = unique.get(b"id-0042").unwrap().unwrap();
    assert_eq!(value.as_bytes(), b"NYC#42");

    let non_unique = table.index("by_city").unwrap();
    assert!(!non_unique.is_unique());
    let mut cursor = non_unique.lookup(b"NYC").unwrap();
    let mut found = Vec::new();
    while cursor.advance().unwrap() {
        assert_eq!(cursor.key(), b"NYC");
        found.push(String::from_utf8(cursor.value().unwrap().to_vec()).unwrap());
    }
    let expected: Vec<String> = (0..60i64)
        .filter(|i| i % 3 == 0)
        .map(|i| format!("NYC#{i}"))
        .collect();
    assert_eq!(found, expected);

    assert_eq!(non_unique.count().unwrap(), 60);
    assert_eq!(
        non_unique
            .count_range(Bound::Included(&b"NYC"[..]), Bound::Included(&b"NYC"[..]))
            .unwrap(),
        20
    );
    // An exclusive bound clears the whole run of rows for that key.
    assert_eq!(
        non_unique
            .count_range(Bound::Excluded(&b"LON"[..]), Bound::Unbounded)
            .unwrap(),
        40
    );

    let report = db.verify(Default::default()).unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
}

#[test]
fn a_key_that_cannot_reduce_a_level_is_rejected() {
    // A 128 byte page with Eytzinger digests holds one 29 byte separator but not two.
    // A level that fits one separator per page promotes as many entries as it received,
    // so the tree would grow a level per rotation and never finish. The builder refuses
    // the key instead.
    let mut builder = DatabaseBuilder::new()
        .page_size(128)
        .unwrap()
        .eytzinger_digests(true);
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    let err = builder.append(table, &[b'a'; 29], b"v").unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::ValueTooLarge);
    assert!(err.to_string().contains("two separators"), "{err}");

    // Two bytes shorter is fine, and so is the same key on a larger page.
    builder.append(table, &[b'a'; 27], b"v").unwrap();
    let mut builder = DatabaseBuilder::new()
        .page_size(4096)
        .unwrap()
        .eytzinger_digests(true);
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder.append(table, &[b'a'; 29], b"v").unwrap();
    builder.append(table, &[b'b'; 33], b"v").unwrap();
    let bytes = builder.build_to_vec().unwrap();
    let db = Database::open_bytes(bytes).unwrap();
    assert_eq!(db.table("t").unwrap().count().unwrap(), 2);
}

#[test]
fn a_key_too_large_for_a_leaf_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    let err = builder.append(table, &[b'a'; 200], b"v").unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::ValueTooLarge);
    assert!(err.to_string().contains("leaf page"), "{err}");
}

#[test]
fn duplicate_primary_keys_are_rejected() {
    let mut builder = DatabaseBuilder::new();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder.append(table, &i64_key(1), b"a").unwrap();
    builder.append(table, &i64_key(1), b"b").unwrap();
    let err = builder.build_to_vec().unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::InvalidArgument);
}

#[test]
fn duplicate_unique_index_keys_are_rejected() {
    let mut builder = DatabaseBuilder::new();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "same",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|_k, _v| Ok(b"constant".to_vec())),
        )
        .unwrap();
    builder.append(table, &i64_key(1), b"a").unwrap();
    builder.append(table, &i64_key(2), b"b").unwrap();
    let err = builder.build_to_vec().unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::InvalidArgument);
}

#[test]
fn empty_tables_are_readable() {
    let mut builder = DatabaseBuilder::new();
    let empty = builder
        .create_table("empty", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .add_secondary_index(
            empty,
            "idx",
            false,
            Arc::new(AsciiEncoding),
            Box::new(|_k, v| Ok(v.to_vec())),
        )
        .unwrap();
    let full = builder
        .create_table("full", Arc::new(AsciiEncoding))
        .unwrap();
    builder.append(full, b"k", b"v").unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    let empty = db.table("empty").unwrap();
    assert_eq!(empty.count().unwrap(), 0);
    assert!(empty.get(&i64_key(1)).unwrap().is_none());
    let mut cursor = empty.scan(Order::Ascending).unwrap();
    assert!(!cursor.advance().unwrap());
    let mut cursor = empty.scan(Order::Descending).unwrap();
    assert!(!cursor.advance().unwrap());
    assert_eq!(empty.index("idx").unwrap().count().unwrap(), 0);

    let full = db.table("full").unwrap();
    assert_eq!(full.get(b"k").unwrap().unwrap().as_bytes(), b"v");

    let report = db.verify(Default::default()).unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
}

#[test]
fn multiple_tables_stay_separate() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let a = builder.create_table("a", Arc::new(Int64Encoding)).unwrap();
    let b = builder.create_table("b", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..100i64 {
        builder
            .append(a, &i64_key(i), format!("a{i}").as_bytes())
            .unwrap();
        builder
            .append(b, format!("k{i:03}").as_bytes(), format!("b{i}").as_bytes())
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    assert_eq!(*db.table_names().unwrap(), vec!["a", "b"]);
    assert_eq!(
        db.table("a")
            .unwrap()
            .get(&i64_key(42))
            .unwrap()
            .unwrap()
            .as_bytes(),
        b"a42"
    );
    assert_eq!(
        db.table("b")
            .unwrap()
            .get(b"k042")
            .unwrap()
            .unwrap()
            .as_bytes(),
        b"b42"
    );
    assert!(db.table("c").is_err());
}

#[test]
fn guards_outlive_the_database() {
    let built = build_i64_table(50, 256, false, 16);
    let guard = {
        let db = Database::open_bytes(built.bytes).unwrap();
        let table = db.table("items").unwrap();
        table.get(&i64_key(built.rows[10].0)).unwrap().unwrap()
    };
    assert_eq!(guard.as_bytes(), built.rows[10].1.as_slice());
}

#[test]
fn the_page_cache_stays_inside_its_budget() {
    // A dataset far larger than the cache, read with a tiny budget.
    let built = build_i64_table(20_000, 256, false, 32);
    let budget = 4 << 20;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_capacity(128 * 1024)
        .open_bytes(built.bytes)
        .unwrap();
    let table = db.table("items").unwrap();
    for (key, value) in built.rows.iter().step_by(37) {
        let got = table.get(&i64_key(*key)).unwrap().unwrap();
        assert_eq!(got.as_bytes(), value.as_slice());
    }
    let report = db.memory_report();
    assert!(
        report.charged <= budget,
        "charged {} of {budget}",
        report.charged
    );
    assert!(
        report.cache_resident <= 128 * 1024 * 2,
        "cache holds {} bytes",
        report.cache_resident
    );

    // A full scan does not accumulate pages either.
    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut seen = 0u64;
    while cursor.advance().unwrap() {
        seen += 1;
    }
    assert_eq!(seen, built.rows.len() as u64);
    assert!(db.memory_report().charged <= budget);
}

#[test]
fn key_encoding_validates_caller_keys() {
    let built = build_i64_table(4, 256, false, 4);
    let db = Database::open_bytes(built.bytes).unwrap();
    let table = db.table("items").unwrap();
    assert_eq!(
        table.get(b"short").unwrap_err().kind(),
        drydb::ErrorKind::InvalidArgument
    );
    assert_eq!(table.key_encoding().id(), "i64");
}

#[test]
fn reversed_range_bounds_are_an_error() {
    let built = build_i64_table(10, 256, false, 4);
    let db = Database::open_bytes(built.bytes).unwrap();
    let table = db.table("items").unwrap();
    let err = table
        .range(
            Bound::Included(&i64_key(100)[..]),
            Bound::Included(&i64_key(0)[..]),
            Order::Ascending,
        )
        .unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::InvalidArgument);
}

#[test]
fn building_to_a_file_publishes_atomically() {
    let dir = std::env::temp_dir().join(format!("drydb-publish-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.drydb");

    let mut builder = DatabaseBuilder::new();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..100i64 {
        builder
            .append(table, &i64_key(i), &i.to_le_bytes())
            .unwrap();
    }
    let report = builder.build_to_file(&path).unwrap();
    assert!(report.page_count > 0);
    assert_eq!(report.tables[0].rows, 100);
    assert_eq!(report.file_size, std::fs::metadata(&path).unwrap().len());

    let db = Database::open(&path).unwrap();
    assert_eq!(db.table("t").unwrap().count().unwrap(), 100);

    // No temporary files were left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "db.drydb")
        .collect();
    assert!(leftovers.is_empty(), "left behind {leftovers:?}");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
