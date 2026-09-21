//! Build-path benchmarks: sorting, packing, compression and publishing.
//!
//! The interesting axis is the sort buffer. A build whose input fits the buffer never
//! touches a temporary file; one whose input does not spills and merges, and that
//! difference is what the external sort exists for, so both are measured.

mod common;

use std::sync::Arc;

use common::{environment, header, measure, scale, CountingAllocator, Rng};
use drydb::{AsciiEncoding, DatabaseBuilder, Int64Encoding, ZstdFilter};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct Options {
    page_size: usize,
    eytzinger: bool,
    zstd: bool,
    sort_buffer: usize,
    secondary: bool,
}

fn build(rows: i64, value_len: usize, options: &Options) -> (u64, u64) {
    let mut builder = DatabaseBuilder::new()
        .page_size(options.page_size)
        .unwrap()
        .eytzinger_digests(options.eytzinger)
        .sort_buffer(options.sort_buffer);
    if options.zstd {
        builder = builder.page_filter(Arc::new(ZstdFilter::default()));
    }
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    if options.secondary {
        builder
            .add_secondary_index(
                table,
                "by_head",
                false,
                Arc::new(AsciiEncoding),
                Box::new(|_k, v| Ok(v[..v.len().min(9)].to_vec())),
            )
            .unwrap();
    }

    // Append in a shuffled order so the sorter actually has work to do.
    let mut rng = Rng::new(0xB111_D000);
    let mut order: Vec<i64> = (0..rows).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    for i in order {
        let value: Vec<u8> = (0..value_len)
            .map(|n| (i as u8).wrapping_add(n as u8))
            .collect();
        builder
            .append(table, &Int64Encoding::encode(i), &value)
            .unwrap();
    }

    let bytes = builder.build_to_vec().unwrap();
    (rows as u64, bytes.len() as u64)
}

fn main() {
    environment();

    let rows = scale("DRYDB_BENCH_BUILD_ROWS", 200_000) as i64;
    let value_len = scale("DRYDB_BENCH_VALUE_LEN", 32) as usize;

    header(&format!(
        "building {rows} rows with {value_len} byte values"
    ));
    let cases: Vec<(&str, Options)> = vec![
        (
            "page 4096, sort fits in memory",
            Options {
                page_size: 4096,
                eytzinger: false,
                zstd: false,
                sort_buffer: 256 << 20,
                secondary: false,
            },
        ),
        (
            "page 4096, 1 MiB sort buffer (spills)",
            Options {
                page_size: 4096,
                eytzinger: false,
                zstd: false,
                sort_buffer: 1 << 20,
                secondary: false,
            },
        ),
        (
            "page 4096, 256 KiB sort buffer (spills more)",
            Options {
                page_size: 4096,
                eytzinger: false,
                zstd: false,
                sort_buffer: 256 << 10,
                secondary: false,
            },
        ),
        (
            "page 4096, eytzinger digests",
            Options {
                page_size: 4096,
                eytzinger: true,
                zstd: false,
                sort_buffer: 256 << 20,
                secondary: false,
            },
        ),
        (
            "page 4096, one secondary index",
            Options {
                page_size: 4096,
                eytzinger: false,
                zstd: false,
                sort_buffer: 256 << 20,
                secondary: true,
            },
        ),
        (
            "page 4096, zstd filtered",
            Options {
                page_size: 4096,
                eytzinger: false,
                zstd: true,
                sort_buffer: 256 << 20,
                secondary: false,
            },
        ),
        (
            "page 65536, classic metadata",
            Options {
                page_size: 65536,
                eytzinger: false,
                zstd: false,
                sort_buffer: 256 << 20,
                secondary: false,
            },
        ),
    ];

    for (label, options) in cases {
        let mut file_size = 0u64;
        let mut report = measure(label, || {
            let (written, size) = build(rows, value_len, &options);
            file_size = size;
            written
        });
        report.notes.push(format!(
            "{:.1} MiB out, {:.2} bytes/row",
            file_size as f64 / (1024.0 * 1024.0),
            file_size as f64 / rows as f64
        ));
        report.print();
    }
}
