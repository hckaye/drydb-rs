//! The async example from the repository README, run as a test.

use std::sync::Arc;

use drydb::{DatabaseBuilder, Int64Encoding};
use drydb_async::AsyncDatabase;

#[tokio::test]
async fn the_readme_example_runs() {
    let dir = std::env::temp_dir().join(format!("drydb-async-readme-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("game.drydb");

    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(42), b"item-0042")
        .unwrap();
    builder.build_to_file(&path).unwrap();

    let db = AsyncDatabase::open(&path).await.unwrap();
    let table = db.table("items").unwrap();
    let value = table.get(&Int64Encoding::encode(42)).await.unwrap();
    assert_eq!(value.unwrap().as_bytes(), b"item-0042");

    let _ = std::fs::remove_dir_all(&dir);
}
