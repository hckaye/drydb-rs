//! The examples from the repository README, run as a test.
//!
//! A README example that no longer compiles is a README example that lies, so the ones
//! that use this crate are kept here and run. `README.ja.md` shows the same code.

use std::ops::Bound;
use std::sync::Arc;

use drydb::{
    AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, OpenOptions, Order, Result,
    VerifyOptions,
};

fn build(path: &std::path::Path) -> Result<()> {
    let mut builder = DatabaseBuilder::new().page_size(4096)?;
    let items = builder.create_table("items", Arc::new(Int64Encoding))?;
    builder.add_secondary_index(
        items,
        "by_name",
        false,
        Arc::new(AsciiEncoding),
        Box::new(|_key, value| Ok(value.to_vec())),
    )?;

    for id in 0..1_000i64 {
        let name = format!("item-{id:04}");
        builder.append(items, &Int64Encoding::encode(id), name.as_bytes())?;
    }

    // A second table, holding values too large for a leaf page.
    let assets = builder.create_table("assets", Arc::new(Int64Encoding))?;
    for id in 0..4i64 {
        builder.append(assets, &Int64Encoding::encode(id), &vec![b'x'; 200 * 1024])?;
    }

    let report = builder.build_to_file(path)?;
    assert_eq!(report.tables[0].rows, 1_000);
    assert!(report.page_count > 0 && report.file_size > 0);
    Ok(())
}

fn read(path: &std::path::Path) -> Result<()> {
    let db = Database::open(path)?;
    let table = db.table("items")?;

    // Point lookup.
    let value = table.get(&Int64Encoding::encode(42))?.expect("row 42");
    assert_eq!(value.as_bytes(), b"item-0042");

    // Range, and a count that reads no values.
    let mut cursor = table.range(
        Bound::Included(&Int64Encoding::encode(100)[..]),
        Bound::Excluded(&Int64Encoding::encode(200)[..]),
        Order::Ascending,
    )?;
    let mut seen = 0;
    while cursor.advance()? {
        let entry = cursor.current().expect("positioned");
        let _ = (entry.key(), entry.value());
        seen += 1;
    }
    assert_eq!(seen, 100);
    assert_eq!(
        table.count_range(Bound::Unbounded, Bound::Unbounded)?,
        1_000
    );

    // Secondary index.
    let index = table.index("by_name")?;
    let mut cursor = index.lookup(b"item-0042")?;
    let mut found = 0;
    while cursor.advance()? {
        let key = cursor.key();
        if let Some(value) = cursor.value() {
            assert_eq!(key, b"item-0042");
            assert_eq!(value.as_bytes(), b"item-0042");
        }
        found += 1;
    }
    assert_eq!(found, 1);

    // Streaming a value rather than making it resident.
    let assets = db.table("assets")?;
    if let Some(reader) = assets.blob_reader(&Int64Encoding::encode(2))? {
        let mut out = Vec::new();
        let copied = reader.copy_to(&mut out, 64 * 1024)?;
        assert_eq!(copied as usize, out.len());
        assert_eq!(out.len(), 200 * 1024);
    }

    // Checking the file.
    let report = db.verify(VerifyOptions::default())?;
    assert!(report.is_ok(), "{report:?}");
    Ok(())
}

fn read_on_a_budget(path: &std::path::Path) -> Result<()> {
    let db = OpenOptions::new()
        .memory_budget(8 * 1024 * 1024)
        .open(path)?;
    let table = db.table("items")?;
    assert!(table.get(&Int64Encoding::encode(42))?.is_some());
    let report = db.memory_report();
    assert!(report.budget_limit >= report.charged);
    Ok(())
}

#[test]
fn the_readme_examples_run() {
    let dir = std::env::temp_dir().join(format!("drydb-readme-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("game.drydb");

    build(&path).unwrap();
    read(&path).unwrap();
    read_on_a_budget(&path).unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}
