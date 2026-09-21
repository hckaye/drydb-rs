//! Property tests against an in-memory model.
//!
//! The model is a `BTreeMap`, which is the answer every query has to match: the point of
//! these tests is that the layout the builder happens to pick -- page size, digest order,
//! whether keys are stored at all, whether a value overflowed -- never changes what a
//! query returns.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use drydb::{AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, Order};
use proptest::prelude::*;

fn build_ascii(rows: &BTreeMap<Vec<u8>, Vec<u8>>, page_size: usize, eytzinger: bool) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new()
        .page_size(page_size)
        .unwrap()
        .eytzinger_digests(eytzinger);
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for (key, value) in rows {
        builder.append(table, key, value).unwrap();
    }
    builder.build_to_vec().unwrap()
}

fn build_i64(rows: &BTreeMap<i64, Vec<u8>>, page_size: usize, eytzinger: bool) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new()
        .page_size(page_size)
        .unwrap()
        .eytzinger_digests(eytzinger);
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for (key, value) in rows {
        builder
            .append(table, &Int64Encoding::encode(*key), value)
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

fn collect(cursor: &mut drydb::Cursor) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    while cursor.advance().unwrap() {
        let entry = cursor.current().expect("positioned");
        out.push((entry.key().to_vec(), entry.value().to_vec()));
    }
    out
}

fn bound_of(key: Option<&Vec<u8>>, exclusive: bool) -> Bound<&[u8]> {
    match key {
        None => Bound::Unbounded,
        Some(k) if exclusive => Bound::Excluded(k.as_slice()),
        Some(k) => Bound::Included(k.as_slice()),
    }
}

fn model_range<'a>(
    rows: &'a BTreeMap<Vec<u8>, Vec<u8>>,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> Vec<(&'a Vec<u8>, &'a Vec<u8>)> {
    rows.iter()
        .filter(|(k, _)| match lower {
            Bound::Unbounded => true,
            Bound::Included(b) => k.as_slice() >= b,
            Bound::Excluded(b) => k.as_slice() > b,
        })
        .filter(|(k, _)| match upper {
            Bound::Unbounded => true,
            Bound::Included(b) => k.as_slice() <= b,
            Bound::Excluded(b) => k.as_slice() < b,
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, max_shrink_iters: 2000, ..ProptestConfig::default() })]

    /// Whatever the layout, a lookup returns exactly what was appended.
    #[test]
    fn ascii_lookups_match_the_model(
        entries in prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 0..24), prop::collection::vec(any::<u8>(), 0..300)),
            1..120,
        ),
        page_size in prop::sample::select(vec![128usize, 256, 1024, 4096, 40_000]),
        eytzinger in any::<bool>(),
    ) {
        let rows: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let bytes = build_ascii(&rows, page_size, eytzinger);
        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();

        prop_assert_eq!(table.count().unwrap(), rows.len() as u64);
        for (key, value) in &rows {
            let got = table.get(key).unwrap();
            prop_assert!(got.is_some(), "missing key {:?}", key);
            let got = got.unwrap();
            prop_assert_eq!(got.as_bytes(), value.as_slice());
        }

        let mut cursor = table.scan(Order::Ascending).unwrap();
        let scanned = collect(&mut cursor);
        let expected: Vec<(Vec<u8>, Vec<u8>)> =
            rows.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        prop_assert_eq!(scanned, expected.clone());

        let mut cursor = table.scan(Order::Descending).unwrap();
        let descending = collect(&mut cursor);
        let mut reversed = expected;
        reversed.reverse();
        prop_assert_eq!(descending, reversed);

        let report = db.verify(Default::default()).unwrap();
        prop_assert!(report.is_ok(), "{:?}", report.problems);
    }

    /// Range bounds agree with the model, in both directions, plus counts.
    #[test]
    fn ascii_ranges_match_the_model(
        entries in prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 0..12), prop::collection::vec(any::<u8>(), 0..40)),
            1..80,
        ),
        probes in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..12), 1..8),
        lower_unbounded in any::<bool>(),
        upper_unbounded in any::<bool>(),
        lower_exclusive in any::<bool>(),
        upper_exclusive in any::<bool>(),
        page_size in prop::sample::select(vec![128usize, 512, 4096]),
    ) {
        let rows: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let bytes = build_ascii(&rows, page_size, false);
        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();

        let mut sorted_probes = probes;
        sorted_probes.sort();
        for pair in sorted_probes.windows(2) {
            let lower_key = if lower_unbounded { None } else { Some(&pair[0]) };
            let upper_key = if upper_unbounded { None } else { Some(&pair[1]) };
            let lower = bound_of(lower_key, lower_exclusive);
            let upper = bound_of(upper_key, upper_exclusive);

            let expected: Vec<(Vec<u8>, Vec<u8>)> = model_range(&rows, lower, upper)
                .into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let mut cursor = table.range(lower, upper, Order::Ascending).unwrap();
            prop_assert_eq!(collect(&mut cursor), expected.clone());

            let mut cursor = table.range(lower, upper, Order::Descending).unwrap();
            let mut reversed = expected.clone();
            reversed.reverse();
            prop_assert_eq!(collect(&mut cursor), reversed);

            prop_assert_eq!(table.count_range(lower, upper).unwrap(), expected.len() as u64);
        }
    }

    /// The same, for the key encoding whose exact digest lets the builder drop key bytes.
    #[test]
    fn i64_queries_match_the_model(
        entries in prop::collection::vec(
            (any::<i64>(), prop::collection::vec(any::<u8>(), 0..200)),
            1..120,
        ),
        page_size in prop::sample::select(vec![128usize, 256, 4096, 40_000]),
        eytzinger in any::<bool>(),
    ) {
        let rows: BTreeMap<i64, Vec<u8>> = entries.into_iter().collect();
        let bytes = build_i64(&rows, page_size, eytzinger);
        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();

        for (key, value) in &rows {
            let got = table.get(&Int64Encoding::encode(*key)).unwrap();
            prop_assert!(got.is_some(), "missing key {key}");
            let got = got.unwrap();
            prop_assert_eq!(got.as_bytes(), value.as_slice());
        }

        let mut cursor = table.scan(Order::Ascending).unwrap();
        let scanned = collect(&mut cursor);
        prop_assert_eq!(scanned.len(), rows.len());
        for ((got_key, got_value), (key, value)) in scanned.iter().zip(&rows) {
            prop_assert_eq!(got_key.as_slice(), &Int64Encoding::encode(*key));
            prop_assert_eq!(got_value, value);
        }

        // Keys that were not inserted are absent.
        for probe in [i64::MIN, i64::MAX, 0, 1, -1] {
            if !rows.contains_key(&probe) {
                prop_assert!(table.get(&Int64Encoding::encode(probe)).unwrap().is_none());
            }
        }
    }

    /// A secondary index resolves the same rows the model would.
    #[test]
    fn secondary_indexes_match_the_model(
        entries in prop::collection::vec(
            (any::<i64>(), prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'c']), 9..12)),
            1..90,
        ),
        page_size in prop::sample::select(vec![256usize, 1024, 4096]),
    ) {
        let rows: BTreeMap<i64, Vec<u8>> = entries.into_iter().collect();
        let mut builder = DatabaseBuilder::new().page_size(page_size).unwrap();
        let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
        builder
            .add_secondary_index(
                table,
                "by_head",
                false,
                Arc::new(AsciiEncoding),
                Box::new(|_k, v| Ok(v[..v.len().min(9)].to_vec())),
            )
            .unwrap();
        for (key, value) in &rows {
            builder.append(table, &Int64Encoding::encode(*key), value).unwrap();
        }
        let bytes = builder.build_to_vec().unwrap();

        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();
        let index = table.index("by_head").unwrap();
        prop_assert_eq!(index.count().unwrap(), rows.len() as u64);

        // Group the model the way the index does.
        let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for value in rows.values() {
            groups.entry(value[..value.len().min(9)].to_vec()).or_default().push(value.clone());
        }
        for (head, expected) in &groups {
            let mut cursor = index.lookup(head).unwrap();
            let mut found = Vec::new();
            while cursor.advance().unwrap() {
                prop_assert_eq!(cursor.key(), head.as_slice());
                found.push(cursor.value().unwrap().to_vec());
            }
            found.sort();
            let mut expected = expected.clone();
            expected.sort();
            prop_assert_eq!(found, expected);
        }
    }
}
