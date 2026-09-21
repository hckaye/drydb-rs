# Fuzz targets

Needs nightly and `cargo install cargo-fuzz`.

```
cargo +nightly fuzz run open-database
cargo +nightly fuzz run parse-page
cargo +nightly fuzz run build-then-read
cargo +nightly fuzz run rkyv-value
```

| Target | What it feeds, and what it asserts |
| --- | --- |
| `open-database` | Arbitrary bytes as a whole database file, then every read path over whatever opens. Any error is fine; a panic, a hang or a leak is not. |
| `parse-page` | Arbitrary bytes as one B+Tree page, then header parsing, validation and every entry accessor. |
| `build-then-read` | Arbitrary key/value pairs through the builder, then reads them back and compares against a `BTreeMap`. |
| `rkyv-value` | Arbitrary bytes as an enveloped rkyv value, then envelope parsing, validation and field access. |

The corpus is not checked in. `tests/interop/work/fixtures` makes a good seed corpus once the
interop tests have run: `cargo +nightly fuzz run open-database tests/interop/work/fixtures`.
