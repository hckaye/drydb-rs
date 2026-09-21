# drydb-rkyv

Record-level [rkyv](https://rkyv.org) value codec for [`drydb`](https://github.com/hckaye/drydb-rs/tree/main/crates/drydb).

One DryDB value holds one self-contained rkyv archive behind a 24-byte envelope that
records the schema and the rkyv configuration it was written with. The database's own
structures are untouched, so the file stays an ordinary DryDB 1.4 file and the C#
implementation still reads these values as opaque bytes.

What it does not do: make rkyv values readable as typed data from C#, or migrate a value
from one schema version to another.

See the crate documentation for the alignment rules, what "zero copy" covers, and how a
mismatched rkyv feature configuration is detected.
