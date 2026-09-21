//! MessagePack values written by one implementation and read by the other.
//!
//! Compatibility here is per DTO, so this pins one: a fixture type covering integers,
//! a string, a float, a bool, an array, a binary blob and a nullable field, in both of
//! the layouts MessagePack-CSharp can write.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use drydb::{Database, DatabaseBuilder, Int64Encoding};
use drydb_msgpack::{Layout, MessagePackCodec, MessagePackTable};
use serde::{Deserialize, Serialize};

const ROW_COUNT: i64 = 40;

/// The array layout: field order matches the C# `[Key(n)]` numbering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ArrayItem {
    id: i64,
    name: String,
    level: i32,
    ratio: f64,
    active: bool,
    tags: Vec<i32>,
    #[serde(with = "serde_bytes")]
    blob: Vec<u8>,
    note: Option<String>,
}

/// The map layout: field names match the C# property names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MapItem {
    id: i64,
    name: String,
    level: i32,
    ratio: f64,
    active: bool,
    tags: Vec<i32>,
    #[serde(with = "serde_bytes")]
    blob: Vec<u8>,
    note: Option<String>,
}

/// What both implementations agree a row should contain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Row {
    id: i64,
    name: String,
    level: i32,
    ratio: f64,
    active: bool,
    tags: Vec<i32>,
    blob: String,
    note: Option<String>,
}

fn expected(i: i64) -> ArrayItem {
    ArrayItem {
        id: i * 1000 - 5000,
        name: format!("item-{i:03}"),
        level: (i % 7) as i32,
        ratio: i as f64 / 4.0,
        active: i % 2 == 0,
        tags: (0..(i % 5) as i32).map(|t| t * 3).collect(),
        blob: (0..(i % 9) as u8).map(|b| b.wrapping_mul(7)).collect(),
        note: if i % 3 == 0 {
            None
        } else {
            Some(format!("note {i}"))
        },
    }
}

fn to_map(item: &ArrayItem) -> MapItem {
    MapItem {
        id: item.id,
        name: item.name.clone(),
        level: item.level,
        ratio: item.ratio,
        active: item.active,
        tags: item.tags.clone(),
        blob: item.blob.clone(),
        note: item.note.clone(),
    }
}

fn to_row(item: &ArrayItem) -> Row {
    Row {
        id: item.id,
        name: item.name.clone(),
        level: item.level,
        ratio: item.ratio,
        active: item.active,
        tags: item.tags.clone(),
        blob: drydb_interop::b64(&item.blob),
        note: item.note.clone(),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn oracle() -> Option<PathBuf> {
    let required = std::env::var("DRYDB_INTEROP").ok().as_deref() == Some("1");
    let root = repo_root();
    let project = root.join("tests/interop/DryDbOracle/DryDbOracle.csproj");
    if !root
        .join("tests/interop/upstream/src/DryDB/DryDB.csproj")
        .exists()
    {
        assert!(!required, "upstream checkout missing");
        eprintln!("skipping msgpack interop: upstream checkout missing");
        return None;
    }
    let dotnet_ok = Command::new("dotnet")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !dotnet_ok {
        assert!(!required, "the .NET SDK is not on PATH");
        eprintln!("skipping msgpack interop: the .NET SDK is not on PATH");
        return None;
    }
    // A directory of its own: see the note in `interop.rs`.
    let out = root.join("tests/interop/work/oracle-msgpack");
    let status = Command::new("dotnet")
        .args(["build", "-c", "Release", "-v", "quiet", "--nologo"])
        .arg(&project)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("dotnet build");
    assert!(status.success(), "building the oracle failed");
    Some(out.join("DryDbOracle.dll"))
}

fn run_oracle(dll: &Path, args: &[&str]) {
    let output = Command::new("dotnet")
        .arg(dll)
        .args(args)
        .output()
        .expect("run the oracle");
    assert!(
        output.status.success(),
        "oracle {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn build_with_rust(layout: Layout, path: &Path) {
    let codec = MessagePackCodec::with_layout(layout);
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    for i in 0..ROW_COUNT {
        let item = expected(i);
        let bytes = match layout {
            Layout::Array => codec.serialize(&item).unwrap(),
            Layout::Map => codec.serialize(&to_map(&item)).unwrap(),
        };
        builder
            .append(table, &Int64Encoding::encode(i), &bytes)
            .unwrap();
    }
    if path.exists() {
        std::fs::remove_file(path).unwrap();
    }
    builder.build_to_file(path).unwrap();
}

fn read_with_rust(layout: Layout, path: &Path) -> Vec<Row> {
    let db = Database::open(path).unwrap();
    let table = MessagePackTable::new(
        db.table("items").unwrap(),
        MessagePackCodec::with_layout(layout),
    );
    (0..ROW_COUNT)
        .map(|i| {
            let key = Int64Encoding::encode(i);
            match layout {
                Layout::Array => {
                    let item: ArrayItem = table.get(&key).unwrap().expect("row present");
                    to_row(&item)
                }
                Layout::Map => {
                    let item: MapItem = table.get(&key).unwrap().expect("row present");
                    to_row(&ArrayItem {
                        id: item.id,
                        name: item.name,
                        level: item.level,
                        ratio: item.ratio,
                        active: item.active,
                        tags: item.tags,
                        blob: item.blob,
                        note: item.note,
                    })
                }
            }
        })
        .collect()
}

fn read_dump(path: &Path) -> Vec<Row> {
    let text = std::fs::read_to_string(path).expect("read the dump");
    serde_json::from_str(&text).expect("parse the dump")
}

#[test]
fn messagepack_values_cross_both_ways() {
    let Some(dll) = oracle() else { return };
    let dir = repo_root().join("tests/interop/work/msgpack");
    std::fs::create_dir_all(&dir).expect("work dir");

    let model: Vec<Row> = (0..ROW_COUNT).map(|i| to_row(&expected(i))).collect();

    for (layout, name) in [(Layout::Array, "array"), (Layout::Map, "map")] {
        let csharp_db = dir.join(format!("{name}.csharp.drydb"));
        let rust_db = dir.join(format!("{name}.rust.drydb"));

        run_oracle(
            &dll,
            &["msgpack-build", name, &csharp_db.display().to_string()],
        );
        build_with_rust(layout, &rust_db);

        // What C# wrote, read here.
        assert_eq!(
            read_with_rust(layout, &csharp_db),
            model,
            "[{name}] Rust reading C#"
        );

        // What this crate wrote, read by C#.
        let dump = dir.join(format!("{name}.csharp-of-rust.json"));
        run_oracle(
            &dll,
            &[
                "msgpack-dump",
                name,
                &rust_db.display().to_string(),
                &dump.display().to_string(),
            ],
        );
        assert_eq!(read_dump(&dump), model, "[{name}] C# reading Rust");

        // And the bytes themselves match, field for field.
        let csharp = Database::open(&csharp_db).unwrap();
        let rust = Database::open(&rust_db).unwrap();
        let csharp_table = csharp.table("items").unwrap();
        let rust_table = rust.table("items").unwrap();
        for i in 0..ROW_COUNT {
            let key = Int64Encoding::encode(i);
            assert_eq!(
                csharp_table.get(&key).unwrap().unwrap().as_bytes(),
                rust_table.get(&key).unwrap().unwrap().as_bytes(),
                "[{name}] row {i} encodes differently"
            );
        }
    }
}
