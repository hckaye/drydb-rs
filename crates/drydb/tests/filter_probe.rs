//! That a budget too small for a page filter refuses before the filter allocates.
//!
//! In its own test binary because the evidence is process-wide: the zstd filter measures
//! its decompression context once per process, so the proof that nothing measured it is
//! only available in a process where nothing else could have.

#![cfg(feature = "zstd")]

use std::sync::Arc;

use drydb::{DatabaseBuilder, ErrorKind, Int64Encoding, OpenOptions, PageFilter, ZstdFilter};

#[test]
fn opening_a_compressed_file_on_a_tiny_budget_never_builds_a_zstd_context() {
    let mut builder = DatabaseBuilder::new()
        .page_size(256)
        .unwrap()
        .page_filter(Arc::new(ZstdFilter::default()));
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"v")
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    // The builder compresses, which needs no decompression context.
    assert_ne!(
        ZstdFilter::default().probe_bytes(),
        0,
        "writing a file must not measure the decompression context"
    );

    for _ in 0..20 {
        let err = OpenOptions::new()
            .memory_budget(32_768)
            .cache_shards(1)
            .open_bytes(bytes.clone())
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidArgument, "{err}");
    }

    // `probe_bytes` is zero once the figure has been measured, so a non-zero value here
    // says no context was ever built: twenty refused opens allocated nothing for zstd.
    assert_ne!(
        ZstdFilter::default().probe_bytes(),
        0,
        "a refused open must not measure the decompression context"
    );

    // With room for it, the same file opens, and that is when the measurement happens.
    let db = OpenOptions::new()
        .memory_budget(16 << 20)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    assert_eq!(ZstdFilter::default().probe_bytes(), 0);
    let table = db.table("t").unwrap();
    assert_eq!(
        table.get(&Int64Encoding::encode(0)).unwrap().as_deref(),
        Some(&b"v"[..])
    );
}
