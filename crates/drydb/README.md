# drydb

A read-only embedded key/value store for Rust, over the [DryDB](https://github.com/hadashiA/DryDB) 1.4 file format.

A database is built once into an immutable file. At runtime a query loads only the B+Tree pages it touches, so nothing is deserialised up front, no index is rebuilt at startup, and the file does not have to fit in memory.

- Point lookups, ranges, prefixes and counts, ascending or descending.
- Secondary indexes, unique and non-unique.
- Values stored on their own page, streamed instead of made resident.
- A builder that takes rows in any order and sorts them through a bounded buffer.
- `verify`, which walks every page and every tree and reports structural damage.

Everything the crate allocates on a caller's behalf is reserved against `OpenOptions::memory_budget` before it is allocated, and a request that does not fit is refused rather than allocated anyway.

A damaged file produces an error, never a panic and never undefined behaviour: file bytes are not reinterpreted as Rust structs, and the crate is `#![deny(unsafe_code)]` apart from the mapping call behind the `mmap` feature.

Optional features: `mmap` for a memory mapped page source, `zstd` for the `DryDB.ZstdCompression` page filter.

Usage, the compatibility scope with the C# implementation and the design documents are in the [repository README](https://github.com/hckaye/drydb-rs#readme).
