//! Arbitrary rows through the builder, then read back and compared to a model.
//!
//! The other targets ask whether bad bytes break the reader. This one asks whether the
//! writer and the reader agree, whatever the data looks like.

#![no_main]

use std::collections::BTreeMap;
use std::sync::Arc;

use arbitrary::Arbitrary;
use drydb::{AsciiEncoding, Database, DatabaseBuilder, Order};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct Input {
    page_size_choice: u8,
    eytzinger: bool,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
}

fuzz_target!(|input: Input| {
    if input.rows.is_empty() || input.rows.len() > 400 {
        return;
    }
    let page_size = match input.page_size_choice % 4 {
        0 => 128usize,
        1 => 512,
        2 => 4096,
        _ => 40_000,
    };
    let model: BTreeMap<Vec<u8>, Vec<u8>> = input
        .rows
        .into_iter()
        .filter(|(k, v)| k.len() <= 512 && v.len() <= 4096)
        .collect();
    if model.is_empty() {
        return;
    }

    let mut builder = DatabaseBuilder::new()
        .page_size(page_size)
        .expect("page size from a fixed set")
        .eytzinger_digests(input.eytzinger);
    let table = builder
        .create_table("t", Arc::new(AsciiEncoding))
        .expect("new table");
    for (key, value) in &model {
        // A key too large for the page size is a reported error, not a panic.
        if builder.append(table, key, value).is_err() {
            return;
        }
    }
    let Ok(bytes) = builder.build_to_vec() else {
        return;
    };

    let db = Database::open_bytes(bytes).expect("a database this crate wrote must open");
    let read = db.table("t").expect("the table it wrote");
    assert_eq!(read.count().expect("count"), model.len() as u64);

    for (key, value) in &model {
        let got = read.get(key).expect("lookup").expect("key present");
        assert_eq!(got.as_bytes(), value.as_slice());
    }

    let mut cursor = read.scan(Order::Ascending).expect("scan");
    let mut expected = model.iter();
    while cursor.advance().expect("advance") {
        let entry = cursor.current().expect("positioned");
        let (key, value) = expected.next().expect("no extra rows");
        assert_eq!(entry.key(), key.as_slice());
        assert_eq!(entry.value(), value.as_slice());
    }
    assert!(expected.next().is_none(), "the scan stopped early");

    let report = db.verify(Default::default()).expect("verify runs");
    assert!(report.is_ok(), "{:?}", report.problems);
});
