//! That a collected range stays inside the database's memory budget.
//!
//! The rows a collection hands back are memory the caller keeps, so they are reserved
//! before they are made: the list, and the copy of each key. What a decoded value holds
//! inside itself belongs to the caller's type and is not counted.

use std::ops::Bound;
use std::sync::Arc;

use drydb::{DatabaseBuilder, Int64Encoding, OpenOptions, Order};
use drydb_msgpack::{MessagePackCodec, MessagePackTable};

fn build(rows: i64) -> Vec<u8> {
    let codec = MessagePackCodec::new();
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    for i in 0..rows {
        let value = codec.serialize(&42u8).unwrap();
        builder
            .append(table, &Int64Encoding::encode(i), &value)
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

#[test]
fn a_collected_range_is_reserved_before_it_is_built() {
    let rows = 10_000i64;
    let budget = 65_536;
    let db = OpenOptions::new()
        .memory_budget(budget)
        .cache_shards(1)
        .open_bytes(build(rows))
        .unwrap();
    let table = MessagePackTable::new(db.table("items").unwrap(), MessagePackCodec::new());

    // Ten thousand rows is six hundred kilobytes of list and keys, which this budget
    // does not have. Asking for them says so instead of allocating them.
    let err = table
        .collect_range::<u8>(
            Bound::Unbounded,
            Bound::Unbounded,
            Order::Ascending,
            rows as usize,
        )
        .expect_err("the whole table does not fit in the budget");
    assert_eq!(err.kind(), drydb::ErrorKind::BudgetExceeded);

    // A collection that fits is charged while it is held and released when it goes.
    let collected = table
        .collect_range::<u8>(Bound::Unbounded, Bound::Unbounded, Order::Ascending, 32)
        .unwrap();
    assert_eq!(collected.len(), 32);
    assert_eq!(collected[0].1, 42);
    assert!(collected.charged_bytes() >= 32 * 24);
    let held = db.charged_bytes();
    drop(collected);
    assert!(
        db.charged_bytes() < held,
        "holding the rows charged {held} bytes and dropping them did not give any back"
    );
}
