//! The rkyv example from the repository README, run as a test.

use std::sync::Arc;

use drydb::{Database, DatabaseBuilder, Int64Encoding};
use drydb_rkyv::{Codec, RkyvSchema, SchemaId};

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct Monster {
    name: String,
    hp: u32,
}

impl RkyvSchema for Monster {
    // Chosen by the application and stored in every value's envelope.
    const SCHEMA_ID: SchemaId = SchemaId(0x4D4F_4E53_5445_5201);
    const SCHEMA_VERSION: u16 = 1;
}

#[test]
fn the_readme_example_runs() {
    let codec = Codec::new();
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let monsters = builder
        .create_table("monsters", Arc::new(Int64Encoding))
        .unwrap();
    let value = codec
        .serialize(&Monster {
            name: "slime".to_string(),
            hp: 12,
        })
        .unwrap();
    builder
        .append(monsters, &Int64Encoding::encode(42), &value)
        .unwrap();
    let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();

    // Charging the database means the copy an unaligned archive needs is reserved.
    let codec = Codec::new().budget(Arc::clone(db.budget()));
    let monsters = db.table("monsters").unwrap();
    let mut read = 0;
    if let Some(value) = monsters.get(&Int64Encoding::encode(42)).unwrap() {
        let prepared = codec.prepare::<Monster>(value).unwrap();
        let monster = prepared.access().unwrap();
        assert_eq!(monster.hp, 12);
        read += 1;
    }
    assert_eq!(read, 1, "the row is there");
}
