//! Arbitrary bytes as one B+Tree page.
//!
//! Narrower than the whole-file target: it reaches the metadata decoders directly,
//! without needing a header and a page directory that happen to line up.

#![no_main]

use drydb::format::node::{NodeKind, PageView};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    // Make the length field agree with the buffer, so the input reaches the parts of
    // the parser that a length mismatch would otherwise short-circuit.
    let mut page = data.to_vec();
    let len = page.len() as i32;
    page[0..4].copy_from_slice(&len.to_le_bytes());

    let Ok(view) = PageView::parse(&page) else {
        return;
    };
    let _ = view.validate();

    for i in 0..view.entry_count().min(4096) {
        match view.kind() {
            NodeKind::Leaf => {
                if let Ok(entry) = view.leaf_entry(i) {
                    let _ = view.leaf_key(&entry);
                    if let drydb::format::node::LeafValue::Inline { offset, len } = entry.value {
                        let _ = view.inline_value(offset, len);
                    }
                }
            }
            NodeKind::Internal => {
                if let Ok(entry) = view.internal_entry(i) {
                    let _ = view.internal_key(&entry);
                }
            }
        }
        let _ = view.digest_at(i);
    }
    let _ = view.lower_bound_rank(u64::from_le_bytes(
        data[..8.min(data.len())]
            .iter()
            .copied()
            .chain(std::iter::repeat(0))
            .take(8)
            .collect::<Vec<u8>>()
            .try_into()
            .expect("eight bytes"),
    ));
});
