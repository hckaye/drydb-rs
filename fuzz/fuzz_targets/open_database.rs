//! Arbitrary bytes as a database file.
//!
//! Most inputs fail to open, which is the expected outcome. What this looks for is the
//! input that opens and then makes a read path panic, loop or allocate without bound.

#![no_main]

use std::ops::Bound;

use drydb::{Int64Encoding, Limits, OpenOptions, Order};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let options = OpenOptions::new().memory_budget(2 << 20).limits(Limits {
        max_stored_page_bytes: 1 << 20,
        max_decoded_page_bytes: 1 << 20,
        max_catalog_bytes: 1 << 16,
        max_tree_depth: 16,
    });
    let Ok(db) = options.open_bytes(data.to_vec()) else {
        return;
    };

    let _ = db.verify(Default::default());

    let names: Vec<String> = match db.table_names() {
        Ok(names) => names.iter().map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    };
    for name in names {
        let Ok(table) = db.table(&name) else { continue };
        let _ = table.count();
        for key in [
            Int64Encoding::encode(0).to_vec(),
            Int64Encoding::encode(-1).to_vec(),
            b"a".to_vec(),
            Vec::new(),
        ] {
            let _ = table.get(&key);
            let _ = table.blob_reader(&key);
            let _ = table.count_range(Bound::Included(&key[..]), Bound::Unbounded);
        }
        for order in [Order::Ascending, Order::Descending] {
            if let Ok(mut cursor) = table.scan(order) {
                // The sibling-chain guard is what keeps this finite; the cap is a
                // backstop so a failure shows up as a hang in one target rather than
                // the whole run.
                for _ in 0..4096 {
                    match cursor.advance() {
                        Ok(true) => {
                            if let Some(entry) = cursor.current() {
                                let _ = entry.key().len() + entry.value().len();
                            }
                        }
                        _ => break,
                    }
                }
            }
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
            let _ = index.get(b"a");
            if let Ok(mut cursor) = index.scan(Order::Ascending) {
                for _ in 0..4096 {
                    match cursor.advance() {
                        Ok(true) => {
                            let _ = cursor.value().map(|v| v.len());
                        }
                        _ => break,
                    }
                }
            }
        }
    }
});
