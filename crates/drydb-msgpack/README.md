# drydb-msgpack

MessagePack value codec for [`drydb`](https://github.com/hckaye/drydb-rs/tree/main/crates/drydb), aimed at reading and writing the same
bytes as [MessagePack-CSharp](https://github.com/MessagePack-CSharp/MessagePack-CSharp).

Two layouts, matching the two ways a C# DTO can be declared:

| This crate | C# |
| --- | --- |
| `Layout::Array` (default) | `[MessagePackObject]` with integer `[Key]`s |
| `Layout::Map` | `[MessagePackObject(keyAsPropertyName: true)]`, or the contractless resolver |

Compatibility is per DTO, not blanket: the two sides agree when the field order, field
names and integer widths line up, and the interop tests pin that down for a fixture DTO
rather than claiming it in general.
