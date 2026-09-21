# drydb-rs

A read-only embedded key/value store for Rust, over the [DryDB](https://github.com/hadashiA/DryDB) 1.4 file format.

A database is built once into an immutable file. At runtime a query loads only the B+Tree pages it touches, so nothing is deserialised up front, no index is rebuilt at startup, and the file does not have to fit in memory. SQL, updates at runtime, transactions, WAL and MVCC are out of scope.

日本語版: [README.ja.md](README.ja.md)

## Relationship to DryDB

DryDB is a read-only embedded database written in C# by [hadashiA](https://github.com/hadashiA). This project reads and writes the same files: a file built by either implementation can be read by the other.

The target is storage format 1.4 at upstream commit `6b175929491793948e63430c20c2d6f58300d97f`. No DryDB source is copied into this repository. The format was implemented from the upstream sources and documentation, and `tests/interop/fetch-upstream.sh` clones that pinned commit into an ignored directory so the interoperability tests can run both implementations over the same fixtures. The places where the two disagree are recorded, with the measured upstream behaviour, in the [exception ledger](docs/compatibility.md#6-例外台帳).

## Installing

The crates are not on crates.io. Take them from the repository:

```toml
[dependencies]
drydb = { git = "https://github.com/hckaye/drydb-rs" }
```

With default features `drydb` has no dependencies. Two features are optional:

| Feature | What it adds |
| --- | --- |
| `mmap` | A memory mapped page source. Pages are copied out of the mapping, so no reference into it escapes, but `MmapSource::open` is `unsafe`: the caller guarantees the file does not change while it is open, and pages the OS keeps resident are outside the memory budget. |
| `zstd` | The `DryDB.ZstdCompression` page filter, so files written with upstream compression can be read and written. |

| Crate | Contents |
| --- | --- |
| [`drydb`](crates/drydb) | Format decoding, page I/O, page cache, B+Tree search, cursors, secondary indexes, blobs, the builder, `verify` |
| [`drydb-rkyv`](crates/drydb-rkyv) | Record-level rkyv value codec |
| [`drydb-msgpack`](crates/drydb-msgpack) | MessagePack value codec, reading and writing the same bytes as MessagePack-CSharp |
| [`drydb-async`](crates/drydb-async) | Async adapter over tokio's blocking pool |
| [`drydb-cli`](crates/drydb-cli) | The `drydb` command |

## Building a file

Rows arrive in any order. The builder sorts them, spilling to a temporary file when they no longer fit in the sort buffer, and writes the tree in one pass.

```rust
use std::sync::Arc;

use drydb::{AsciiEncoding, DatabaseBuilder, Int64Encoding};

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

let report = builder.build_to_file("game.drydb")?;
println!("{} pages, {} bytes", report.page_count, report.file_size);
```

`build_to_vec` returns the same file as bytes instead of writing it.

The key encoding decides the order and the 64-bit digest a search compares. `Int64Encoding`, `AsciiEncoding`, `Uuidv7Encoding` and `UlidEncoding` come with the crate, and the `KeyEncoding` trait takes your own.

## Reading a file

### Point lookups

```rust
use drydb::{Database, Int64Encoding};

let db = Database::open("game.drydb")?;
let table = db.table("items")?;

if let Some(value) = table.get(&Int64Encoding::encode(42))? {
    // Borrowed from the page in the cache, which the guard keeps resident.
    println!("{} bytes", value.as_bytes().len());
}
```

A `ValueGuard` holds its page. Eviction, other queries and dropping the `Database` do not invalidate the bytes in hand.

### Ranges, prefixes and counts

```rust
use std::ops::Bound;

use drydb::Order;

let mut cursor = table.range(
    Bound::Included(&Int64Encoding::encode(100)[..]),
    Bound::Excluded(&Int64Encoding::encode(200)[..]),
    Order::Ascending,
)?;
while cursor.advance()? {
    let entry = cursor.current().expect("positioned");
    let _ = (entry.key(), entry.value());
}

let rows = table.count_range(Bound::Unbounded, Bound::Unbounded)?;
```

A cursor holds the leaf it is on, so what a scan holds does not grow with the table. A value too large for a leaf page lives on a page of its own, and stepping onto that row reads the whole of it, so a scan also has to fit the largest such value it passes: over rows of two hundred kilobytes it asks for that much, where `count` asks for nothing and `BlobReader` asks for one chunk. `scan` walks the whole table, `prefix` walks the keys that start with a byte string, and `count` and `count_range` read no values at all.

### Secondary indexes

An index maps its own key to a reference to the row in the primary tree. Reading through it resolves that reference.

```rust
let index = table.index("by_name")?;
let mut cursor = index.lookup(b"item-0042")?;
while cursor.advance()? {
    let key = cursor.key();
    if let Some(value) = cursor.value() {
        let _ = (key, value.as_bytes());
    }
}
```

`get` answers with the first row under the key, which on a non-unique index is the matching row whose primary key sorts first. `lookup` gives every row under the key, in that same order.

### Values too large to keep resident

`Table::get` makes the whole value resident. A value too large for a leaf page is stored on a page of its own, and such a value can be streamed instead, allocating only the chunk buffer asked for. The stream reads the file directly, so it needs a database without a page filter.

```rust
let assets = db.table("assets")?;
if let Some(reader) = assets.blob_reader(&Int64Encoding::encode(2))? {
    let mut out = Vec::new();
    let copied = reader.copy_to(&mut out, 64 * 1024)?;
    println!("{copied} bytes");
}
```

`BlobReader` also implements `Read` and `Seek`. It answers `None` for a key that is not there, and refuses a value stored inline in its leaf page, which `get` reads.

### Typed values

A value is bytes as far as the database is concerned. Two adapters give them a type. Both write and read with the same codec, so each example here builds its own table.

```rust
use drydb_msgpack::{MessagePackCodec, MessagePackTable};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Item {
    name: String,
    price: u32,
}

let codec = MessagePackCodec::new();
let mut builder = DatabaseBuilder::new().page_size(4096)?;
let stock = builder.create_table("stock", Arc::new(Int64Encoding))?;
let value = codec.serialize(&Item { name: "sword".to_string(), price: 120 })?;
builder.append(stock, &Int64Encoding::encode(42), &value)?;
let db = Database::open_bytes(builder.build_to_vec()?)?;

let stock = MessagePackTable::new(db.table("stock")?, MessagePackCodec::new());
let item: Option<Item> = stock.get(&Int64Encoding::encode(42))?;
```

`drydb-rkyv` reads a value as an rkyv archive without copying it, when the archive lands aligned in the page. When it does not, the codec copies that one value, and it charges the copy to the database it is given:

```rust
use std::sync::Arc;

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

let codec = Codec::new();
let mut builder = DatabaseBuilder::new().page_size(4096)?;
let monsters = builder.create_table("monsters", Arc::new(Int64Encoding))?;
let value = codec.serialize(&Monster { name: "slime".to_string(), hp: 12 })?;
builder.append(monsters, &Int64Encoding::encode(42), &value)?;
let db = Database::open_bytes(builder.build_to_vec()?)?;

// Charging the database means the copy an unaligned archive needs is reserved.
let codec = Codec::new().budget(Arc::clone(db.budget()));
let monsters = db.table("monsters")?;
if let Some(value) = monsters.get(&Int64Encoding::encode(42))? {
    let prepared = codec.prepare::<Monster>(value)?;
    let monster = prepared.access()?;
    println!("{}", monster.hp);
}
```

A codec built with `Codec::new()` alone has no budget to charge, so its copies are outside the one the database keeps. `Codec::reserve_with` takes the `Database` or the `Table` instead, which can reclaim a cached page to make room rather than refusing.

C# reads rkyv values as opaque bytes. MessagePack values are typed on both sides, per DTO.

### From async code

Reading a page is a blocking file read. The adapter says so rather than hiding it: a lookup completes on the calling thread when every page it needs is cached, and goes to `tokio::task::spawn_blocking` when it does not.

```rust
use drydb_async::AsyncDatabase;

let db = AsyncDatabase::open("game.drydb").await?;
let table = db.table("items")?;
let value = table.get(&Int64Encoding::encode(42)).await?;
```

## Memory

`OpenOptions::memory_budget` is the ceiling on what the crate allocates on your behalf.

```rust
use drydb::OpenOptions;

let db = OpenOptions::new()
    .memory_budget(8 * 1024 * 1024)
    .open("game.drydb")?;
```

Every such allocation is reserved before it is made: page buffers, the parsed catalog, the page directory, decompression buffers, a query's copies of its bounds, a cursor's key buffers, and the rows a collected scan returns. What does not fit is refused with `ErrorKind::BudgetExceeded` after reclaiming what the cache can give back, rather than allocated anyway. Nothing waits for room, so a budget too small produces an error and never a deadlock.

That covers `drydb` itself, including what it hands back: a verification report and a list of table names come with the reservation that paid for them, and give it up when they are dropped. It covers the rows `drydb-async` and `drydb-msgpack` hand back as well. An adapter charges the same budget when it is given it: `drydb-rkyv` charges the copy it makes for an unaligned archive once the codec has `budget` or `reserve_with`, and not before. What a decoded value allocates inside itself belongs to your own type, and is not counted.

The budget is not a ceiling on resident set size. It does not cover the allocator's own overhead, values you copy out yourself, the operating system's page cache, pages an `mmap` source keeps resident, or the image handed to `Database::open_bytes`, which is what is being read the way a file on disk is. `Database::memory_report` returns the split.

## Checking a file

```rust
use drydb::VerifyOptions;

let report = db.verify(VerifyOptions::default())?;
if !report.is_ok() {
    for problem in &report.problems {
        println!("{problem:?}");
    }
}
```

The pass walks every page and every tree, holding a bounded number of pages while it does. Running out of budget, or a read that fails, comes back as `Err` rather than as a finding: the pass did not learn that the file is damaged, it learned that it could not look.

## Command line

```sh
# Build a file from a tab separated `key<TAB>value` input.
drydb build game.drydb --table items --encoding i64 --input rows.tsv

drydb inspect game.drydb
drydb verify game.drydb
drydb get game.drydb items 100
drydb range game.drydb items --from 100 --to 200 --limit 20
drydb count game.drydb items
drydb prefix names.drydb names ch
```

`get`, `range` and `count` take `--index <name>` to read through a secondary index instead of the primary key. Keys are printed and read back in the same form, which `--key-format` selects. Run `drydb --help` for the full list.

## What this implementation guarantees

**Damaged input produces an error.** File bytes are never reinterpreted as Rust structs; every field is decoded from little-endian bytes through length-checked slices. The crate is `#![deny(unsafe_code)]`, with the one exception of the `mmap` mapping call. A damaged file yields `CorruptData`, never a panic, never undefined behaviour, and never a short answer presented as a complete one. Tests flip single bytes of a valid file and run every read path over the result.

**Memory stays inside the budget**, as described above.

**Values outlive the queries that produced them.** A `ValueGuard` keeps its page resident for as long as it is held.

## Compatibility with the C# implementation

Interoperability is checked with 12 fixtures covering i64, ascii, uuidv7 and ulid keys, classic and compact metadata, Eytzinger digests, the layout that omits key bytes, overflow values, unique and non-unique secondary indexes, and zstd compression. Each fixture is built by both implementations and read by both, comparing the four combinations. For 10 of the 12 the two outputs match byte for byte.

Nine differences are recorded in the [exception ledger](docs/compatibility.md#6-例外台帳), each with a test that pins the measured upstream behaviour as an assertion. Most are places where the upstream builder and reader disagree with each other, and this implementation reads files from both.

## Working on the code

```sh
cargo test --workspace --exclude drydb-interop

# Against the C# implementation. Needs the .NET SDK.
./tests/interop/fetch-upstream.sh
DRYDB_INTEROP=1 cargo test -p drydb-interop

# Undefined behaviour
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p drydb --lib
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p drydb --test miri_smoke

# Fuzzing. Needs cargo-fuzz.
cd fuzz && cargo +nightly fuzz run open-database

# Measurements
cargo bench -p drydb --features zstd
```

The minimum supported Rust version is 1.85, and the library, every feature and the whole test suite are checked against it.

## Documents

The design documents are in Japanese.

| Document | Contents |
| --- | --- |
| [Architecture](docs/architecture.md) | Ownership, the page cache, the memory contract, I/O, safety |
| [Compatibility](docs/compatibility.md) | The wire format, what was measured, the supported range, the exception ledger |
| [rkyv codec](docs/rkyv-design.md) | The envelope, profile detection, alignment, validation |
| [Benchmarks](docs/benchmarks.md) | Method, baseline figures, the optimisations taken and refused |
| [Implementation plan](docs/implementation-plan.md) | Status, how the work is verified, design decisions |
| [Task list](docs/tasks.md) | Each item and the test that covers it |

## License and attribution

This repository is [MIT](LICENSE) licensed, Copyright (c) 2026 hckaye.

DryDB is a separate project by hadashiA, also MIT licensed, Copyright (c) 2024 hadashiA. Its source is not included or redistributed here: `tests/interop/fetch-upstream.sh` clones the pinned commit into a directory that is not tracked by git, and it is used only to run the interoperability tests. The sources and documents consulted while implementing the format are listed, with commit-pinned links, in the [primary sources](docs/compatibility.md#8-一次資料) section of the compatibility document.
