//! A damaged file must produce errors, never a panic and never a hang.
//!
//! The sweep below mutates a valid database one byte at a time and runs the whole public
//! surface against the result. Every outcome is acceptable except a panic: the point is
//! that no byte pattern on disk can reach an unchecked index, an unbounded allocation or
//! an infinite loop.

use std::ops::Bound;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use drydb::{AsciiEncoding, DatabaseBuilder, ErrorKind, Int64Encoding, Limits, OpenOptions, Order};

/// A small database that exercises every layout the reader supports.
fn sample_database(page_size: usize, eytzinger: bool) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new()
        .page_size(page_size)
        .unwrap()
        .eytzinger_digests(eytzinger);
    let items = builder
        .create_table("items", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .add_secondary_index(
            items,
            "by_tag",
            false,
            Arc::new(AsciiEncoding),
            Box::new(|_k, v| Ok(v[..v.len().min(9)].to_vec())),
        )
        .unwrap();
    for i in 0..120i64 {
        let value = format!("tag{:06}-value-{i}", i % 7);
        builder
            .append(items, &Int64Encoding::encode(i), value.as_bytes())
            .unwrap();
    }
    // One value large enough to need a blob page.
    builder
        .append(items, &Int64Encoding::encode(999), &vec![0x5A; 4000])
        .unwrap();

    let words = builder
        .create_table("words", Arc::new(AsciiEncoding))
        .unwrap();
    for i in 0..60 {
        builder
            .append(words, format!("key{i:03}").as_bytes(), b"v")
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

/// Runs every read path. Errors are fine; panics are not.
fn exercise(bytes: Vec<u8>) {
    let options = OpenOptions::new().memory_budget(2 << 20).limits(Limits {
        max_stored_page_bytes: 1 << 20,
        max_decoded_page_bytes: 1 << 20,
        max_catalog_bytes: 1 << 16,
        max_tree_depth: 16,
    });
    let Ok(db) = options.open_bytes(bytes) else {
        return;
    };

    let _ = db.verify(Default::default());
    let _ = db.metrics();
    let _ = db.memory_report();

    let names: Vec<String> = match db.table_names() {
        Ok(names) => names.iter().map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    };
    for name in names {
        let Ok(table) = db.table(&name) else { continue };
        let _ = table.count();
        for key in [
            Int64Encoding::encode(0).to_vec(),
            Int64Encoding::encode(61).to_vec(),
            Int64Encoding::encode(999).to_vec(),
            b"key010".to_vec(),
            b"".to_vec(),
            vec![0xff; 40],
        ] {
            let _ = table.get(&key);
            let _ = table.contains_key(&key);
            let _ = table.blob_reader(&key);
            let _ = table.count_range(Bound::Included(&key[..]), Bound::Unbounded);
        }
        for order in [Order::Ascending, Order::Descending] {
            if let Ok(mut cursor) = table.scan(order) {
                let mut steps = 0;
                // The bound is generous but finite: a cyclic sibling chain must be
                // reported as corruption rather than scanned forever.
                while steps < 5000 {
                    match cursor.advance() {
                        Ok(true) => {
                            if let Some(entry) = cursor.current() {
                                let _ = entry.key().len() + entry.value().len();
                                let _ = entry.to_guard();
                            }
                        }
                        _ => break,
                    }
                    steps += 1;
                }
                assert!(steps < 5000, "a scan ran past every plausible bound");
            }
            let _ = table.prefix(b"key", order);
        }
        let index_names: Vec<String> = match table.index_names() {
            Ok(names) => names.iter().map(|s| s.to_string()).collect(),
            Err(_) => Vec::new(),
        };
        for index_name in index_names {
            let Ok(index) = table.index(&index_name) else {
                continue;
            };
            let _ = index.count();
            let _ = index.get(b"tag000000");
            if let Ok(mut cursor) = index.scan(Order::Ascending) {
                let mut steps = 0;
                while steps < 5000 {
                    match cursor.advance() {
                        Ok(true) => {
                            let _ = cursor.key().len();
                            let _ = cursor.value().map(|v| v.len());
                        }
                        _ => break,
                    }
                    steps += 1;
                }
                assert!(steps < 5000, "an index scan ran past every plausible bound");
            }
        }
    }
}

/// Deterministic generator, so a failing case can be reproduced from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 17) ^ (self.0 >> 33)
    }
}

fn sweep(label: &str, original: Vec<u8>, mutations: usize, seed: u64) {
    let panics = Arc::new(AtomicUsize::new(0));
    let previous = std::panic::take_hook();
    {
        let panics = Arc::clone(&panics);
        std::panic::set_hook(Box::new(move |info| {
            panics.fetch_add(1, Ordering::SeqCst);
            eprintln!("panic during the corruption sweep: {info}");
        }));
    }

    let mut rng = Rng(seed);
    let mut failures = Vec::new();
    for _ in 0..mutations {
        let offset = (rng.next() as usize) % original.len();
        let patch = (rng.next() % 256) as u8;
        let mut bytes = original.clone();
        if bytes[offset] == patch {
            bytes[offset] = patch.wrapping_add(1);
        } else {
            bytes[offset] = patch;
        }
        let snapshot = bytes.clone();
        if catch_unwind(AssertUnwindSafe(|| exercise(bytes))).is_err() {
            failures.push((offset, snapshot[offset]));
        }
    }

    // Truncations, which hit a different set of bounds checks.
    for cut in [
        0usize,
        1,
        25,
        26,
        27,
        64,
        200,
        original.len() / 2,
        original.len() - 1,
    ] {
        if cut >= original.len() {
            continue;
        }
        let bytes = original[..cut].to_vec();
        if catch_unwind(AssertUnwindSafe(|| exercise(bytes))).is_err() {
            failures.push((cut, 0));
        }
    }

    std::panic::set_hook(previous);
    assert!(
        failures.is_empty(),
        "[{label}] {} mutations panicked, first at offset {:?}",
        failures.len(),
        failures.first()
    );
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "[{label}] a panic escaped the sweep"
    );
}

#[test]
fn single_byte_mutations_never_panic() {
    sweep(
        "compact-omitted",
        sample_database(512, false),
        1500,
        0x5EED_0001,
    );
}

#[test]
fn single_byte_mutations_never_panic_eytzinger() {
    sweep("eytzinger", sample_database(512, true), 1000, 0x5EED_0002);
}

#[test]
fn single_byte_mutations_never_panic_classic_meta() {
    sweep(
        "classic-meta",
        sample_database(40_000, false),
        800,
        0x5EED_0003,
    );
}

#[test]
fn random_bytes_are_rejected() {
    let mut rng = Rng(0x5EED_0004);
    for len in [0usize, 1, 26, 64, 512, 4096] {
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next() % 256) as u8).collect();
        assert!(
            drydb::Database::open_bytes(bytes).is_err(),
            "random {len} bytes must not open as a database"
        );
    }
}

#[test]
fn a_page_pointing_at_itself_is_reported() {
    // A root page whose only child is itself would otherwise loop forever.
    let mut bytes = sample_database(256, false);
    let db = drydb::Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap();
    drop(db);

    // Point every internal child ordinal at the root by rewriting the page's payload is
    // fiddly; instead make the root's right sibling point at itself, which the sibling
    // walk has to notice.
    let directory = {
        let db = drydb::Database::open_bytes(bytes.clone()).unwrap();
        let catalog = db.catalog().clone();
        catalog.directory_position.get() as usize
    };
    let slot = directory + root.get() as usize * 8;
    let offset = i64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap()) as usize;
    // Right sibling field sits 20 bytes into the page.
    bytes[offset + 20..offset + 28].copy_from_slice(&(root.get() as i64).to_le_bytes());

    let db = drydb::Database::open_bytes(bytes).unwrap();
    let table = db.table("items").unwrap();
    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut steps = 0u64;
    let outcome = loop {
        match cursor.advance() {
            Ok(true) => steps += 1,
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
        // The cursor's own bound is the page count, so this can only be reached if the
        // guard is missing entirely.
        assert!(
            steps < 1_000_000,
            "the cursor followed a cyclic sibling chain"
        );
    };
    // Either the scan ends or it reports corruption; it does not spin.
    if let Err(e) = outcome {
        assert!(matches!(e.kind(), ErrorKind::CorruptData));
    }
}

#[test]
fn a_file_declaring_two_filters_is_rejected() {
    // Upstream can neither write nor read such a file, so this reader refuses it rather
    // than guessing a chaining order.
    let mut bytes = sample_database(512, false);
    bytes[6..8].copy_from_slice(&2u16.to_le_bytes());
    let err = drydb::Database::open_bytes(bytes).unwrap_err();
    assert!(matches!(
        err.kind(),
        ErrorKind::Unsupported | ErrorKind::CorruptData
    ));
}

#[test]
fn an_unknown_filter_is_named_in_the_error() {
    let mut bytes = sample_database(512, false).to_vec();
    // Splice a one-filter header in front of the descriptors.
    let mut spliced = bytes[..26].to_vec();
    spliced[6..8].copy_from_slice(&1u16.to_le_bytes());
    spliced.push(4);
    spliced.extend_from_slice(b"nope");
    spliced.extend_from_slice(&bytes[26..]);
    // Offsets have shifted, so the directory position must move too.
    let directory = i64::from_le_bytes(spliced[18..26].try_into().unwrap()) + 5;
    spliced[18..26].copy_from_slice(&directory.to_le_bytes());
    bytes = spliced;

    let err = drydb::Database::open_bytes(bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnknownFilter);
    assert!(err.to_string().contains("nope"), "{err}");
}

#[test]
fn unknown_key_encodings_are_rejected() {
    let bytes = sample_database(512, false);
    let mut patched = bytes.clone();
    let position = patched
        .windows(3)
        .position(|w| w == b"i64")
        .expect("encoding id");
    patched[position..position + 3].copy_from_slice(b"xyz");
    let err = drydb::Database::open_bytes(patched).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnknownEncoding);
}

#[test]
fn a_budget_too_small_for_one_page_is_reported_at_open() {
    let bytes = sample_database(4096, false);
    let err = OpenOptions::new()
        .memory_budget(1024)
        .open_bytes(bytes)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
}
