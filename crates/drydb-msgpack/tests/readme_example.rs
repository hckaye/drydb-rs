//! The MessagePack example from the repository README, run as a test.

use std::sync::Arc;

use drydb::{Database, DatabaseBuilder, Int64Encoding};
use drydb_msgpack::{MessagePackCodec, MessagePackTable};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Item {
    name: String,
    price: u32,
}

#[test]
fn the_readme_example_runs() {
    let codec = MessagePackCodec::new();
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let stock = builder
        .create_table("stock", Arc::new(Int64Encoding))
        .unwrap();
    let value = codec
        .serialize(&Item {
            name: "sword".to_string(),
            price: 120,
        })
        .unwrap();
    builder
        .append(stock, &Int64Encoding::encode(42), &value)
        .unwrap();
    let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();

    let stock = MessagePackTable::new(db.table("stock").unwrap(), MessagePackCodec::new());
    let item: Option<Item> = stock.get(&Int64Encoding::encode(42)).unwrap();
    assert_eq!(item.unwrap().price, 120);
}
